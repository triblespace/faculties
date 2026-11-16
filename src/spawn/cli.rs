//! Explicit Spawn CLI: brief from a file or stdin, ambient spawner, waiting.
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use triblespace::prelude::Id;

use super::operations::{describe_end, render_list, render_show};
use super::{Exit, LaunchConfig, RunRequest, SettleRequest, Spawn};

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "spawn",
    about = "Start a task zooid from a brief, in its own systemd user unit"
)]
pub(crate) struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    /// Path to the pile file.
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable signing-key file. Ordinary operations never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Launch the zooid a brief names, and print its name.
    ///
    /// The name is the brief's hash as two proquints. An
    /// identical brief relaunches the same zooid once its last run has
    /// settled; while its persona is live this refuses.
    Run {
        /// The brief: a file, or `-` for stdin. It is the prompt, verbatim.
        #[arg(long, value_name = "PATH")]
        brief: String,
        /// Directory the zooid starts in.
        #[arg(long, value_name = "DIR")]
        workdir: PathBuf,
        /// Claude effort: low, medium, high, xhigh or max.
        #[arg(long, default_value = "high")]
        effort: String,
        /// The spawning persona, messaged when the zooid ends. Defaults to $PERSONA.
        #[arg(long, value_name = "PERSONA")]
        spawner: Option<String>,
    },
    /// Every spawn, one line each.
    List,
    /// One spawn and all its runs, with the latest report.
    Show {
        name: String,
        /// Print the brief too.
        #[arg(long)]
        brief: bool,
    },
    /// Block until the latest run's unit has ended, then print how it ended
    /// and its report. Run it as a background task in the spawning window.
    Wait {
        name: String,
        /// Seconds between unit-state checks.
        #[arg(long, default_value_t = 30)]
        poll: u64,
    },
    /// The unit's stop-post step: record the end, message the spawner as the
    /// zooid, retire the persona. The flags take systemd's EXIT_CODE,
    /// EXIT_STATUS and SERVICE_RESULT.
    Settle {
        name: String,
        /// The run being settled (hex id), as `spawn run` wrote it into the script.
        #[arg(long, value_name = "ID")]
        run: String,
        /// `exited`, `killed` or `dumped` (systemd's EXIT_CODE).
        #[arg(long, default_value = "")]
        code: String,
        /// Exit number or signal name (systemd's EXIT_STATUS).
        #[arg(long, default_value = "")]
        status: String,
        /// systemd's SERVICE_RESULT, e.g. `success` or `oom-kill`.
        #[arg(long, default_value = "")]
        result: String,
        /// File holding claude's stdout.
        #[arg(long, value_name = "PATH")]
        report: Option<PathBuf>,
    },
}

fn read_brief(source: &str) -> Result<String> {
    let mut bytes = Vec::new();
    if source == "-" {
        std::io::stdin()
            .read_to_end(&mut bytes)
            .context("read the brief from stdin")?;
    } else {
        bytes = std::fs::read(source).with_context(|| format!("read the brief {source}"))?;
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("the brief is not UTF-8"))
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    let storage = cli.workspace.storage(cli.pile, cli.key)?;
    if let Command::Wait { name, poll } = &command {
        // Each check opens the pile afresh, so a wait that lasts hours keeps
        // no store open and reads the settlement another process wrote.
        let spawn = Spawn::new(storage, LaunchConfig::from_home_env()?);
        let view = spawn.wait(name, Duration::from_secs((*poll).max(1)))?;
        let run = view.runs.last().expect("wait found a run");
        let mut text = format!("{} ended: {}\n", view.name, describe_end(run));
        text.push_str(&render_show(&view, false));
        return crate::cli::with_output("spawn", |out| out.text(text));
    }
    storage.scope_to_exit(|storage| {
        let spawn = Spawn::new(storage.clone(), LaunchConfig::from_home_env()?);
        match command {
            Command::Run {
                brief,
                workdir,
                effort,
                spawner,
            } => {
                let spawner = spawner
                    .or_else(|| std::env::var("PERSONA").ok())
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow!("pass --spawner or set PERSONA"))?;
                let launched = spawn.run(&RunRequest {
                    brief: read_brief(&brief)?,
                    workdir,
                    effort,
                    spawner,
                })?;
                crate::cli::with_output("spawn", |out| {
                    out.line(&launched.name)?;
                    out.note(format!(
                        "started {} (run {:x}, {} prompt chars); report -> {}, log -> {}; \
                         `spawn wait {}` blocks until it ends",
                        launched.unit,
                        launched.run,
                        launched.prompt_chars,
                        launched.report.display(),
                        launched.log.display(),
                        launched.name
                    ))
                })
            }
            Command::List => {
                let spawns = spawn.list()?;
                crate::cli::with_output("spawn", |out| out.text(render_list(&spawns)))
            }
            Command::Show { name, brief } => {
                let view = spawn.show(&name)?;
                crate::cli::with_output("spawn", |out| out.text(render_show(&view, brief)))
            }
            Command::Wait { .. } => unreachable!("handled above"),
            Command::Settle {
                name,
                run,
                code,
                status,
                result,
                report,
            } => {
                let run =
                    Id::from_hex(run.trim()).ok_or_else(|| anyhow!("invalid run id {run}"))?;
                let exit = Exit::from_systemd(&code, &status, &result);
                let settled = spawn.settle(&SettleRequest {
                    name: name.clone(),
                    run,
                    exit: exit.clone(),
                    report,
                })?;
                crate::cli::with_output("spawn", |out| {
                    out.line(format!(
                        "settled {name} run {:x}: {}, report {}",
                        settled.run,
                        exit.describe(),
                        settled
                            .report
                            .map(super::handle_text)
                            .unwrap_or_else(|| "none".to_owned())
                    ))
                })
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn permanent_cli_has_no_scope_or_branch_selector() {
        let command = Cli::command();
        for forbidden in ["scope", "branch", "branch_id"] {
            assert!(!command
                .get_arguments()
                .any(|argument| argument.get_id() == forbidden));
        }
    }
}
