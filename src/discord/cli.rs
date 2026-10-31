//! Discord CLI host input and sync-then-read workflow, and the resident
//! process (`live`) and the say requests it plays (`say`).
use super::intake::{self, Intake};
use super::live::{self, Live, StateDir};
use super::{operations::*, render};
use crate::out::Out;
use anyhow::{anyhow, bail, Context, Result};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};
use std::num::NonZeroU64;
use std::{fs, io::Read, path::PathBuf};

/// What `discord live --voice-channel` answers in a build without voice.
#[cfg(not(feature = "discord-voice"))]
const NO_VOICE: &str =
    "voice is not built into this discord binary; build it with the discord-voice feature";

/// What `discord live --hear` answers in a build without hearing.
#[cfg(not(feature = "discord-hearing"))]
const NO_HEARING: &str = "hearing is not built into this discord binary; build it with the \
     discord-hearing feature";

/// What `discord live` answers when given nothing to run: only what this
/// build can run.
#[cfg(feature = "discord-voice")]
const NOTHING_TO_RUN: &str = "nothing to run: give --guild and --voice-channel to join a voice \
     channel, or --intake-channel or --intake-dms for intake";
#[cfg(not(feature = "discord-voice"))]
const NOTHING_TO_RUN: &str = "nothing to run: give --intake-channel or --intake-dms for intake";

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "discord",
    about = "Post to and ingest Discord channels into TribleSpace, and run the bot live on \
             Discord's gateway (message intake, and voice with the discord-voice feature)"
)]
pub struct Cli {
    /// Path to the pile file to use: send, read and channels need it, and so
    /// does live's intake.
    #[arg(long, env = "PILE", global = true)]
    pile: Option<PathBuf>,
    /// Existing durable signing-key file. Reads and writes never create it;
    /// initialize explicitly with trible pile signing-key init.
    #[arg(long, env = "TRIBLESPACE_KEY", global = true)]
    key: Option<PathBuf>,
    /// Explicit legacy token override. Without it, resolve the pile's exact
    /// Secrets reference. Use @path or @- to avoid exposing an override in argv.
    #[arg(long, env = "DISCORD_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Collection(CommandMode),
    /// Run the bot live on Discord's gateway until stopped.
    ///
    /// One gateway session stores what the intake channels (and DMs)
    /// receive in the discord collection as it arrives and, with
    /// --voice-channel, joins that voice channel and plays the say requests
    /// `discord say` publishes for it into the discord collection, recording
    /// each as played or failed there. Runs in the foreground; run it under
    /// a user unit so it outlives the shell.
    Live(LiveArgs),
    /// Ask `discord live` to speak text in a voice channel: publishes a say
    /// request into the discord collection (needs the pile), which the
    /// resident process joined to that channel plays after every earlier
    /// request and records as played (its audio played to the end) or
    /// failed. A request more than two minutes old when the resident process
    /// starts is failed as stale rather than played.
    Say {
        /// Voice channel to speak in (global Discord snowflake).
        #[arg(long, env = "DISCORD_VOICE_CHANNEL", value_name = "CHANNEL_ID")]
        voice_channel: NonZeroU64,
        /// Guild (server) of the voice channel, recorded on the request.
        #[arg(long, env = "DISCORD_GUILD", value_name = "GUILD_ID")]
        guild: Option<NonZeroU64>,
        /// Who asks, recorded on the request.
        #[arg(long, env = "PERSONA")]
        persona: Option<String>,
        #[command(flatten)]
        options: crate::voice::options::SpeechOptions,
        /// What to say, literally.
        text: String,
    },
    /// Hear a recorded clip the way `live --hear` hears its voice channel,
    /// without Discord: a 16 kHz mono 16-bit WAV is cut into utterances, and
    /// each is transcribed and printed with its timing and words. The model
    /// is the pile's (`--pile`, `TRIBLESPACE_COLLECTION_MODELS`). Nothing is
    /// stored.
    #[cfg(feature = "discord-hearing")]
    Hear {
        /// The clip: a 16 kHz mono 16-bit WAV, or an ogg/opus recording
        /// (`.ogg`, `.oga`, `.opus`) such as a Discord voice message.
        wav: PathBuf,
    },
    /// Load the voice the way `live --voice-channel` does, without Discord,
    /// speak TEXT to nowhere, and exit. Run it through the staged binary's
    /// launcher before a switch: the live start then finds the reference
    /// codes and the codec's kernels already cached, and its first line
    /// comes in seconds instead of after their 22 s and 15 s. Needs the pile
    /// whose model collection holds the voice (`--pile` or `PILE`) and a
    /// build with the discord-voice feature.
    Warm {
        /// What to speak into the void; a short line is enough.
        #[arg(default_value = "The voice is warm.")]
        text: String,
        /// First load the hearing model the way `live --hear` does and have
        /// it transcribe the voice's reference clip once, so that a start
        /// with --hear finds its kernels cached too (its first load in a new
        /// directory otherwise compiles them for 17 s, and the voice's first
        /// line waits behind that). Needs a build with the discord-hearing
        /// and voice-breeze features.
        #[arg(long)]
        hear: bool,
    },
}

#[derive(Args)]
struct StateArg {
    /// State directory of `live`: intake keeps where each channel's intake
    /// begins in `intake/`, with what could not be stored. Default:
    /// $XDG_DATA_HOME/faculties/discord, or ~/.local/share/faculties/discord
    /// without XDG_DATA_HOME.
    #[arg(long, env = "DISCORD_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,
}

#[derive(Args)]
struct LiveArgs {
    #[command(flatten)]
    state: StateArg,
    /// Guild (server) of the voice channel (global Discord snowflake).
    #[arg(long, value_name = "GUILD_ID", requires = "voice_channel")]
    guild: Option<NonZeroU64>,
    /// Voice channel to join (global Discord snowflake); needs --guild, and a
    /// build with the discord-voice feature.
    #[arg(long, value_name = "CHANNEL_ID", requires = "guild")]
    voice_channel: Option<NonZeroU64>,
    /// Text channel to post --greeting in once the voice connection is up
    /// (global Discord snowflake), so that people know to join.
    #[arg(long, value_name = "CHANNEL_ID", requires = "voice_channel")]
    announce: Option<NonZeroU64>,
    /// What to post in the --announce channel.
    #[arg(long, default_value = "I'm in voice.")]
    greeting: String,
    /// Store every message of this text channel (global Discord snowflake)
    /// in the discord collection, where orient finds it (repeatable), from
    /// the first time the process runs with it on; its earlier history stays
    /// out (`discord read` brings it in, and it is never news). Needs the
    /// Message Content intent enabled for the bot: without it, Discord
    /// refuses intake, and a voice connection goes on with voice only.
    /// Intake is off unless this or --intake-dms is given.
    #[arg(long = "intake-channel", value_name = "CHANNEL_ID")]
    intake_channels: Vec<NonZeroU64>,
    /// Also store direct messages to the bot, each DM channel from the first
    /// message the process hears in it.
    #[arg(long)]
    intake_dms: bool,
    /// Hear everybody in the voice channel but the bot, with the Voxtral
    /// model of the pile's model collection (`TRIBLESPACE_COLLECTION_MODELS`):
    /// each utterance is transcribed and stored with its audio as a message
    /// in the voice channel, under the user who said it, where orient finds
    /// it. Without it nobody is heard. Needs a pile, and a build with the
    /// discord-hearing feature.
    #[arg(long, requires = "voice_channel")]
    hear: bool,
}

#[derive(Subcommand)]
enum CommandMode {
    /// Inspect or bind the pile-backed bot credential (never prints plaintext).
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Post a message and persist the returned Discord observation.
    Send {
        /// Channel id (global Discord snowflake).
        channel_id: String,
        /// Message body. Use @path for file input or @- for stdin.
        text: String,
    },
    /// Pull one complete forward interval plus a bounded recent window.
    Read {
        /// Channel id (global Discord snowflake). If omitted, poll every
        /// visible text-capable channel.
        channel_id: Option<String>,
        /// Only display messages at or after this RFC3339 timestamp.
        #[arg(long)]
        since: Option<String>,
        /// Maximum messages to display after ingestion (0 = no limit).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Display newest first.
        #[arg(long)]
        descending: bool,
        /// Maximum messages per forward page (Discord caps this at 100).
        #[arg(long, default_value_t = 100)]
        fetch_limit: u32,
        /// Recent messages re-fetched to observe bounded-window edits.
        #[arg(long, default_value_t = 50)]
        reconcile_limit: u32,
    },
    /// List guilds and channels visible to the bot.
    Channels {
        #[command(subcommand)]
        command: ChannelsCommand,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Show authentication revision and exact Secrets version ids only.
    Status,
    /// Bind an existing exact Secrets version; reconcile observed auth forks.
    Set {
        /// Exact immutable version id, not a credential or a secret name.
        #[arg(value_parser = parse_secret_id)]
        secret_version: triblespace::prelude::Id,
    },
}

fn parse_secret_id(raw: &str) -> std::result::Result<triblespace::prelude::Id, String> {
    triblespace::prelude::Id::from_hex(raw)
        .ok_or_else(|| "expected an exact 32-character Secrets version id".to_owned())
}

#[derive(Subcommand)]
enum ChannelsCommand {
    /// Print guilds and channels.
    List {
        /// Only show channels in this guild (global Discord snowflake).
        #[arg(long)]
        guild: Option<String>,
    },
    /// Address a channel to the people it is for: what is written there then
    /// wakes only them (orient's news), where a channel addressed to nobody
    /// in particular wakes everybody. The people given are the whole set and
    /// replace whoever the channel was addressed to; --nobody addresses it
    /// to nobody in particular again. Recorded in the discord collection
    /// (needs the pile), with no call to Discord.
    #[command(group(clap::ArgGroup::new("whom").required(true).args(["people", "nobody"])))]
    Address {
        /// Channel id (global Discord snowflake).
        channel_id: String,
        /// Who the channel is for: Relations labels, aliases or ids.
        people: Vec<String>,
        /// Address the channel to nobody in particular.
        #[arg(long)]
        nobody: bool,
    },
    /// Print every addressed channel and who it is addressed to.
    Addressed,
}

pub fn execute(mut cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let command = cli
        .command
        .take()
        .ok_or_else(|| anyhow!("no Discord command"))?;
    let command = match command {
        Command::Collection(command) => command,
        Command::Live(args) => return run_live(&cli, args),
        Command::Say {
            voice_channel,
            guild,
            persona,
            options,
            text,
        } => {
            return say(
                &cli,
                guild,
                voice_channel,
                persona.as_deref(),
                &text,
                &options,
                out,
            )
        }
        Command::Warm { text, hear } => return warm(cli.pile.as_deref(), &text, hear, out),
        #[cfg(feature = "discord-hearing")]
        Command::Hear { wav } => {
            let pile = cli
                .pile
                .clone()
                .ok_or_else(|| anyhow!("discord hear needs a pile (--pile or PILE)"))?;
            return hear(&pile, &wav, out);
        }
    };
    let token = if matches!(&command, CommandMode::Auth { .. }) {
        None
    } else {
        cli.token
            .as_deref()
            .map(|raw| load_value_or_file_trimmed(raw, "Discord token"))
            .transpose()?
    };
    let pile = cli
        .pile
        .ok_or_else(|| anyhow!("missing pile; pass --pile or set PILE"))?;
    let operations = Discord::new(pile, cli.key);
    let operations = match token {
        Some(token) => operations.with_token(token),
        None => operations,
    };
    match command {
        CommandMode::Auth { command } => match command {
            AuthCommand::Status => render::auth(&operations.auth_status()?, out),
            AuthCommand::Set { secret_version } => {
                let revision = operations.auth_set(secret_version)?;
                out.line(format!(
                    "Discord auth revision {revision:X}; Secrets version {secret_version:X}"
                ))
            }
        },
        CommandMode::Send { channel_id, text } => {
            let text = crate::text_arg(&text, "message text")?;
            render::sent(&operations.send(&channel_id, &text)?, out)
        }
        CommandMode::Read {
            channel_id,
            since,
            limit,
            descending,
            fetch_limit,
            reconcile_limit,
        } => {
            let read = ReadOptions {
                channel_id: channel_id.clone(),
                since,
                limit,
                descending,
            };
            read.validate()?;
            let report = operations.pull(PullOptions {
                channel_id,
                fetch_limit: fetch_limit.clamp(1, 100),
                reconcile_limit: reconcile_limit.clamp(1, 100),
            })?;
            render::pull_header(&report, out)?;
            for channel in &report.channels {
                match &channel.result {
                    Ok(receipt) => render::channel_receipt(receipt, out)?,
                    Err(error) => {
                        eprintln!("  ! {} ({}): {error}", channel.channel_id, channel.name)
                    }
                }
            }
            if report.all_visible && report.channels.is_empty() {
                return Ok(());
            }
            render::history(&operations.read(read)?, out)
        }
        CommandMode::Channels {
            command: ChannelsCommand::List { guild },
        } => render::channels(&operations.channels_list(guild.as_deref())?, out),
        CommandMode::Channels {
            command: ChannelsCommand::Address {
                channel_id, people, ..
            },
        } => render::addressed(&[operations.address_channel(&channel_id, &people)?], out),
        CommandMode::Channels {
            command: ChannelsCommand::Addressed,
        } => render::addressed(&operations.channel_addressing()?, out),
    }
}
pub fn run() -> Result<()> {
    let mut cli = Cli::parse();
    match cli.command.take() {
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
        // The resident process runs in the foreground and writes its own log.
        Some(Command::Live(args)) => run_live(&cli, args),
        Some(command) => {
            // The collection commands cannot run without a pile: that is a
            // usage error, reported before anything else is looked at.
            if matches!(command, Command::Collection(_)) && cli.pile.is_none() {
                Cli::command()
                    .error(
                        ErrorKind::MissingRequiredArgument,
                        "the following required arguments were not provided:\n  --pile <PILE>",
                    )
                    .exit();
            }
            cli.command = Some(command);
            crate::cli::with_output("discord", |out| execute(cli, out))
        }
    }
}

/// `discord live`: the gateway session with intake, a voice connection, or
/// both.
fn run_live(cli: &Cli, args: LiveArgs) -> Result<()> {
    // The voice driver reports its own failures through tracing; RUST_LOG
    // (e.g. songbird=debug) makes them visible in the process's log.
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init();
    }
    let LiveArgs {
        state,
        guild,
        voice_channel,
        announce,
        greeting,
        intake_channels,
        intake_dms,
        hear,
    } = args;
    // clap asks for --guild and --voice-channel together.
    let voice = guild.zip(voice_channel);
    #[cfg(not(feature = "discord-voice"))]
    {
        if voice.is_some() {
            bail!(NO_VOICE);
        }
        let _ = (announce, greeting);
    }
    #[cfg(not(feature = "discord-hearing"))]
    if hear {
        bail!(NO_HEARING);
    }
    // What is heard is stored through intake, which hearing therefore runs.
    let wants_intake = !intake_channels.is_empty() || intake_dms || hear;
    if voice.is_none() && !wants_intake {
        bail!(NOTHING_TO_RUN);
    }
    let state = StateDir::resolve(state.state_dir)?;
    // What was asked for or begun before now is judged by
    // `crate::discord::say_turn`, never blindly played.
    #[cfg(feature = "discord-voice")]
    let speaker_started = crate::discord::moment(crate::clock::now()?);
    // The voice plays the say requests of the discord collection and records
    // their outcomes there: it needs the pile, writable.
    #[cfg(feature = "discord-voice")]
    if voice.is_some() && cli.pile.is_none() {
        bail!("the voice plays say requests from the pile; pass --pile or set PILE");
    }
    // One store for the life of the process, shared by intake and the voice,
    // rather than one pile open per message.
    let discord = cli
        .pile
        .clone()
        .map(|pile| Discord::with_storage(crate::storage::Storage::shared(pile, cli.key.clone())));
    #[cfg(feature = "discord-voice")]
    if let (Some(_), Some(discord)) = (voice, &discord) {
        discord.preflight_write().context(
            "the voice records say request outcomes in the discord collection, which cannot be \
             written",
        )?;
    }
    let token = require_token(cli)?;
    // Intake that cannot write is off, loudly, and a voice connection runs
    // either way; without one, nothing is left to run.
    let off = |reason: String| -> Result<Option<Intake>> {
        if voice.is_none() {
            bail!("intake cannot run: {reason}");
        }
        eprintln!("[discord] intake is off: {reason}");
        Ok(None)
    };
    let intake = match (wants_intake, discord.clone()) {
        (false, _) => None,
        (true, None) => off("it needs a pile (--pile or PILE)".to_owned())?,
        (true, Some(discord)) => {
            let intake = Intake::new(
                discord,
                Box::new(Rest::new(token.clone())),
                intake_channels,
                intake_dms,
                state.intake(),
                intake::floor_at(std::time::SystemTime::now()),
            );
            match intake.preflight() {
                Ok(()) => Some(intake),
                Err(error) => off(format!(
                    "the discord collection cannot be written: {error:#}"
                ))?,
            }
        }
    };
    // The model is the pile's: hearing reads Voxtral out of its model
    // collection, so it needs the pile as much as intake does.
    #[cfg(feature = "discord-hearing")]
    let hearing = match (hear, cli.pile.clone()) {
        (false, _) => None,
        (true, None) => bail!("hearing needs a pile (--pile or PILE)"),
        (true, Some(pile)) => Some(super::hearing::Config {
            pile,
            unstored: state.intake().join(intake::UNSTORED_SPEECH),
            // Wired to intake by `live::run`.
            voice_messages: None,
        }),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start the async runtime")?;
    let outcome = runtime.block_on(live::run(Live {
        token,
        intake,
        #[cfg(feature = "discord-voice")]
        voice: voice.map(|(guild, channel)| super::voice::Config {
            guild,
            channel,
            announce,
            greeting,
            #[cfg(feature = "discord-hearing")]
            hearing,
            requests: super::say::SayQueue::new(
                discord
                    .clone()
                    .expect("the voice was refused without a pile"),
                channel,
                speaker_started,
            ),
            pile: cli.pile.clone(),
        }),
    }));
    // Whatever is still running (a download, say) does not hold the process
    // up past its own bounded shutdown.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    outcome
}

/// `discord hear`: a clip heard as the voice channel is.
#[cfg(feature = "discord-hearing")]
fn hear(pile: &std::path::Path, wav: &std::path::Path, out: &mut Out<'_>) -> Result<()> {
    let ogg = wav
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "ogg" | "oga" | "opus"));
    if ogg {
        let bytes = std::fs::read(wav).with_context(|| format!("read {}", wav.display()))?;
        let clip = super::voice_message::decode_ogg_opus(&bytes)
            .with_context(|| format!("decode {}", wav.display()))?;
        return super::hearing::hear_clip(pile, &clip, out);
    }
    // Panics on anything but a 16-bit mono WAV.
    let (clip, rate) = mary::models::f5::wav::read_pcm16_mono(wav);
    if rate as usize != super::hearing::RATE {
        bail!(
            "{} is at {rate} Hz; hearing takes {} Hz",
            wav.display(),
            super::hearing::RATE
        );
    }
    super::hearing::hear_clip(pile, &clip, out)
}

/// `discord say`: publish a say request for the voice channel into the
/// discord collection. Any build can ask; a `discord live` built with voice
/// and joined to the channel plays it.
fn say(
    cli: &Cli,
    guild: Option<NonZeroU64>,
    channel: NonZeroU64,
    persona: Option<&str>,
    text: &str,
    options: &crate::voice::options::SpeechOptions,
    out: &mut Out<'_>,
) -> Result<()> {
    options.validate()?;
    let pile = cli
        .pile
        .clone()
        .ok_or_else(|| anyhow!("discord say publishes into the pile; pass --pile or set PILE"))?;
    let guild = guild.map(|guild| guild.to_string());
    let request = Discord::new(pile, cli.key.clone()).request_speech(
        guild.as_deref(),
        &channel.to_string(),
        text,
        options,
        persona,
    )?;
    out.line(format!(
        "say request {request:X} for voice channel {channel}"
    ))
}

/// `discord warm`: with `hear` the hearing model loaded and the voice's
/// reference clip transcribed, then the voice loaded and one line spoken,
/// with no Discord. Loading and running are what fill the caches a start
/// reads; everything is timed so a run of this after a switch candidate is
/// staged shows what the live start will pay.
///
/// Hearing goes first because the two models share one device and burn's
/// device settings take their default float type from whichever backend
/// touches the device first: after the voice (an f32 lane) has loaded, the
/// hearing model's activations come out F32 and its NVFP4 projections
/// refuse them. This is the order a live start produces too, with the
/// hearing thread's load ahead of the voice's.
#[cfg(feature = "discord-voice")]
fn warm(pile: Option<&std::path::Path>, text: &str, hear: bool, out: &mut Out<'_>) -> Result<()> {
    use crate::voice::synthesis::{ModelSources, Synthesizer};
    let synthesizer = Synthesizer::new(ModelSources::from_environment(pile));
    if hear {
        warm_hearing(pile, &synthesizer.sources, out)?;
    }
    let started = std::time::Instant::now();
    synthesizer.prime()?;
    out.line(format!(
        "voice loaded in {:.1} s",
        started.elapsed().as_secs_f64()
    ))?;
    let started = std::time::Instant::now();
    let clip = synthesizer.synthesize(text)?;
    out.line(format!(
        "spoke {:.1} s of audio in {:.1} s",
        clip.duration_seconds(),
        started.elapsed().as_secs_f64()
    ))
}

#[cfg(not(feature = "discord-voice"))]
fn warm(_: Option<&std::path::Path>, _: &str, _: bool, _: &mut Out<'_>) -> Result<()> {
    bail!(NO_VOICE)
}

/// `discord warm --hear`: the hearing model loaded as `live --hear` loads
/// it, and the voice's reference clip, read as the voice reads it (24 kHz),
/// heard once through the same transcriber the voice channel's utterances
/// go through.
#[cfg(all(feature = "discord-hearing", feature = "voice-breeze"))]
fn warm_hearing(
    pile: Option<&std::path::Path>,
    sources: &crate::voice::synthesis::ModelSources,
    out: &mut Out<'_>,
) -> Result<()> {
    use crate::voice::synthesis::{BreezeSources, ModelSources};
    use mary::models::breeze::reference::{read_wav, SAMPLE_RATE};
    let pile = pile.ok_or_else(|| anyhow!("hearing needs a pile (--pile or PILE)"))?;
    let reference_wav = match sources {
        ModelSources::Breeze(BreezeSources { reference_wav, .. })
        | ModelSources::Qwen3Tts { reference_wav, .. } => reference_wav,
        ModelSources::Invalid(error) => bail!("invalid speech configuration: {error}"),
    };
    let clip = read_wav(reference_wav)?;
    super::hearing::warm(pile, &clip, SAMPLE_RATE, out)
}

#[cfg(not(all(feature = "discord-hearing", feature = "voice-breeze")))]
fn warm_hearing(
    _: Option<&std::path::Path>,
    _: &crate::voice::synthesis::ModelSources,
    _: &mut Out<'_>,
) -> Result<()> {
    bail!("warm --hear needs a build with the discord-hearing and voice-breeze features")
}

fn require_token(cli: &Cli) -> Result<String> {
    if cli.token.is_none() {
        let pile = cli.pile.clone().ok_or_else(|| anyhow!(
            "pile-backed Discord authentication needs --pile or PILE; existing callers may explicitly supply --token"
        ))?;
        return Discord::new(pile, cli.key.clone())
            .token()
            .map(|token| token.to_string());
    }
    let token = cli
        .token
        .as_deref()
        .ok_or_else(|| anyhow!("missing Discord token; pass --token, DISCORD_TOKEN, @path, or @-"))
        .and_then(|raw| load_value_or_file_trimmed(raw, "Discord token"))?;
    if token.is_empty() {
        bail!("empty Discord token");
    }
    Ok(token)
}

fn load_value_or_file(raw: &str, label: &str) -> Result<String> {
    if let Some(path) = raw.strip_prefix('@') {
        if path == "-" {
            let mut value = String::new();
            std::io::stdin()
                .read_to_string(&mut value)
                .with_context(|| format!("read {label} from stdin"))?;
            return Ok(value);
        }
        return fs::read_to_string(path).with_context(|| format!("read {label} from {path}"));
    }
    Ok(raw.to_owned())
}

fn load_value_or_file_trimmed(raw: &str, label: &str) -> Result<String> {
    Ok(load_value_or_file(raw, label)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn say_parses_delivery_without_changing_message_send() {
        let cli = Cli::try_parse_from([
            "discord",
            "say",
            "--voice-channel",
            "2",
            "(laugh) Hello.",
            "--direction",
            "Warm, professional",
            "--cfg-scale",
            "2.5",
        ])
        .unwrap();
        let Some(Command::Say { text, options, .. }) = cli.command else {
            unreachable!()
        };
        assert_eq!(text, "(laugh) Hello.");
        assert_eq!(options.direction.as_deref(), Some("Warm, professional"));
        assert_eq!(options.cfg_scale, Some(2.5));
        assert!(Cli::try_parse_from([
            "discord",
            "say",
            "--voice-channel",
            "2",
            "hello",
            "--direction",
            "warm",
            "--clear-direction"
        ])
        .is_err());
        assert!(
            Cli::try_parse_from(["discord", "send", "1", "hello", "--direction", "warm"]).is_err()
        );
    }

    /// `discord say` publishes a request into the discord collection, with
    /// who asked, where and how, in any build and without a token or a
    /// model; the old folder queue is not touched.
    #[test]
    fn say_cli_publishes_a_request_without_auth_or_synthesis() {
        crate::test_support::clear_ambient_environment();
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("say.pile");
        let key = directory.path().join("say.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let cli = Cli::try_parse_from([
            "discord",
            "--pile",
            pile.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "say",
            "--voice-channel",
            "200000000000000201",
            "--guild",
            "200000000000000300",
            "--persona",
            "ada",
            "(sigh) Hello.",
            "--direction",
            "Warm",
            "--cfg-scale",
            "2.5",
        ])
        .unwrap();
        let mut printed = Vec::new();
        execute(
            cli,
            &mut Out::new(&mut |part| {
                if let crate::out::Part::Text { text } = part {
                    printed.push(text);
                }
                Ok(())
            }),
        )
        .unwrap();
        assert!(printed[0].starts_with("say request "), "{printed:?}");
        let pending = Discord::new(pile, Some(key))
            .unsettled_speech("200000000000000201")
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "(sigh) Hello.");
        assert_eq!(pending[0].options.direction.as_deref(), Some("Warm"));
        assert_eq!(pending[0].options.cfg_scale, Some(2.5));
        assert!(!directory.path().join("say").exists());
    }

    #[test]
    fn live_and_say_parse_beside_the_collection_commands() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "discord",
            "--token",
            "t",
            "live",
            "--guild",
            "1",
            "--voice-channel",
            "2",
            "--intake-channel",
            "3",
            "--intake-channel",
            "4",
            "--intake-dms",
        ])
        .unwrap();
        let Some(Command::Live(live)) = cli.command else {
            panic!("live parses as live");
        };
        assert_eq!(
            live.guild
                .zip(live.voice_channel)
                .map(|(g, c)| (g.get(), c.get())),
            Some((1, 2))
        );
        assert_eq!(live.intake_channels.len(), 2);
        assert!(live.intake_dms);
        // A voice channel is nothing without its guild.
        assert!(Cli::try_parse_from(["discord", "live", "--voice-channel", "2"]).is_err());
        let cli = Cli::try_parse_from(["discord", "warm"]).unwrap();
        let Some(Command::Warm { text, hear }) = cli.command else {
            panic!("warm parses as warm");
        };
        assert_eq!((text.as_str(), hear), ("The voice is warm.", false));
        // Hearing is a switch beside the text, in either order.
        for args in [
            ["discord", "warm", "--hear", "hello"],
            ["discord", "warm", "hello", "--hear"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let Some(Command::Warm { text, hear }) = cli.command else {
                panic!("warm parses as warm");
            };
            assert_eq!((text.as_str(), hear), ("hello", true));
        }
        let cli = Cli::try_parse_from(["discord", "say", "--voice-channel", "2", "hello"]).unwrap();
        let Some(Command::Say {
            voice_channel,
            guild,
            text,
            options,
            ..
        }) = cli.command
        else {
            panic!("say parses as say");
        };
        assert!(options.is_default());
        assert_eq!(
            (voice_channel.get(), guild, text),
            (2, None, "hello".to_owned())
        );
        // The folder queue is gone: say has no state directory.
        assert!(Cli::try_parse_from([
            "discord",
            "say",
            "--voice-channel",
            "2",
            "--state-dir",
            "/s",
            "x"
        ])
        .is_err());
        let cli = Cli::try_parse_from(["discord", "channels", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Collection(CommandMode::Channels { .. }))
        ));
        // The shared flags go before or after the subcommand.
        for args in [
            [
                "discord",
                "--token",
                "t",
                "--pile",
                "/p",
                "live",
                "--intake-dms",
            ],
            [
                "discord",
                "live",
                "--token",
                "t",
                "--pile",
                "/p",
                "--intake-dms",
            ],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(cli.token.as_deref(), Some("t"));
            assert_eq!(cli.pile, Some(PathBuf::from("/p")));
        }
        let cli = Cli::try_parse_from(["discord", "read", "--pile", "/p", "--token", "t"]).unwrap();
        assert_eq!(cli.pile, Some(PathBuf::from("/p")));
    }

    /// Hearing is a switch, and nobody to name: whoever is in the voice
    /// channel is heard, with the model the pile's model collection holds.
    /// There is no list of users to hear, no model path and no tokenizer.
    #[test]
    fn live_hears_the_voice_channel_with_the_piles_model() {
        let cli = Cli::try_parse_from([
            "discord",
            "live",
            "--guild",
            "1",
            "--voice-channel",
            "2",
            "--hear",
        ])
        .unwrap();
        let Some(Command::Live(live)) = cli.command else {
            panic!("live parses as live");
        };
        assert!(live.hear);
        assert!(Cli::try_parse_from(["discord", "live", "--hear"]).is_err());
        for gone in [
            &["--hear-user", "3"][..],
            &["--hear-tokenizer", "/m/tekken.json"],
            &["--hear-model", "/m/voxtral.pile"],
        ] {
            let args = ["discord", "live", "--guild", "1", "--voice-channel", "2"];
            assert!(Cli::try_parse_from(args.iter().chain(gone)).is_err());
        }
    }

    /// `discord live` names only what this build can run, and a build without
    /// voice says so for `--voice-channel` before it needs a token or touches
    /// the state directory; with voice, it refuses to join without the pile
    /// its say requests are in.
    #[test]
    fn live_and_say_answer_for_what_this_build_can_run() {
        let live = |args: &[&str]| {
            let mut cli = Cli::try_parse_from(args).unwrap();
            let Some(Command::Live(live)) = cli.command.take() else {
                panic!("live parses as live");
            };
            run_live(&cli, live).unwrap_err().to_string()
        };
        let nothing = live(&["discord", "live", "--state-dir", "/nonexistent"]);
        assert!(nothing.starts_with("nothing to run"), "{nothing}");
        assert!(nothing.contains("--intake-channel"), "{nothing}");
        assert_eq!(
            nothing.contains("--voice-channel"),
            cfg!(feature = "discord-voice"),
            "{nothing}"
        );
        #[cfg(not(feature = "discord-voice"))]
        {
            let voice = live(&[
                "discord",
                "live",
                "--state-dir",
                "/nonexistent",
                "--guild",
                "1",
                "--voice-channel",
                "2",
            ]);
            assert_eq!(voice, NO_VOICE);
        }
        #[cfg(feature = "discord-voice")]
        {
            crate::test_support::clear_ambient_environment();
            let voice = live(&[
                "discord",
                "live",
                "--state-dir",
                "/nonexistent",
                "--guild",
                "1",
                "--voice-channel",
                "2",
            ]);
            assert!(voice.contains("say requests from the pile"), "{voice}");
        }
    }

    #[test]
    fn permanent_cli_has_one_fixed_collection_identity() {
        let command = Cli::command();
        for forbidden in ["scope", "branch", "branch_id", "head", "repair"] {
            assert!(!command
                .get_arguments()
                .any(|argument| argument.get_id() == forbidden));
        }
    }
}
