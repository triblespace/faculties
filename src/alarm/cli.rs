//! Alarm CLI: the raiser comes from PERSONA, the rest is literal.

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use super::follow;
use super::guards::THE_CHECK;
use super::operations::{Alarm, RaiseRequest, Raised, SystemClock};
use super::render;
use super::rungs::{Executor, Live, Settings};
use crate::out::Out;
use crate::schemas::alarm::{VIA_CLI, VIA_RELAYED};

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "alarm",
    about = "Wake JP physically when something is about to be lost, and keep the ledger of it",
    after_help = "Before raising: name the physical thing you measured and the semantic thing \
                  you care about, and ask whether the system ever promised they would agree."
)]
pub struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    /// Path to the pile file.
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable signing-key file. Reads and writes never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    /// Who acts: a live zooid's Relations label or id. Raises, acks,
    /// resolves and notes are attributed to it.
    #[arg(long, env = "PERSONA", global = true, hide_env_values = true)]
    persona: Option<String>,
    /// The Relations person an alarm wakes. No default.
    #[arg(long = "for", env = "ALARM_FOR", global = true, value_name = "PERSON")]
    for_person: Option<String>,
    #[command(flatten)]
    settings: SettingsArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Where the rungs go on this deployment. No value has a default: a rung
/// whose target is unset is recorded as unreachable from this host.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct SettingsArgs {
    /// JP's Discord user id (a snowflake): the mention pings it, and his
    /// lines are read by it to detect an ack.
    #[arg(
        long,
        env = "ALARM_JP_DISCORD_USER",
        global = true,
        value_name = "SNOWFLAKE"
    )]
    pub jp_discord_user: Option<String>,
    /// The Discord channel a real alarm's mention goes to.
    #[arg(
        long,
        env = "ALARM_DISCORD_CHANNEL",
        global = true,
        value_name = "SNOWFLAKE"
    )]
    pub discord_channel: Option<String>,
    /// The Discord channel a test alarm goes to: the raiser's own.
    #[arg(
        long,
        env = "ALARM_TEST_DISCORD_CHANNEL",
        global = true,
        value_name = "SNOWFLAKE"
    )]
    pub test_discord_channel: Option<String>,
    /// The Discord voice channel the voice rung asks to speak in.
    #[arg(
        long,
        env = "ALARM_VOICE_CHANNEL",
        global = true,
        value_name = "SNOWFLAKE"
    )]
    pub voice_channel: Option<String>,
    /// The voice channel's guild, when the say request should name it.
    #[arg(
        long,
        env = "ALARM_VOICE_GUILD",
        global = true,
        value_name = "SNOWFLAKE"
    )]
    pub voice_guild: Option<String>,
    /// The Relations group of every live zooid: the colony rung's recipient.
    #[arg(long, env = "ALARM_COLONY_GROUP", global = true, value_name = "GROUP")]
    pub colony_group: Option<String>,
    /// The ssh destination of the Mac for banners, sound and shout. Unset on
    /// a Mac, they run locally.
    #[arg(long, env = "ALARM_MAC_SSH", global = true, value_name = "DESTINATION")]
    pub mac_ssh: Option<String>,
    /// The Hue room or zone whose lights breathe.
    #[arg(long, env = "ALARM_HUE_TARGET", global = true, value_name = "ROOM")]
    pub hue_target: Option<String>,
}

impl SettingsArgs {
    pub fn settings(&self) -> Settings {
        let clean = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        Settings {
            jp_discord_user: clean(&self.jp_discord_user),
            discord_channel: clean(&self.discord_channel),
            test_channel: clean(&self.test_discord_channel),
            voice_channel: clean(&self.voice_channel),
            voice_guild: clean(&self.voice_guild),
            colony_group: clean(&self.colony_group),
            mac_ssh: clean(&self.mac_ssh),
            hue_target: clean(&self.hue_target),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Via {
    /// The person who saw it acks it.
    Cli,
    /// A zooid relays JP's word (needs --evidence).
    Relayed,
}

#[derive(Subcommand)]
enum Command {
    /// Raise an alarm: wake JP now, then escalate until someone acks it.
    #[command(after_help = format!(
        "Classes: data-loss, recovery-window, hung-host, security.\n\
         Every alarm names what is lost and by when (--decays) and what you measured \
         (--evidence). Before raising: {THE_CHECK}."
    ))]
    Raise {
        /// data-loss, recovery-window, hung-host or security.
        #[arg(long)]
        class: String,
        /// One line: what is wrong.
        #[arg(long)]
        what: String,
        /// What is lost, and by when, if nobody acts.
        #[arg(long)]
        decays: Option<String>,
        /// What you measured.
        #[arg(long)]
        evidence: Option<String>,
        /// Deduplication key, e.g. pile-truncation:sky:self.pile.
        #[arg(long)]
        incident: Option<String>,
        /// A test: R0 only, to the raiser's own channel, no JP mention.
        #[arg(long)]
        test: bool,
        /// Start a second ladder while one of a different class is open.
        #[arg(long)]
        separate: bool,
        /// Print what would happen; write and send nothing.
        #[arg(long)]
        dry_run: bool,
        /// The raising host. Default: this machine's short host name.
        #[arg(long)]
        host: Option<String>,
    },
    /// Record that an alarm was seen; this closes it.
    Ack {
        /// Alarm id or unique prefix.
        id: String,
        /// Who saw it. Default: PERSONA.
        #[arg(long)]
        by: Option<String>,
        #[arg(long, value_enum, default_value_t = Via::Cli)]
        via: Via,
        /// For a relayed ack: JP's words and where he said them.
        #[arg(long)]
        evidence: Option<String>,
    },
    /// Record that the cause was dealt with; this closes the alarm.
    Resolve {
        /// Alarm id or unique prefix.
        id: String,
        #[arg(long)]
        reason: String,
        /// It was a false alarm: halves the raiser's daily allowance for 7 days.
        #[arg(long)]
        false_alarm: bool,
    },
    /// Attach news to an alarm without restarting its ladder.
    Note {
        /// Alarm id or unique prefix.
        id: String,
        /// The news. Use @path for file input or @- for stdin.
        text: String,
    },
    /// List alarms, newest first.
    List {
        /// Only the open ones.
        #[arg(long)]
        open: bool,
    },
    /// Show one alarm, its steps and everything recorded about it.
    Show {
        /// Alarm id or unique prefix.
        id: String,
    },
    /// Run an alarm's ladder until it is acked, resolved or over. `raise`
    /// starts this in a transient unit; run it by hand when raise says the
    /// ladder is not running. Best-effort across hosts: a follower on each
    /// of two hosts would both run the ladder.
    Follow {
        /// Alarm id or unique prefix.
        #[arg(required_unless_present = "resume", conflicts_with = "resume")]
        id: Option<String>,
        /// Start a follower for every open alarm whose follower is gone
        /// (its newest step is older than twice the sound interval and no
        /// live process holds its lock here). For a boot path.
        #[arg(long)]
        resume: bool,
    },
}

fn persona(persona: &Option<String>) -> Result<String> {
    match persona.as_deref().map(str::trim) {
        Some(persona) if !persona.is_empty() => Ok(persona.to_owned()),
        _ => bail!("no persona: set PERSONA (or pass --persona) to a live zooid"),
    }
}

/// Execute a parsed command. No subcommand prints usage and opens nothing.
pub fn execute(cli: Cli, output: &mut Out<'_>) -> Result<()> {
    let Some(command) = cli.command else {
        return output.text(Cli::command().render_help().to_string());
    };
    let storage = cli.workspace.storage(cli.pile, cli.key)?;
    let alarm = Alarm::with_storage(storage.clone());
    let settings = cli.settings.settings();
    let mut clock = SystemClock;
    match command {
        Command::Raise {
            class,
            what,
            decays,
            evidence,
            incident,
            test,
            separate,
            dry_run,
            host,
        } => {
            let raiser = persona(&cli.persona)?;
            let Some(for_person) = cli.for_person.filter(|person| !person.trim().is_empty()) else {
                bail!("no one to wake: set ALARM_FOR (or pass --for) to a Relations person");
            };
            let host = match host {
                Some(host) => host,
                None => crate::quota::local_host()?,
            };
            if matches!(class.trim(), "data-loss" | "recovery-window") {
                output.line(render::SECURE_FIRST)?;
            }
            let request = RaiseRequest {
                raiser: &raiser,
                for_person: &for_person,
                class: &class,
                host: &host,
                what: &what,
                decays: decays.as_deref().unwrap_or(""),
                evidence: evidence.as_deref().unwrap_or(""),
                incident: incident.as_deref(),
                test,
                separate,
            };
            if dry_run {
                return render::preview(&alarm.preview(&request, &mut clock)?, output);
            }
            let mut executor = Executor::new(
                Live::new(storage.clone()),
                settings.clone(),
                crate::quota::local_host()?,
            );
            let raised = alarm.raise(&request, &mut executor, &mut clock)?;
            render::raised(&raised, output)?;
            if let Raised::Alarm {
                alarm: id, subject, ..
            } = &raised
            {
                if subject.test {
                    output.line("test: no follower, the ladder stops after R0")?;
                } else {
                    let locks = follow::runtime_dir()?;
                    match follow::start(
                        &storage,
                        &settings,
                        &follow::ambient_routes(),
                        *id,
                        &mut *follow::default_spawner(&locks),
                    ) {
                        Ok(unit) => output.line(format!("follower started: {unit}"))?,
                        Err(error) => bail!(
                            "rung 0 sent; ladder NOT running ({error:#}); run `alarm follow {id:x}`"
                        ),
                    }
                }
            }
            Ok(())
        }
        Command::Ack {
            id,
            by,
            via,
            evidence,
        } => {
            let by = match by {
                Some(by) => by,
                None => persona(&cli.persona)?,
            };
            let via = match via {
                Via::Cli => VIA_CLI,
                Via::Relayed => VIA_RELAYED,
            };
            let recorded = alarm.ack(&id, &by, via, evidence.as_deref(), &mut clock)?;
            render::recorded("ack", &recorded, output)
        }
        Command::Resolve {
            id,
            reason,
            false_alarm,
        } => {
            let by = persona(&cli.persona)?;
            let recorded = alarm.resolve(&id, &by, &reason, false_alarm, &mut clock)?;
            render::recorded("resolve", &recorded, output)
        }
        Command::Note { id, text } => {
            let by = persona(&cli.persona)?;
            let text = crate::text_arg(&text, "note text")?;
            let recorded = alarm.note(&id, &by, &text, &mut clock)?;
            render::recorded("note", &recorded, output)
        }
        Command::List { open } => render::list(
            &alarm.list(open, settings.jp_discord_user.as_deref(), &mut clock)?,
            output,
        ),
        Command::Show { id } => render::show(&alarm.show(&id)?, output),
        Command::Follow { id, resume } => {
            let locks = follow::runtime_dir()?;
            if resume {
                let routes = follow::ambient_routes();
                let adopted = follow::resume(
                    &alarm,
                    &mut clock,
                    &locks,
                    &mut *follow::default_spawner(&locks),
                    &|id| {
                        let pile = std::fs::canonicalize(storage.path())?;
                        let key = std::fs::canonicalize(crate::storage::signer_path(
                            &pile,
                            storage.key_path(),
                        ))?;
                        follow::follower_command(
                            &follow::alarm_program()?,
                            &pile,
                            &key,
                            id,
                            follow::follower_env(&settings, routes.iter().cloned()),
                        )
                    },
                )?;
                return render::adopted(&adopted, output);
            }
            let id = alarm.resolve_id(id.as_deref().expect("clap requires an id"))?;
            let _lock = follow::Lock::acquire(&locks, id)?;
            // One store for the whole ladder: opening the pile is the
            // expensive part, and the follower reads it every few seconds.
            let storage = storage.retained();
            let alarm = Alarm::with_storage(storage.clone());
            let mut executor = Executor::new(
                Live::new(storage.clone()),
                settings.clone(),
                crate::quota::local_host()?,
            )
            .probing_the_mac();
            let discord = crate::discord::Discord::with_storage(storage.clone());
            let channel = settings.discord_channel.clone();
            let mut pull = || match &channel {
                Some(channel) => discord
                    .pull(crate::discord::PullOptions {
                        channel_id: Some(channel.clone()),
                        ..Default::default()
                    })
                    .map(drop),
                None => Ok(()),
            };
            let ended = follow::follow(
                &alarm,
                id,
                &mut executor,
                &mut clock,
                settings.jp_discord_user.as_deref(),
                &mut pull,
            );
            storage.finish(ended.and_then(|ended| render::ended(id, ended, output)))
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    crate::cli::with_output("alarm", |output| execute(cli, output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raise_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = vec![
            "alarm",
            "--pile",
            "unused.pile",
            "raise",
            "--class",
            "data-loss",
            "--what",
            "self.pile is 19 KB",
            "--decays",
            "the snapshot cycles out at 04:00",
            "--evidence",
            "ls -l says 19 KB, it was 6.1 GB",
        ];
        args.extend_from_slice(extra);
        args
    }

    #[test]
    fn alarm_raise_parses_with_its_reason_and_optional_flags() {
        assert!(Cli::try_parse_from(raise_args(&[])).is_ok());
        assert!(Cli::try_parse_from(raise_args(&[
            "--incident",
            "pile-truncation:sky:self.pile",
            "--test",
            "--separate",
            "--dry-run",
            "--host",
            "sky",
        ]))
        .is_ok());
        // A raise names its class and what is wrong.
        assert!(Cli::try_parse_from(["alarm", "--pile", "p", "raise", "--what", "x"]).is_err());
        assert!(Cli::try_parse_from(raise_args(&["--color", "red"])).is_err());
    }

    #[test]
    fn alarm_ack_resolve_note_list_show_parse() {
        let base = ["alarm", "--pile", "p"];
        let parses = |rest: &[&str]| Cli::try_parse_from(base.iter().chain(rest)).is_ok();
        assert!(parses(&["ack", "9e1a"]));
        assert!(parses(&[
            "ack",
            "9e1a",
            "--by",
            "jp",
            "--via",
            "relayed",
            "--evidence",
            "said seen in #cc"
        ]));
        assert!(!parses(&["ack"]));
        assert!(!parses(&["ack", "9e1a", "--via", "carrier-pigeon"]));
        assert!(parses(&[
            "resolve",
            "9e1a",
            "--reason",
            "restored",
            "--false-alarm"
        ]));
        assert!(!parses(&["resolve", "9e1a"]));
        assert!(parses(&["note", "9e1a", "the copy finished"]));
        assert!(parses(&["list", "--open"]));
        assert!(parses(&["show", "9e1a"]));
        assert!(parses(&["follow", "9e1a"]));
        assert!(parses(&["follow", "--resume"]));
        assert!(!parses(&["follow"]));
        assert!(!parses(&["follow", "9e1a", "--resume"]));
        assert!(!parses(&["show"]));
    }

    #[test]
    fn alarm_without_a_subcommand_prints_usage_and_opens_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("never.pile");
        let cli = Cli::try_parse_from(["alarm", "--pile", pile.to_str().unwrap()]).unwrap();
        let mut text = String::new();
        execute(
            cli,
            &mut Out::new(&mut |part| {
                if let crate::out::Part::Text { text: part } = part {
                    text.push_str(&part);
                }
                Ok(())
            }),
        )
        .unwrap();
        assert!(text.contains("raise"), "{text}");
        assert!(text.contains("follow") || text.contains("show"), "{text}");
        assert!(!pile.exists());
    }

    #[test]
    fn alarm_raise_without_a_persona_is_refused_before_storage() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("never.pile");
        let mut args = raise_args(&[]);
        args[2] = pile.to_str().unwrap();
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(cli.persona.is_none(), "the test runs without PERSONA");
        let error = execute(cli, &mut Out::new(&mut |_| Ok(()))).unwrap_err();
        assert!(format!("{error:#}").contains("PERSONA"), "{error:#}");
        assert!(!pile.exists());
    }
}
