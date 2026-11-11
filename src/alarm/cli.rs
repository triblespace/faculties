//! Alarm CLI: the raiser comes from PERSONA, the rest is literal.

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use super::guards::THE_CHECK;
use super::ladder::{Capabilities, Due};
use super::operations::{Alarm, RaiseRequest, SystemClock};
use super::render;
use super::rungs::{RungResult, RungRunner, Subject};
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
    #[command(subcommand)]
    command: Option<Command>,
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
}

/// The rung executor of a build without transports: every rung it is asked
/// to run is recorded failed, so the ledger never claims a delivery.
struct Unwired;

impl RungRunner for Unwired {
    fn capabilities(&mut self, _subject: &Subject) -> Capabilities {
        Capabilities::default()
    }

    fn run(&mut self, _subject: &Subject, _due: Due) -> RungResult {
        RungResult::failed("no rung executor in this build")
    }
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
    let alarm = Alarm::with_storage(cli.workspace.storage(cli.pile, cli.key)?);
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
            let raised = alarm.raise(&request, &mut Unwired, &mut clock)?;
            render::raised(&raised, output)
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
        Command::List { open } => render::list(&alarm.list(open, &mut clock)?, output),
        Command::Show { id } => render::show(&alarm.show(&id)?, output),
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
