//! Discord CLI host input and sync-then-read workflow, and the resident
//! process (`live`).
use super::intake::{self, Intake};
use super::live::{self, Live, StateDir};
use super::{operations::*, render};
use crate::out::Out;
use anyhow::{anyhow, bail, Context, Result};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};
use std::num::NonZeroU64;
use std::{fs, io::Read, path::PathBuf};

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "discord",
    about = "Post to and ingest Discord channels into TribleSpace, and run the bot live on \
             Discord's gateway (message intake)"
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
    /// Discord bot token. Use @path or @- to avoid exposing it in argv.
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
    /// receive in the discord collection as it arrives. Runs in the
    /// foreground; run it under a user unit so it outlives the shell.
    Live(LiveArgs),
}

#[derive(Args)]
struct StateArg {
    /// State directory of `live`: intake keeps where each channel's intake
    /// begins in `intake/`. Default: $XDG_DATA_HOME/faculties/discord, or
    /// ~/.local/share/faculties/discord without XDG_DATA_HOME.
    #[arg(long, env = "DISCORD_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,
}

#[derive(Args)]
struct LiveArgs {
    #[command(flatten)]
    state: StateArg,
    /// Store every message of this text channel (global Discord snowflake)
    /// in the discord collection, where orient finds it (repeatable), from
    /// the first time the process runs with it on; its earlier history stays
    /// out (`discord read` brings it in). Needs the Message Content intent
    /// enabled for the bot: without it, Discord refuses intake. Intake is off
    /// unless this or --intake-dms is given.
    #[arg(long = "intake-channel", value_name = "CHANNEL_ID")]
    intake_channels: Vec<NonZeroU64>,
    /// Also store direct messages to the bot, each DM channel from the first
    /// message the process hears in it.
    #[arg(long)]
    intake_dms: bool,
}

#[derive(Subcommand)]
enum CommandMode {
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
enum ChannelsCommand {
    /// Print guilds and channels.
    List {
        /// Only show channels in this guild (global Discord snowflake).
        #[arg(long)]
        guild: Option<String>,
    },
}

pub fn execute(mut cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let command = cli
        .command
        .take()
        .ok_or_else(|| anyhow!("no Discord command"))?;
    let command = match command {
        Command::Collection(command) => command,
        Command::Live(args) => return run_live(&cli, args),
    };
    let token = require_token(&cli)?;
    let pile = cli
        .pile
        .ok_or_else(|| anyhow!("missing pile; pass --pile or set PILE"))?;
    let operations = Discord::new(pile, cli.key).with_token(token);
    match command {
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

/// `discord live`: the gateway session with intake.
fn run_live(cli: &Cli, args: LiveArgs) -> Result<()> {
    let LiveArgs {
        state,
        intake_channels,
        intake_dms,
    } = args;
    if intake_channels.is_empty() && !intake_dms {
        bail!("nothing to run: give --intake-channel or --intake-dms for intake");
    }
    let state = StateDir::resolve(state.state_dir)?;
    let token = require_token(cli)?;
    let pile = cli
        .pile
        .clone()
        .ok_or_else(|| anyhow!("intake cannot run: it needs a pile (--pile or PILE)"))?;
    // One store for the life of the process, rather than one pile open per
    // message.
    let intake = Intake::new(
        Discord::with_storage(crate::storage::Storage::shared(pile, cli.key.clone())),
        Box::new(Rest::new(token.clone())),
        intake_channels,
        intake_dms,
        state.intake(),
        intake::floor_at(std::time::SystemTime::now()),
    );
    intake
        .preflight()
        .context("intake cannot run: the discord collection cannot be written")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start the async runtime")?;
    let outcome = runtime.block_on(live::run(Live {
        token,
        intake: Some(intake),
    }));
    // Whatever is still running (a download, say) does not hold the process
    // up past its own bounded shutdown.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    outcome
}

fn require_token(cli: &Cli) -> Result<String> {
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
    fn live_parses_beside_the_collection_commands() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "discord",
            "--token",
            "t",
            "live",
            "--state-dir",
            "/s",
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
        assert_eq!(live.state.state_dir, Some(PathBuf::from("/s")));
        assert_eq!(live.intake_channels.len(), 2);
        assert!(live.intake_dms);
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
