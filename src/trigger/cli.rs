//! Explicit one-shot Trigger operations. This frontend never starts a watcher
//! or acknowledges results on behalf of Orient's notification owner.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use triblespace::prelude::Id;

use super::model;
use super::operations::{ExecutionOptions, Triggers};
use super::runner::{Context, Outcome, Presentation};

#[derive(Parser)]
#[command(version = crate::GIT_VERSION, name = "trigger",
    about = "Run checks on explicit timers or events and preserve their results")]
pub struct Cli {
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable signing-key file; ordinary commands never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Add a fixed-period check. Completion and acknowledgement do not reschedule it.
    Add {
        label: String,
        /// Positive interval, for example 90s, 30m, 24h or 7d.
        #[arg(long, value_parser = interval_seconds)]
        every: u64,
        /// Shell command; @file and @- read command text, not the check's stdin.
        #[arg(long)]
        check: String,
        /// Exact recipient ID. Omit to address every persona.
        #[arg(long = "persona", value_parser = parse_id)]
        personas: Vec<Id>,
        /// Exact previous definition ID. Repeat to join observed revisions.
        #[arg(long, value_parser = parse_id)]
        supersedes: Vec<Id>,
    },
    /// Add an event-bound check; an advisory check never vetoes its caller.
    AddEvent {
        label: String,
        #[arg(long)]
        event: String,
        #[arg(long, value_enum)]
        context: EventContext,
        #[arg(long)]
        check: String,
        /// Exact recipient ID. Omit to address every persona.
        #[arg(long = "persona", value_parser = parse_id)]
        personas: Vec<Id>,
        #[arg(long, value_parser = parse_id)]
        supersedes: Vec<Id>,
    },
    /// List definitions, including superseded and paused definitions.
    List,
    /// Show a definition by exact ID; labels are not identity keys.
    Show {
        #[arg(value_parser = parse_id)]
        trigger: Id,
    },
    /// Execute each currently due timer slot once for this local signer/persona.
    RunDue {
        #[arg(long, value_parser = parse_id)]
        persona: Id,
        #[command(flatten)]
        execution: Execution,
    },
    /// Run a named event definition and return its synchronous verdict, if any.
    RunEvent {
        #[arg(value_parser = parse_id)]
        trigger: Id,
        #[arg(long, value_parser = parse_id)]
        persona: Id,
        #[arg(long, value_enum)]
        context: EventContext,
        /// Stable caller occurrence ID. Repeated delivery reuses its result;
        /// omit only when this invocation is a genuinely new occurrence.
        #[arg(long, value_parser = parse_id)]
        occurrence: Option<Id>,
        #[command(flatten)]
        execution: Execution,
    },
    /// Route a caller-owned event to interested checks. Does not start a watcher.
    EmitEvent {
        event: String,
        #[arg(long, value_parser = parse_id)]
        persona: Id,
        #[arg(long, value_enum)]
        context: EventContext,
        /// Preserve this ID when redelivering the same occurrence.
        #[arg(long, value_parser = parse_id)]
        occurrence: Id,
        #[command(flatten)]
        execution: Execution,
    },
    /// Disclosure policy, scans and clearances using the existing native grammar.
    Disclosure {
        #[command(subcommand)]
        command: crate::posture::cli::Command,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EventContext {
    Advisory,
    Synchronous,
}

impl From<EventContext> for Context {
    fn from(context: EventContext) -> Self {
        match context {
            EventContext::Advisory => Self::AdvisoryEvent,
            EventContext::Synchronous => Self::SynchronousEvent,
        }
    }
}

#[derive(Args)]
struct Execution {
    /// Working directory supplied to the check, never inferred from its label.
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Check input bytes from a file or `-` for stdin. Omit for empty input.
    #[arg(long, value_name = "PATH_OR_-")]
    stdin: Option<PathBuf>,
    #[arg(long, default_value = "30s", value_parser = interval_seconds)]
    timeout: u64,
    /// Maximum combined stdout/stderr bytes; exceeding it fails the check.
    #[arg(long, default_value = "1048576", value_parser = positive_bytes)]
    output_limit: usize,
}

impl Execution {
    fn input(&self) -> Result<Vec<u8>> {
        match self.stdin.as_deref() {
            None => Ok(Vec::new()),
            Some(path) if path.as_os_str() == "-" => {
                let mut bytes = Vec::new();
                io::stdin()
                    .read_to_end(&mut bytes)
                    .context("read check stdin")?;
                Ok(bytes)
            }
            Some(path) => {
                std::fs::read(path).with_context(|| format!("read check input {}", path.display()))
            }
        }
    }

    fn options<'a>(&'a self, stdin: &'a [u8]) -> ExecutionOptions<'a> {
        ExecutionOptions {
            directory: &self.cwd,
            stdin,
            timeout: Duration::from_secs(self.timeout),
            output_limit: self.output_limit,
        }
    }
}

fn parse_id(raw: &str) -> std::result::Result<Id, String> {
    Id::from_hex(raw)
        .ok_or_else(|| format!("expected an exact 32-digit hexadecimal ID, got {raw:?}"))
}

fn interval_seconds(raw: &str) -> std::result::Result<u64, String> {
    let text = raw.trim();
    let split = text
        .find(|character: char| !character.is_ascii_digit())
        .ok_or_else(|| "interval needs a seconds/minutes/hours/days unit".to_owned())?;
    let (digits, unit) = text.split_at(split);
    let amount = digits
        .parse::<u64>()
        .map_err(|_| "invalid interval quantity".to_owned())?;
    let scale = match unit.trim() {
        "s" | "second" | "seconds" => 1,
        "m" | "minute" | "minutes" => 60,
        "h" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return Err("interval unit must be seconds, minutes, hours or days (s/m/h/d)".into()),
    };
    amount
        .checked_mul(scale)
        .filter(|value| *value > 0)
        .ok_or_else(|| "interval must be positive and fit in seconds".to_owned())
}

fn positive_bytes(raw: &str) -> std::result::Result<usize, String> {
    raw.parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "output limit must be a positive byte count".to_owned())
}

/// Successful check output is emitted byte-for-byte, with no added banner or
/// newline. Failure diagnostics use stderr; partial stdout is not a success
/// message. An advisory failure is reported but cannot veto its event caller.
fn render_outcome(
    outcome: &Outcome,
    verdict: Option<i32>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<i32> {
    match outcome.presentation() {
        Presentation::Quiet => {}
        Presentation::Message(bytes) => stdout.write_all(bytes)?,
        Presentation::Failed(status) => {
            writeln!(stderr, "trigger: failed check: {status:?}")?;
            stderr.write_all(&outcome.stderr)?;
        }
    }
    if outcome.context == Context::SynchronousEvent {
        // A successful native check can still deny. Replayed results carry
        // that authored verdict; execution success must not replace it.
        return verdict.context("synchronous check result has no understood verdict; not allowing");
    }
    Ok(i32::from(
        outcome.context == Context::Timer
            && matches!(outcome.presentation(), Presentation::Failed(_)),
    ))
}

pub fn run() -> Result<ExitCode> {
    execute(Cli::parse())
}

fn execute(cli: Cli) -> Result<ExitCode> {
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(ExitCode::SUCCESS);
    };
    if let Command::Disclosure { command } = command {
        crate::cli::with_output("trigger", |out| {
            crate::posture::cli::execute_command(cli.pile, cli.key, command, &["disclosure"], out)
        })?;
        return Ok(ExitCode::SUCCESS);
    }
    let triggers = Triggers::new(cli.pile, cli.key);
    let verdict = match command {
        Command::Add {
            label,
            every,
            check,
            personas,
            supersedes,
        } => {
            let check = crate::text_arg(&check, "Trigger check")?;
            let definition = model::timer_definition(
                &label,
                &check,
                every,
                crate::clock::now()?,
                &personas,
                &supersedes,
            )?;
            let id = triggers.publish(definition)?;
            crate::cli::with_output("trigger", |out| out.line(format!("{id:x}")))?;
            0
        }
        Command::AddEvent {
            label,
            event,
            context,
            check,
            personas,
            supersedes,
        } => {
            let check = crate::text_arg(&check, "Trigger check")?;
            let id = triggers.publish(model::event_definition(
                &label,
                &check,
                &event,
                context.into(),
                &personas,
                &supersedes,
            )?)?;
            crate::cli::with_output("trigger", |out| out.line(format!("{id:x}")))?;
            0
        }
        Command::List => {
            let definitions = triggers.list(None)?;
            crate::cli::with_output("trigger", |out| {
                for definition in definitions {
                    out.line(format!(
                        "{:x} {} [{}; {}; {}]",
                        definition.id,
                        definition.label,
                        definition.context.as_str(),
                        if definition.current {
                            "current"
                        } else {
                            "superseded"
                        },
                        if definition.active {
                            "active"
                        } else {
                            "not runnable"
                        }
                    ))?;
                }
                Ok(())
            })?;
            0
        }
        Command::Show { trigger } => {
            let definitions = triggers.list(Some(trigger))?;
            if definitions.is_empty() {
                bail!("Trigger definition {trigger:x} is not readable");
            }
            crate::cli::with_output("trigger", |out| {
                for definition in definitions {
                    out.line(format!("{:x} {}", definition.id, definition.label))?;
                    out.line(format!(
                        "context: {}; current: {}; active: {}",
                        definition.context.as_str(),
                        definition.current,
                        definition.active
                    ))?;
                    if let Some(seconds) = definition.interval_seconds {
                        out.line(format!("interval: {seconds}s"))?;
                    }
                    if let Some(event) = definition.event {
                        out.line(format!("event: {event}"))?;
                    }
                    match definition.command {
                        Ok(command) => out.line(format!("check: {command}"))?,
                        Err(error) => out.line(format!("check unavailable: {error}"))?,
                    }
                }
                Ok(())
            })?;
            0
        }
        Command::RunDue { persona, execution } => {
            let input = execution.input()?;
            let results =
                triggers.run_due(persona, crate::clock::now()?, execution.options(&input))?;
            let mut stdout = io::stdout().lock();
            let mut stderr = io::stderr().lock();
            let mut verdict = 0;
            for result in results {
                verdict = verdict.max(render_outcome(
                    &result.outcome,
                    result.verdict,
                    &mut stdout,
                    &mut stderr,
                )?);
            }
            stdout.flush()?;
            stderr.flush()?;
            verdict
        }
        Command::RunEvent {
            trigger,
            persona,
            context,
            occurrence,
            execution,
        } => {
            let input = execution.input()?;
            let results = triggers.run_event(
                trigger,
                persona,
                context.into(),
                None,
                occurrence,
                execution.options(&input),
            )?;
            let mut stdout = io::stdout().lock();
            let mut stderr = io::stderr().lock();
            let mut verdict = 0;
            for result in results {
                let next =
                    render_outcome(&result.outcome, result.verdict, &mut stdout, &mut stderr)?;
                if verdict == 0 {
                    verdict = next;
                }
            }
            stdout.flush()?;
            stderr.flush()?;
            verdict
        }
        Command::EmitEvent {
            event,
            persona,
            context,
            occurrence,
            execution,
        } => {
            let input = execution.input()?;
            let results = triggers.dispatch_event(
                &event,
                persona,
                context.into(),
                occurrence,
                execution.options(&input),
            )?;
            let mut stdout = io::stdout().lock();
            let mut stderr = io::stderr().lock();
            let mut verdict = 0;
            for result in results {
                let next =
                    render_outcome(&result.outcome, result.verdict, &mut stdout, &mut stderr)?;
                if verdict == 0 {
                    verdict = next;
                }
            }
            stdout.flush()?;
            stderr.flush()?;
            verdict
        }
        Command::Disclosure { .. } => unreachable!("handled before opening Trigger operations"),
    };
    let verdict = u8::try_from(verdict)
        .map_err(|_| anyhow!("check returned invalid exit verdict {verdict}"))?;
    Ok(ExitCode::from(verdict))
}

#[cfg(test)]
mod tests {
    use super::super::runner::Status;
    use super::*;

    fn render(context: Context, status: Status, bytes: &[u8]) -> (i32, Vec<u8>, Vec<u8>) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = Outcome {
            context,
            status,
            stdout: bytes.to_vec(),
            stderr: Vec::new(),
        };
        let verdict = render_outcome(
            &outcome,
            outcome.synchronous_verdict(),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        (verdict, stdout, stderr)
    }

    #[test]
    fn interval_requires_positive_seconds_minutes_hours_or_days() {
        for (input, seconds) in [
            ("3s", 3),
            ("2 minutes", 120),
            ("4h", 14400),
            ("1 day", 86400),
        ] {
            assert_eq!(interval_seconds(input).unwrap(), seconds);
        }
        for input in ["0s", "-1h", "1", "1.5h", "1week", "18446744073709551615d"] {
            assert!(interval_seconds(input).is_err(), "{input}");
        }
    }

    #[test]
    fn literal_empty_stdout_is_quiet_but_whitespace_and_non_utf8_are_messages() {
        assert_eq!(
            render(Context::Timer, Status::Success, b""),
            (0, vec![], vec![])
        );
        for message in [&b" \n"[..], &b"hello"[..], &b"\xff"[..]] {
            assert_eq!(
                render(Context::Timer, Status::Success, message),
                (0, message.to_vec(), vec![])
            );
        }
    }

    #[test]
    fn failure_is_explicit_and_only_synchronous_event_can_veto_event_caller() {
        let (timer, output, diagnostics) = render(Context::Timer, Status::Exit(23), b"partial");
        assert_eq!(timer, 1);
        assert!(output.is_empty());
        assert!(String::from_utf8(diagnostics)
            .unwrap()
            .contains("failed check: Exit(23)"));
        assert_eq!(render(Context::AdvisoryEvent, Status::Exit(23), b"").0, 0);
        assert_eq!(
            render(Context::SynchronousEvent, Status::Exit(23), b"").0,
            23
        );
        assert_eq!(
            render(Context::SynchronousEvent, Status::TimedOut, b"").0,
            1
        );
    }

    #[test]
    fn synchronous_replay_preserves_native_denial_and_missing_verdict_is_not_allow() {
        let outcome = Outcome {
            context: Context::SynchronousEvent,
            status: Status::Success,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(
            render_outcome(&outcome, Some(23), &mut Vec::new(), &mut Vec::new()).unwrap(),
            23
        );
        assert!(render_outcome(&outcome, None, &mut Vec::new(), &mut Vec::new()).is_err());
    }

    #[test]
    fn parser_has_explicit_events_and_no_done_or_cooldown() {
        let id = "4634FE1E28E12954771242E7590F298A";
        assert!(Cli::try_parse_from([
            "trigger",
            "--pile",
            "unused.pile",
            "emit-event",
            "repo-refs-changed",
            "--persona",
            id,
            "--context",
            "advisory",
            "--occurrence",
            id,
        ])
        .is_ok());
        assert!(
            Cli::try_parse_from([
                "trigger",
                "--pile",
                "unused.pile",
                "emit-event",
                "repo-refs-changed",
                "--persona",
                id,
                "--context",
                "advisory",
            ])
            .is_err(),
            "an event source must name the occurrence for safe redelivery"
        );
        assert!(Cli::try_parse_from([
            "trigger",
            "--pile",
            "unused.pile",
            "add-event",
            "pre-push",
            "--check",
            "exit 0",
            "--event",
            "pre-push",
            "--context",
            "synchronous",
            "--persona",
            id
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "trigger",
            "--pile",
            "unused.pile",
            "run-event",
            id,
            "--persona",
            id,
            "--context",
            "advisory",
            "--occurrence",
            id,
            "--cwd",
            ".",
            "--stdin",
            "-"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["trigger", "--pile", "unused.pile", "done", id]).is_err());
        assert!(Cli::try_parse_from([
            "trigger",
            "--pile",
            "unused.pile",
            "add",
            "notice",
            "--every",
            "1h",
            "--check",
            "printf notice",
            "--cooldown",
            "1h"
        ])
        .is_err());
    }

    #[test]
    fn disclosure_reuses_existing_parser() {
        assert!(Cli::try_parse_from([
            "trigger",
            "--pile",
            "unused.pile",
            "disclosure",
            "vocab",
            "add",
            "protected",
            "--channel",
            "github-public"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "trigger",
            "--pile",
            "unused.pile",
            "disclosure",
            "git",
            "--repo",
            ".",
            "HEAD",
            "--not",
            "--remotes=origin"
        ])
        .is_ok());
    }
}
