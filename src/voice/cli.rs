//! Explicit CLI UX: named-device speech remains a host operation; synthesis
//! can instead return audio to Drive without playing anything on the host.
use super::device::{Device, DEFAULT_DAEMON};
use super::options::SpeechOptions;
use super::synthesis::{ModelSources, Synthesizer};
use super::{Channel, Voice};
use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "voice",
    about = "Speech synthesis + privacy-aware output routing, on two channels."
)]
pub struct Cli {
    /// Path to the pile file
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable signing-key file. Reads and writes never create it;
    /// initialize explicitly with `trible pile signing-key init <pile>`.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    /// Reachy daemon base URL (the `shout` Reachy-speaker target).
    #[arg(long, env = "REACHY_DAEMON", default_value = DEFAULT_DAEMON)]
    daemon: String,
    /// Soma base URL. When explicitly configured and reachable, this is the
    /// preferred drained-stream target for public `shout`; it is never used by
    /// private `say`.
    #[arg(long, env = "SOMA_URL", global = true)]
    soma: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_help_does_not_open_bootstrap_and_selector_is_global() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("missing.pile");
        let key = directory.path().join("missing.key");
        let revision = format!("{:X}", triblespace::prelude::genid().id);
        let cli = Cli::try_parse_from([
            "voice",
            "--pile",
            pile.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "--workspace",
            &revision,
        ])
        .unwrap();
        let mut help = String::new();
        execute(
            cli,
            &mut crate::out::Out::new(&mut |part| {
                if let crate::out::Part::Text { text } = part {
                    help.push_str(&text);
                }
                Ok(())
            }),
        )
        .unwrap();
        assert!(help.contains("--workspace"));
        assert!(!pile.exists());
        assert!(!key.exists());
        let cli = Cli::try_parse_from([
            "voice",
            "--pile",
            "unused",
            "stream",
            "--workspace",
            &revision,
        ])
        .unwrap();
        assert_eq!(
            cli.workspace.workspace,
            Some(crate::cli_workspace::parse_revision(&revision).unwrap())
        );
    }

    #[test]
    fn workspace_route_set_is_isolated_and_model_commands_fail_before_io() {
        use crate::schemas::{config::config, workspace as attrs};
        use triblespace::core::collection::CollectionStoreExt;
        use triblespace::core::metadata;
        use triblespace::prelude::*;
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("node.pile");
        let key = directory.path().join("node.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let base = crate::storage::Storage::new(pile.clone(), Some(key.clone()));
        let revision = genid();
        let root = base
            .with_pile(|store, signer| {
                let policy = crate::collection_names::private_policy(signer.verifying_key());
                let root = store.collection("selected voice", policy.clone())?.handle();
                let config = store.collection(crate::config::COLLECTION_NAME, policy)?;
                let route = entity! {
                    config::anchor: crate::schemas::voice::COLLECTION_SCOPE_ID,
                    config::selects: root,
                };
                store.commit(
                    config,
                    signer,
                    entity! { &revision @
                        metadata::tag: attrs::WORKSPACE, config::anchor: genid().id,
                        attrs::enrolled: true, metadata::name: "Synthetic voice",
                        attrs::workspace_route*: route,
                    },
                )?;
                Ok(root)
            })
            .unwrap();
        let revision = format!("{:X}", revision.id);
        let args = [
            "voice",
            "--pile",
            pile.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "--workspace",
            &revision,
        ];
        let cli =
            Cli::try_parse_from(
                args.into_iter()
                    .chain(["route-set", "say", "Test headphones"]),
            )
            .unwrap();
        execute(cli, &mut crate::out::Out::new(&mut |_| Ok(()))).unwrap();
        let selected = base
            .clone()
            .with_collections(std::collections::BTreeMap::from([(
                crate::schemas::voice::COLLECTION_SCOPE_ID,
                root,
            )]));
        assert_eq!(
            Voice::with_storage(selected).route(Channel::Say).unwrap(),
            vec!["Test headphones"]
        );
        assert!(!Voice::with_storage(base)
            .route(Channel::Say)
            .unwrap()
            .contains(&"Test headphones".to_owned()));
        for command in ["say", "shout", "synthesize"] {
            let cli = Cli::try_parse_from(args.into_iter().chain([command, "@missing-text-file"]))
                .unwrap();
            let error = execute(cli, &mut crate::out::Out::new(&mut |_| Ok(()))).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("workspace model-asset routing is unconfigured"),
                "{error:#}"
            );
        }
        let cli = Cli::try_parse_from(args.into_iter().chain(["stream"])).unwrap();
        let error = execute_stream(cli).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("workspace model-asset routing is unconfigured"),
            "{error:#}"
        );
    }

    #[test]
    fn delivery_flags_are_carried_by_each_speech_command() {
        for command in ["say", "shout", "synthesize", "stream"] {
            let mut args = vec![
                "voice",
                "--pile",
                "unused",
                command,
                "--direction",
                "Plain and warm",
                "--cfg-scale",
                "2.5",
            ];
            if command != "stream" {
                args.push("(laugh) Hello.");
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let options = match cli.command.unwrap() {
                Command::Say { options, .. }
                | Command::Shout { options, .. }
                | Command::Synthesize { options, .. }
                | Command::Stream { options, .. } => options,
                _ => unreachable!(),
            };
            assert_eq!(options.direction.as_deref(), Some("Plain and warm"));
            assert_eq!(options.cfg_scale, Some(2.5));
        }
        assert!(Cli::try_parse_from([
            "voice",
            "--pile",
            "unused",
            "synthesize",
            "hello",
            "--direction",
            "soft",
            "--clear-direction"
        ])
        .is_err());
    }

    #[test]
    fn stream_accepts_drive_argument_order() {
        let cli = Cli::try_parse_from([
            "voice",
            "--pile",
            "voice-test.pile",
            "stream",
            "--soma",
            "http://body:8383",
        ])
        .unwrap();
        assert_eq!(cli.soma.as_deref(), Some("http://body:8383"));
        assert!(matches!(cli.command, Some(Command::Stream { .. })));
    }
}

#[derive(Subcommand)]
enum Command {
    /// Speak on the PRIVATE channel — in-ear / headphone only. Routes to the
    /// highest-priority connected private device; if none can be safely
    /// targeted, prints the text instead of playing aloud. Recorded on the
    /// fixed Voice collection.
    Say {
        #[command(flatten)]
        options: SpeechOptions,
        /// What to say.
        text: String,
        /// Resolve routing and report the target (or text-fallback) WITHOUT
        /// synthesizing or playing — for checking the policy on a busy GPU.
        #[arg(long)]
        dry_run: bool,
        /// Half-duplex pause file: created before any sound and removed when
        /// the utterance ends, so a listener started with the SAME path drops
        /// audio while we speak. See `crate::turntaking`. The listener
        /// never closes its microphone — the hold stops the model, never the
        /// person.
        #[arg(long, env = "VOICE_PAUSE_FILE")]
        pause_file: Option<PathBuf>,
    },
    /// Speak ALOUD on the PUBLIC channel — Soma body first, then Reachy daemon
    /// and audible host-device fallbacks. Recorded on the fixed Voice
    /// collection.
    Shout {
        #[command(flatten)]
        options: SpeechOptions,
        /// What to shout.
        text: String,
        /// Resolve routing and report the target WITHOUT synthesizing/playing.
        #[arg(long)]
        dry_run: bool,
        /// Half-duplex pause file — see `voice say --pause-file`. A shout is
        /// exactly the case that needs it: the room speaker is in the room
        /// with the microphone.
        #[arg(long, env = "VOICE_PAUSE_FILE")]
        pause_file: Option<PathBuf>,
    },
    /// Speak a framed text stream from stdin as it arrives. Complete sentences
    /// become 24 kHz mono PCM records on stdout and, for public `shout`, stream
    /// to a configured Soma body through the same resident Mary session.
    Stream {
        #[command(flatten)]
        options: SpeechOptions,
        /// Channel under which completed sentences are recorded.
        #[arg(long, default_value = "shout")]
        channel: String,
        /// Half-duplex pause file, held for the life of the stream.
        #[arg(long, env = "VOICE_PAUSE_FILE")]
        pause_file: Option<PathBuf>,
    },
    /// Show the routing policy for both channels, the connected audio devices,
    /// and what each channel WOULD select right now (a pure dry-run). Read-only.
    Route,
    /// Set the ordered device-preference list for a channel, replacing it.
    /// Devices are matched case-insensitively as substrings of the connected
    /// device names. For `say`, non-private entries are warned about and will be
    /// ignored at speak-time (the privacy invariant can't be configured away).
    RouteSet {
        /// "say" or "shout".
        channel: String,
        /// Device-name patterns in priority order (highest preference first).
        #[arg(required = true)]
        devices: Vec<String>,
    },
    /// Synthesize a resident WAV without playing any local device.
    Synthesize {
        #[command(flatten)]
        options: SpeechOptions,
        /// Literal prose, @file, @- stdin, or @@escaped leading at-sign.
        text: String,
        /// Also save the original WAV here (recommended without Drive).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// List the connected audio output devices and their privacy class. The
    /// raw input to routing — a quick way to see what `say`/`shout` can target.
    Devices,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    if matches!(&cli.command, Some(Command::Stream { .. })) {
        return execute_stream(cli);
    }
    crate::cli::with_output("voice", |out| execute(cli, out))
}

fn execute_stream(cli: Cli) -> Result<()> {
    let Cli {
        pile,
        key,
        workspace,
        soma,
        command,
        ..
    } = cli;
    let Some(Command::Stream {
        channel,
        pause_file,
        options,
    }) = command
    else {
        anyhow::bail!("Voice framed execution requires the stream command");
    };
    let channel = Channel::parse(&channel)?;
    let storage = workspace.storage(pile.clone(), key)?.retained();
    storage.require_ambient_models("voice stream")?;
    let synthesizer = Synthesizer::new(ModelSources::from_environment(Some(&pile)));
    synthesizer.validate_options(&options)?;
    let _pause = pause_file.as_deref().map(|path| {
        eprintln!("  [half-duplex] holding {}", path.display());
        crate::turntaking::PauseGuard::hold(path)
    });
    let voice = Voice::with_storage(storage.clone());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let result = super::stream::run_with_options(
        &voice,
        &synthesizer,
        &options,
        channel,
        soma.as_deref(),
        stdin.lock(),
        stdout.lock(),
    )
    .map(|_| ());
    storage.finish(result)
}

pub fn execute(cli: Cli, out: &mut crate::out::Out<'_>) -> Result<()> {
    if cli.command.is_none() {
        return out.line(Cli::command().render_help().to_string());
    }
    let storage = cli.workspace.storage(cli.pile.clone(), cli.key)?;
    if matches!(
        &cli.command,
        Some(Command::Say { dry_run: false, .. })
            | Some(Command::Shout { dry_run: false, .. })
            | Some(Command::Synthesize { .. })
            | Some(Command::Stream { .. })
    ) {
        storage.require_ambient_models("voice synthesis")?;
    }
    let synthesizer = Synthesizer::new(ModelSources::from_environment(Some(&cli.pile)));
    let voice = Voice::with_storage(storage);
    let device = Device::new(voice.clone(), cli.daemon, cli.soma, synthesizer.clone());
    match cli.command {
        None => anyhow::bail!("Voice requires an explicit command"),
        Some(Command::Say {
            text,
            dry_run,
            pause_file,
            options,
        }) => {
            synthesizer.validate_options(&options)?;
            let text = crate::text_arg(&text, "voice text")?;
            device
                .speak_with_options(
                    Channel::Say,
                    &text,
                    &options,
                    dry_run,
                    pause_file.as_deref(),
                    out,
                )?
                .emit(Channel::Say, out)
        }
        Some(Command::Shout {
            text,
            dry_run,
            pause_file,
            options,
        }) => {
            synthesizer.validate_options(&options)?;
            let text = crate::text_arg(&text, "voice text")?;
            device
                .speak_with_options(
                    Channel::Shout,
                    &text,
                    &options,
                    dry_run,
                    pause_file.as_deref(),
                    out,
                )?
                .emit(Channel::Shout, out)
        }
        Some(Command::Stream { .. }) => {
            anyhow::bail!("voice stream writes framed PCM and must bypass ordinary CLI output")
        }
        Some(Command::Route) => device.routes()?.emit(out),
        Some(Command::RouteSet { channel, devices }) => voice
            .set_route(Channel::parse(&channel)?, &devices)?
            .emit(out),
        Some(Command::Devices) => {
            let devices = super::device::detect_output_devices()?;
            if devices.is_empty() {
                out.line("no audio output devices found.")?;
            }
            for device in devices {
                out.line(super::device::describe_device(&device))?;
            }
            Ok(())
        }
        Some(Command::Synthesize {
            text,
            out: path,
            options,
        }) => {
            synthesizer.validate_options(&options)?;
            anyhow::ensure!(
                cfg!(feature = "voice"),
                "speech synthesis requires a build with the `voice` feature"
            );
            let text = crate::text_arg(&text, "voice text")?;
            let clip = synthesizer.synthesize_with_options(&text, &options)?;
            if let Some(path) = path {
                clip.save(&path)?;
                out.line(path.display().to_string())?;
            }
            out.audio(clip.wav, super::AUDIO_WAV_MIME)
        }
    }
}
