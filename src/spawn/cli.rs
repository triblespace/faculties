//! Explicit Spawn CLI: brief from a file or stdin, ambient spawner, waiting.
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use triblespace::prelude::Id;

use super::operations::{describe_end, render_list, render_pass, render_requests, render_show};
use super::{
    inbox_failure, keep_lock_path, CoverWish, Exit, GoalRequest, InboxHook, InboxRequest,
    KeepConfig, LaunchConfig, Launched, Provider, RequestInput, RunRequest, SettleRequest, Spawn,
    ZooidRef,
};

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "zooid",
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
    Spawn {
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
        /// The agent: claude, codex, mistral (opt-in; bills money, capped by
        /// SPAWN_MISTRAL_MAX_PRICE), auto (most quota headroom; never mistral) or local
        /// (this host's SPAWN_LOCAL_EXECUTOR argv template, with {prompt}, {message},
        /// {name}, {effort}, {script}, {runtime}, {brief}, {spawner} and {resume} (where its archived history ends)
        /// replaced; refused without it, never a hosted
        /// fallback; SPAWN_LOCAL_MAX_RUNTIME, in seconds, optionally bounds the unit, and
        /// SPAWN_LOCAL_MEMORY_MAX replaces its 24G MemoryMax). A local
        /// zooid ends with an optional closing message, not a report.
        #[arg(long, default_value = "claude")]
        provider: String,
        /// The cover's wished share of the worker's context window, in
        /// percent, for memories, beliefs and goals together (e.g. 20). Best
        /// effort: beliefs and goals render whole, so the cover may come out
        /// larger; the sizing is printed and recorded. Without it the cover
        /// is the plain one (240,000 memory characters).
        #[arg(long, value_name = "PERCENT")]
        cover: Option<f64>,
        /// The worker model's context window in tokens, before any other
        /// source (the provider's configuration, a window an earlier run
        /// recorded). Only used with --cover.
        #[arg(long, value_name = "TOKENS", requires = "cover")]
        window: Option<u64>,
    },
    /// Launch a zooid for a Compass goal: the brief is rendered from the
    /// goal's title, its parent chain and its latest notes, plus the zooid's
    /// authority to note, move and check off the goal itself.
    ///
    /// The spawn records the goal revision, and the zooid notes its launch
    /// on the goal. Nothing moves the goal when the zooid exits.
    Goal {
        /// The goal id, or a unique prefix of it.
        goal: String,
        /// Directory the zooid starts in.
        #[arg(long, value_name = "DIR")]
        workdir: PathBuf,
        /// Your own instructions, appended to the rendered brief: a file, or
        /// `-` for stdin.
        #[arg(long, value_name = "PATH")]
        extra: Option<String>,
        /// Effort: low, medium, high, xhigh or max.
        #[arg(long, default_value = "high")]
        effort: String,
        /// The spawning persona, messaged when the zooid ends. Defaults to $PERSONA.
        #[arg(long, value_name = "PERSONA")]
        spawner: Option<String>,
        /// The agent: claude, codex, mistral (opt-in; bills money, capped by
        /// SPAWN_MISTRAL_MAX_PRICE), auto (most quota headroom; never mistral) or local
        /// (this host's SPAWN_LOCAL_EXECUTOR argv template, with {prompt}, {message},
        /// {name}, {effort}, {script}, {runtime}, {brief}, {spawner} and {resume} (where its archived history ends)
        /// replaced; refused without it, never a hosted
        /// fallback; SPAWN_LOCAL_MAX_RUNTIME, in seconds, optionally bounds the unit, and
        /// SPAWN_LOCAL_MEMORY_MAX replaces its 24G MemoryMax). A local
        /// zooid ends with an optional closing message, not a report.
        #[arg(long, default_value = "claude")]
        provider: String,
        /// The cover's wished share of the worker's context window, in
        /// percent, for memories, beliefs and goals together (e.g. 20). Best
        /// effort: beliefs and goals render whole, so the cover may come out
        /// larger; the sizing is printed and recorded. Without it the cover
        /// is the plain one (240,000 memory characters).
        #[arg(long, value_name = "PERCENT")]
        cover: Option<f64>,
        /// The worker model's context window in tokens, before any other
        /// source (the provider's configuration, a window an earlier run
        /// recorded). Only used with --cover.
        #[arg(long, value_name = "TOKENS", requires = "cover")]
        window: Option<u64>,
    },
    /// Ask one host's keeper to start a run of a zooid there: writes a run
    /// request into the spawn collection, which syncs like any other. The
    /// request names no workdir, command or budget; the host decides those.
    Request {
        /// An existing zooid, by name.
        #[arg(long, value_name = "NAME", required_unless_present = "brief")]
        zooid: Option<String>,
        /// A new zooid's brief: a file, or `-` for stdin.
        #[arg(long, value_name = "PATH", conflicts_with = "zooid")]
        brief: Option<String>,
        /// The target host's node key (64 hex digits). Defaults to this host.
        #[arg(long, value_name = "KEY")]
        host: Option<String>,
        /// A readable name beside the key. Defaults to this host's name when
        /// --host is omitted.
        #[arg(long)]
        label: Option<String>,
        /// The agent: claude, codex, mistral (opt-in; bills money), auto or local
        /// (refused by a target host without SPAWN_LOCAL_EXECUTOR).
        #[arg(long, default_value = "claude")]
        provider: String,
        /// Effort: low, medium, high, xhigh or max.
        #[arg(long, default_value = "high")]
        effort: String,
        /// The asking persona, messaged when the run ends. Defaults to $PERSONA.
        #[arg(long, value_name = "PERSONA")]
        spawner: Option<String>,
        /// The run this one continues (hex id), when it is a move.
        #[arg(long, value_name = "RUN")]
        previous: Option<String>,
        /// Text delivered once after the brief: a file, or `-` for stdin.
        /// This keeper refuses a request that carries one: there is no
        /// disclosure check yet.
        #[arg(long, value_name = "PATH")]
        handoff: Option<String>,
        /// The cover's wished share of the worker's context window, in
        /// percent (best effort). The target host finds the window.
        #[arg(long, value_name = "PERCENT")]
        cover: Option<f64>,
    },
    /// Every run request with its outcomes.
    Requests {
        /// Only those addressed to this host.
        #[arg(long)]
        here: bool,
    },
    /// This host's keeper: start the runs requested of this host, and
    /// record accepted, running, refused and task-terminal for each.
    /// A run's unit is named from its request, so a request is started once.
    Keep {
        /// One pass, then exit.
        #[arg(long)]
        once: bool,
        /// The directory every run starts in. Defaults to $HOME.
        #[arg(long, value_name = "DIR", env = "SPAWN_KEEP_WORKDIR")]
        workdir: Option<PathBuf>,
        /// Seconds between passes, without --once.
        #[arg(long, default_value_t = 60)]
        poll: u64,
    },
    /// Every zooid, one line each.
    List,
    /// Each parent persona: its doing goals, the zooids it started that have
    /// no end recorded, finished reports it has not read, open ask-jp goals
    /// and when it was last seen. Read from existing records; a persona with
    /// no current evidence is `unknown`, never idle.
    Colony {
        /// Also show this persona (label, alias or id), beside every spawner
        /// and every resident zooid (affinity zooid, not retired, not a
        /// spawned run's own persona).
        #[arg(long = "persona", value_name = "PERSONA")]
        personas: Vec<String>,
    },
    /// One zooid and all its runs, with the latest report.
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
    /// zooid, then retire the persona. Safe to run again after a failure: an
    /// end or message already recorded is not repeated. The flags take
    /// systemd's EXIT_CODE, EXIT_STATUS and SERVICE_RESULT.
    Settle {
        name: String,
        /// The run being settled (hex id), as `zooid spawn` wrote it into the script.
        #[arg(long, value_name = "ID")]
        run: String,
        /// The request the run serves (hex id), when a keeper started it.
        /// Settlement links the run to it, so a keeper that stopped before
        /// publishing the run record still sees that the request ran.
        #[arg(long, value_name = "ID")]
        request: Option<String>,
        /// `exited`, `killed` or `dumped` (systemd's EXIT_CODE).
        #[arg(long, default_value = "")]
        code: String,
        /// Exit number or signal name (systemd's EXIT_STATUS).
        #[arg(long, default_value = "")]
        status: String,
        /// systemd's SERVICE_RESULT, e.g. `success` or `oom-kill`.
        #[arg(long, default_value = "")]
        result: String,
        /// The run's report file: a hosted run's final reply, or a local
        /// run's optional closing message. Missing or empty records none.
        #[arg(long, value_name = "PATH")]
        report: Option<PathBuf>,
        /// vibe's export.json, for a mistral run: its usage and cost are
        /// recorded with the end.
        #[arg(long, value_name = "PATH")]
        usage: Option<PathBuf>,
    },
    /// A local executor that launches one zooid into Drive's living loop,
    /// named in SPAWN_LOCAL_EXECUTOR as
    /// `zooid drive-task --binding FILE --brief {brief} --spawner {spawner}
    /// --name {name} --resume={resume}`. It turns the run's pinned
    /// TRIBLESPACE_COLLECTION_* routes into Drive's --native-collection
    /// arguments, takes everything else from the binding file, and execs
    /// `drive run --mind mini`. It opens no pile. A route variable naming no
    /// faculties collection, a malformed or repeated route, a missing archive
    /// route, a missing or extra binding key, or an empty brief or one over
    /// the bound refuses before Drive starts. A role Drive does not accept is
    /// refused by Drive itself, naming it. The zooid runs until it is
    /// interrupted: spawn stops a local unit with SIGINT, Drive's teardown.
    DriveTask {
        /// The host's binding file (JSON): drive, tenant, home, model,
        /// window, telemetry and provider. Optional memory-chars overrides
        /// Drive's recalled-memory character allowance; optional mini-vision
        /// and organ ({key, peers, port}) are Drive's --mini-vision and
        /// --organ-key/--organ-peer/--organ-port.
        #[arg(long, value_name = "PATH")]
        binding: PathBuf,
        /// The run's brief file, from {brief}: her first input on a fresh
        /// start and the wake's tint, passed to Drive untouched.
        #[arg(long, value_name = "PATH")]
        brief: PathBuf,
        /// Who sent the brief, from {spawner}: Drive's --brief-from.
        #[arg(long)]
        spawner: String,
        /// The zooid's name, Drive's native persona.
        #[arg(long)]
        name: String,
        /// The archived node the zooid's history ends at, from {resume}
        /// (`--resume={resume}`): Drive's --native-resume. Empty starts the
        /// zooid fresh. Omitted, no history is continued either.
        #[arg(long, value_name = "NODE", allow_hyphen_values = false)]
        resume: Option<String>,
    },
    /// A claude zooid's inbox hook, run by its unit script as `inbox stop` or
    /// `inbox post`: reads the hook's JSON on stdin and prints the answer.
    /// Stop blocks the end once while messages are unread; post shows new
    /// unread messages at most once a minute. Neither acknowledges anything.
    Inbox {
        name: String,
        /// The run whose hook this is (hex id), as `zooid spawn` wrote it into
        /// the script. Post's throttle and shown set are per run.
        #[arg(long, value_name = "ID")]
        run: String,
        /// `stop` or `post`.
        #[arg(long)]
        hook: String,
    },
}

/// Answer an inbox hook. Whatever fails, it exits 0 with the answer the
/// hook needs: a stop that cannot read the inbox blocks once and says why,
/// a post logs the failure to stderr (the run's log) and prints nothing.
fn run_inbox(
    workspace: &crate::cli_workspace::WorkspaceArgs,
    pile: PathBuf,
    key: Option<PathBuf>,
    name: String,
    run: String,
    hook: String,
) -> Result<()> {
    let hook = InboxHook::parse(&hook)?;
    let mut input = String::new();
    let stop_hook_active = match std::io::stdin().read_to_string(&mut input) {
        Ok(_) => match serde_json::from_str::<serde_json::Value>(&input) {
            Ok(value) => value["stop_hook_active"].as_bool().unwrap_or(false),
            Err(error) => {
                // Whether this stop follows our own block is unknown, so it
                // is never blocked on: a block here could repeat until the
                // agent's own cap.
                eprintln!("zooid inbox: the hook input is not JSON ({error}); allowing");
                return Ok(());
            }
        },
        Err(error) => {
            eprintln!("zooid inbox: reading the hook input failed ({error}); allowing");
            return Ok(());
        }
    };
    let answer = (|| -> Result<_> {
        let run = Id::from_hex(run.trim()).ok_or_else(|| anyhow!("invalid run id {run}"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("the clock is before 1970")?
            .as_secs();
        let request = InboxRequest {
            name,
            run,
            hook,
            stop_hook_active,
            now,
        };
        if hook == InboxHook::Stop && stop_hook_active {
            return Ok(super::InboxAnswer::default());
        }
        let storage = workspace.storage(pile, key)?;
        storage.scope_to_exit(|storage| {
            Ok(Spawn::new(storage.clone(), LaunchConfig::from_home_env()?).inbox(&request))
        })
    })()
    .unwrap_or_else(|error| inbox_failure(hook, &error));
    if let Some(warning) = &answer.warning {
        eprintln!("{warning}");
    }
    if let Some(output) = &answer.output {
        println!("{output}");
    }
    Ok(())
}

fn spawner_or_persona(spawner: Option<String>) -> Result<String> {
    spawner
        .or_else(|| std::env::var("PERSONA").ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("pass --spawner or set PERSONA"))
}

fn print_launched(launched: &Launched) -> Result<()> {
    crate::cli::with_output("zooid", |out| {
        out.line(&launched.name)?;
        out.note(format!(
            "started {} (run {:x}, {} prompt chars, {}: {}); report -> {}, log -> {}; \
             `zooid wait {}` blocks until it ends",
            launched.unit,
            launched.run,
            launched.prompt_chars,
            launched.provider.name(),
            launched.provider_reason,
            launched.report.display(),
            launched.log.display(),
            launched.name
        ))?;
        match (&launched.model, &launched.model_unset) {
            (Some(model), _) => out.note(format!("model: {model}"))?,
            (None, Some(why)) => out.note(format!("model: none named; {why}"))?,
            (None, None) => {}
        }
        if let Some(why) = &launched.unprefixed {
            out.note(why)?;
        }
        if let Some(note) = launched.goal_note {
            out.note(format!("noted the launch on the goal: {note:x}"))?;
        }
        if let Some(why) = &launched.untinted {
            out.note(format!("recollection untinted: {why}"))?;
        }
        if let Some(cover) = &launched.cover {
            out.note(format!("cover sizing: {}", cover.summary))?;
        }
        for warning in &launched.warnings {
            out.note(format!("warning: {warning}"))?;
        }
        Ok(())
    })
}

fn keep_config(workdir: Option<PathBuf>) -> Result<KeepConfig> {
    let workdir = match workdir {
        Some(workdir) => workdir,
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?),
    };
    Ok(KeepConfig {
        workdir,
        lock: keep_lock_path()?,
    })
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
    if let Command::DriveTask {
        binding,
        brief,
        spawner,
        name,
        resume,
    } = &command
    {
        // The run's pile and key are Drive's native faculties owner; no
        // store is opened here.
        let key = cli
            .key
            .as_deref()
            .ok_or_else(|| anyhow!("drive-task needs the run's TRIBLESPACE_KEY"))?;
        let routes = super::drive_task::routes_from_env()?;
        let resume = super::drive_task::parse_resume(resume.as_deref().unwrap_or(""))?;
        let never = super::drive_task::exec(
            binding,
            &super::drive_task::RunInputs {
                brief,
                spawner,
                name,
                routes: &routes,
                pile: &cli.pile,
                key,
                resume,
            },
        )?;
        match never {}
    }
    if let Command::Inbox { name, run, hook } = command {
        return run_inbox(&cli.workspace, cli.pile, cli.key, name, run, hook);
    }
    let storage = cli.workspace.storage(cli.pile, cli.key)?;
    if let Command::Keep {
        once: false,
        workdir,
        poll,
    } = &command
    {
        // A pass opens the pile afresh, so the loop holds no store between.
        let spawn = Spawn::new(storage, LaunchConfig::from_home_env()?);
        let config = keep_config(workdir.clone())?;
        spawn.keep(
            &config,
            Duration::from_secs((*poll).max(1)),
            |pass| match pass {
                Ok(pass) => print!("{}", render_pass(pass)),
                Err(error) => eprintln!("keep pass failed: {error:#}"),
            },
        );
    }
    if let Command::Wait { name, poll } = &command {
        // Each check opens the pile afresh, so a wait that lasts hours keeps
        // no store open and reads the settlement another process wrote.
        let spawn = Spawn::new(storage, LaunchConfig::from_home_env()?);
        let view = spawn.wait(name, Duration::from_secs((*poll).max(1)))?;
        let run = view.runs.last().expect("wait found a run");
        let mut text = format!("{} ended: {}\n", view.name, describe_end(run));
        text.push_str(&render_show(&view, false));
        return crate::cli::with_output("zooid", |out| out.text(text));
    }
    storage.scope_to_exit(|storage| {
        let spawn = Spawn::new(storage.clone(), LaunchConfig::from_home_env()?);
        match command {
            Command::Spawn {
                brief,
                workdir,
                effort,
                spawner,
                provider,
                cover,
                window,
            } => {
                let launched = spawn.run(&RunRequest {
                    brief: read_brief(&brief)?,
                    workdir,
                    effort,
                    spawner: spawner_or_persona(spawner)?,
                    provider: Provider::parse(&provider)?,
                    cover: CoverWish {
                        percent: cover,
                        window,
                    },
                })?;
                print_launched(&launched)
            }
            Command::Goal {
                goal,
                workdir,
                extra,
                effort,
                spawner,
                provider,
                cover,
                window,
            } => {
                let launched = spawn.goal(&GoalRequest {
                    goal,
                    extra: extra.as_deref().map(read_brief).transpose()?,
                    workdir,
                    effort,
                    spawner: spawner_or_persona(spawner)?,
                    provider: Provider::parse(&provider)?,
                    cover: CoverWish {
                        percent: cover,
                        window,
                    },
                })?;
                print_launched(&launched)
            }
            Command::Request {
                zooid,
                brief,
                host,
                label,
                provider,
                effort,
                spawner,
                previous,
                handoff,
                cover,
            } => {
                let zooid = match (zooid, brief) {
                    (Some(name), None) => ZooidRef::Name(name),
                    (None, Some(brief)) => ZooidRef::Brief(read_brief(&brief)?),
                    _ => return Err(anyhow!("pass --zooid or --brief")),
                };
                let label = match (&host, label) {
                    (_, Some(label)) => Some(label),
                    (None, None) => std::fs::read_to_string("/etc/hostname")
                        .ok()
                        .map(|name| name.trim().to_owned())
                        .filter(|name| !name.is_empty()),
                    (Some(_), None) => None,
                };
                let requested = spawn.request(&RequestInput {
                    zooid,
                    host: host.as_deref().map(super::parse_host).transpose()?,
                    host_label: label,
                    provider: Provider::parse(&provider)?,
                    effort,
                    spawner: spawner_or_persona(spawner)?,
                    previous: previous
                        .map(|run| {
                            Id::from_hex(run.trim()).ok_or_else(|| anyhow!("invalid run id {run}"))
                        })
                        .transpose()?,
                    handoff: handoff.as_deref().map(read_brief).transpose()?,
                    cover_percent: cover,
                })?;
                crate::cli::with_output("zooid", |out| {
                    out.line(format!("{:x}", requested.request))?;
                    out.note(format!(
                        "requested {} on host {}{}; its keeper starts it in {}",
                        requested.name,
                        super::host_text(requested.host),
                        requested
                            .host_label
                            .as_deref()
                            .map(|label| format!(" ({label})"))
                            .unwrap_or_default(),
                        requested.unit
                    ))?;
                    if let Some(why) = &requested.unprefixed {
                        out.note(why)?;
                    }
                    Ok(())
                })
            }
            Command::Requests { here } => {
                let host = if here {
                    Some(spawn.local_host()?)
                } else {
                    None
                };
                let requests = spawn.requests(host)?;
                crate::cli::with_output("zooid", |out| out.text(render_requests(&requests)))
            }
            Command::Keep { workdir, .. } => {
                let pass = spawn.keep_once(&keep_config(workdir)?)?;
                crate::cli::with_output("zooid", |out| out.text(render_pass(&pass)))
            }
            Command::Colony { personas } => {
                let view = spawn.colony(&super::ColonyOptions { personas })?;
                crate::cli::with_output("zooid", |out| out.text(super::render_colony(&view)))
            }
            Command::List => {
                let spawns = spawn.list()?;
                crate::cli::with_output("zooid", |out| out.text(render_list(&spawns)))
            }
            Command::Show { name, brief } => {
                let view = spawn.show(&name)?;
                crate::cli::with_output("zooid", |out| out.text(render_show(&view, brief)))
            }
            Command::Wait { .. } | Command::Inbox { .. } | Command::DriveTask { .. } => {
                unreachable!("handled above")
            }
            Command::Settle {
                name,
                run,
                request,
                code,
                status,
                result,
                report,
                usage,
            } => {
                let run =
                    Id::from_hex(run.trim()).ok_or_else(|| anyhow!("invalid run id {run}"))?;
                let request = request
                    .map(|text| {
                        Id::from_hex(text.trim())
                            .ok_or_else(|| anyhow!("invalid request id {text}"))
                    })
                    .transpose()?;
                let exit = Exit::from_systemd(&code, &status, &result);
                let settled = spawn.settle(&SettleRequest {
                    name: name.clone(),
                    run,
                    request,
                    exit: exit.clone(),
                    report,
                    usage,
                })?;
                crate::cli::with_output("zooid", |out| {
                    out.line(format!(
                        "settled {name} run {:x}: {}{}, report {}{}",
                        settled.run,
                        exit.describe(),
                        if settled.already_ended {
                            " (end already recorded)"
                        } else {
                            ""
                        },
                        settled
                            .report
                            .map(super::handle_text)
                            .unwrap_or_else(|| "none".to_owned()),
                        if settled.already_messaged {
                            ", spawner already messaged"
                        } else {
                            ""
                        }
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
