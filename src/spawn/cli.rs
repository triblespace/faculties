//! Explicit Spawn CLI: brief from a file or stdin, ambient spawner, waiting.
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use triblespace::prelude::Id;

use super::operations::{describe_end, render_list, render_show};
use super::{
    inbox_failure, CoverWish, DeclareRequest, Exit, FeedRequest, GoalRequest, InboxHook,
    InboxRequest, LaunchConfig, Launched, PlaceWish, Provider, RunRequest, SettleRequest, Spawn,
    ZooidWish,
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
        /// SPAWN_MISTRAL_MAX_PRICE), agy (opt-in; the Antigravity CLI on this
        /// host's Google login, its own HOME per zooid, no compaction),
        /// auto (most quota headroom; never mistral or agy) or local
        /// (this host's SPAWN_LOCAL_EXECUTOR argv template, with {prompt}, {message},
        /// {name}, {effort}, {script}, {runtime}, {brief}, {spawner} and {resume} (where its archived history ends)
        /// replaced; refused without it, never a hosted
        /// fallback; SPAWN_LOCAL_MAX_RUNTIME, in seconds, optionally bounds the unit). A local
        /// zooid ends with an optional closing message, not a report.
        #[arg(long, default_value = "claude")]
        provider: String,
        /// The cover's wished share of the worker's context window, in
        /// percent, for memories, beliefs and goals together (e.g. 20). Best
        /// effort: goals render whole, so the cover may come out
        /// larger; the sizing is printed and recorded. Without it the cover
        /// uses 240,000 journal characters, and up to 40,000 of Knowledge readings
        /// beside them.
        #[arg(long, value_name = "PERCENT")]
        cover: Option<f64>,
        /// The worker model's context window in tokens, before any other
        /// source (the provider's configuration, a window an earlier run
        /// recorded). Only used with --cover.
        #[arg(long, value_name = "TOKENS", requires = "cover")]
        window: Option<u64>,
        /// The zooid's name, for one that has none stored yet: a persona
        /// label (lowercase letters, digits and dashes) instead of the
        /// brief's proquints. An existing persona of that label is the
        /// zooid's. A zooid that has a name keeps it; a different one refuses.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// The persona outlives each run: settlement never retires it, the
        /// launch refuses only while the zooid's unit runs, and a fresh life
        /// wakes into a window's wake (never a smaller cover; refused with
        /// --cover) once its previous life is archived. An agy resident is
        /// fed colony news between turns. `zooid declare --rest` is the
        /// durable stop; nothing here starts the next life.
        #[arg(long, conflicts_with = "cover")]
        resident: bool,
    },
    /// Launch a zooid for a goal: the brief is rendered from the
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
        /// SPAWN_MISTRAL_MAX_PRICE), agy (opt-in; the Antigravity CLI on this
        /// host's Google login, its own HOME per zooid, no compaction),
        /// auto (most quota headroom; never mistral or agy) or local
        /// (this host's SPAWN_LOCAL_EXECUTOR argv template, with {prompt}, {message},
        /// {name}, {effort}, {script}, {runtime}, {brief}, {spawner} and {resume} (where its archived history ends)
        /// replaced; refused without it, never a hosted
        /// fallback; SPAWN_LOCAL_MAX_RUNTIME, in seconds, optionally bounds the unit). A local
        /// zooid ends with an optional closing message, not a report.
        #[arg(long, default_value = "claude")]
        provider: String,
        /// The cover's wished share of the worker's context window, in
        /// percent, for memories, beliefs and goals together (e.g. 20). Best
        /// effort: goals render whole, so the cover may come out
        /// larger; the sizing is printed and recorded. Without it the cover
        /// uses 240,000 journal characters, and up to 40,000 of Knowledge readings
        /// beside them.
        #[arg(long, value_name = "PERCENT")]
        cover: Option<f64>,
        /// The worker model's context window in tokens, before any other
        /// source (the provider's configuration, a window an earlier run
        /// recorded). Only used with --cover.
        #[arg(long, value_name = "TOKENS", requires = "cover")]
        window: Option<u64>,
        /// The zooid's name, for one that has none stored yet: a persona
        /// label (lowercase letters, digits and dashes) instead of the
        /// brief's proquints. An existing persona of that label is the
        /// zooid's. A zooid that has a name keeps it; a different one refuses.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// The persona outlives each run: settlement never retires it, the
        /// launch refuses only while the zooid's unit runs, and a fresh life
        /// wakes into a window's wake (never a smaller cover; refused with
        /// --cover) once its previous life is archived. An agy resident is
        /// fed colony news between turns. `zooid declare --rest` is the
        /// durable stop; nothing here starts the next life.
        #[arg(long, conflicts_with = "cover")]
        resident: bool,
    },
    /// Record where and how a zooid lives: on one host, on any host, or at
    /// rest.
    ///
    /// The declaration follows every one standing here, so it is what the
    /// zooid declares from now on; `zooid show` shows it. It starts, stops and
    /// asks nothing, and it is valid whether or not anything realises it. When
    /// the one standing declaration already says the same place and model,
    /// nothing is written. Recording needs WRITE on the Spawn collection, as
    /// every Spawn fact does; that shows who may record, not who may speak
    /// for a zooid.
    #[command(group(clap::ArgGroup::new("where").required(true).args(["here", "host", "any_host", "rest"])))]
    Declare {
        name: String,
        /// On this host: the key this store signs with.
        #[arg(long)]
        here: bool,
        /// On the host with this node key (64 hex digits).
        #[arg(long, value_name = "KEY")]
        host: Option<String>,
        /// On whichever host takes it up.
        #[arg(long)]
        any_host: bool,
        /// Nowhere: the zooid exists and rests.
        #[arg(long, conflicts_with_all = ["model_spec", "model_label"])]
        rest: bool,
        /// Bind this model (hex id): the immutable entity a native runtime
        /// names it by in the archive, for these exact weights, configuration
        /// and tokenizer. Without it the node chooses.
        #[arg(long, value_name = "ID")]
        model_spec: Option<String>,
        /// A model label to show. It binds nothing.
        #[arg(long, value_name = "TEXT")]
        model_label: Option<String>,
        /// Why, in words.
        #[arg(long, value_name = "TEXT")]
        why: Option<String>,
        /// The persona recording it. Defaults to $PERSONA.
        #[arg(long, value_name = "PERSONA")]
        persona: Option<String>,
    },
    /// Start a resident zooid's next life here, when one follows: one
    /// declaration places it on this host, its newest life realised that
    /// declaration and ended cleanly past its life limit, its conversation
    /// is archived and its unit is not running. Otherwise print why nothing
    /// follows and start nothing. One launch at most per life: asking again,
    /// or twice at once, finds the life followed. A rest, an ordinary end or
    /// a placement newer than the last life never continues.
    Continue {
        name: String,
        /// Only say whether a next life follows; start nothing.
        #[arg(long)]
        check: bool,
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
    /// One zooid, where it declares that it lives, and all its runs, with
    /// the latest report, and what the removed host queue recorded about it.
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
    /// A fed resident's input, run by its unit script into the agent's
    /// stdin: the prompt as the first stream-json user event, then the
    /// persona's news between turns, each item at least once (a failed
    /// flush, or a hook showing the same item at the same moment, can show
    /// it twice), until the life is past
    /// its limit (then the input closes) or the agent is gone. It
    /// acknowledges nothing and writes no Orient receipt.
    Feed {
        name: String,
        /// The run being fed (hex id), as `zooid spawn` wrote it into the script.
        #[arg(long, value_name = "ID")]
        run: String,
        /// Seconds between reads of the news.
        #[arg(long, default_value_t = 15)]
        interval: u64,
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
        /// Drive's recalled-memory character allowance.
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
    if let Some(agy) = super::agy::AgyHook::parse(&hook) {
        return run_agy_hook(workspace, pile, key, name, run, agy);
    }
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

/// One of an agy zooid's hooks: the life limit from its transcript, and its
/// inbox through the same answers a claude zooid's hooks give. Input that
/// cannot be read allows everything and says why in the log: a hook that
/// refused on its own failure would stop a healthy zooid.
fn run_agy_hook(
    workspace: &crate::cli_workspace::WorkspaceArgs,
    pile: PathBuf,
    key: Option<PathBuf>,
    name: String,
    run: String,
    hook: super::agy::AgyHook,
) -> Result<()> {
    use super::agy::AgyHook;
    let mut text = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("zooid inbox: reading the agy hook input failed ({error}); allowing");
        return Ok(());
    }
    let input = match super::agy::hook_input(&text) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("zooid inbox: the agy hook input is not JSON ({error}); allowing");
            return Ok(());
        }
    };
    let config = LaunchConfig::from_home_env()?;
    let life = match super::agy::read_life(
        &config.agy_home(&name),
        input.conversation.as_deref(),
        config.agy_window,
    ) {
        Ok(life) => life,
        Err(error) => {
            eprintln!("zooid inbox: the life limit is not enforced for this call: {error:#}");
            None
        }
    };
    let inbox = || {
        let inbox_hook = match hook {
            AgyHook::Stop if input.continuation_active => return None,
            AgyHook::Stop => InboxHook::Stop,
            _ => InboxHook::Post,
        };
        let answer = (|| -> Result<_> {
            let run = Id::from_hex(run.trim()).ok_or_else(|| anyhow!("invalid run id {run}"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("the clock is before 1970")?
                .as_secs();
            let request = InboxRequest {
                name: name.clone(),
                run,
                hook: inbox_hook,
                stop_hook_active: false,
                now,
            };
            let storage = workspace.storage(pile.clone(), key.clone())?;
            storage.scope_to_exit(|storage| {
                Ok(Spawn::new(storage.clone(), config.clone()).inbox(&request))
            })
        })()
        .unwrap_or_else(|error| inbox_failure(inbox_hook, &error));
        if let Some(warning) = &answer.warning {
            eprintln!("{warning}");
        }
        answer.output
    };
    if let Some(output) = super::agy::hook_answer(hook, life.as_ref(), inbox) {
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
        if let Some(why) = &launched.lexical_fallback {
            out.note(format!(
                "semantic tint unavailable (used the lexical cue): {why}"
            ))?;
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
    if let Command::Feed {
        name,
        run,
        interval,
    } = &command
    {
        // Like a wait it lasts a life: every read opens the pile afresh.
        let run = Id::from_hex(run.trim()).ok_or_else(|| anyhow!("invalid run id {run}"))?;
        let spawn = Spawn::new(storage, LaunchConfig::from_home_env()?);
        let end = spawn.feed(
            &FeedRequest {
                name: name.clone(),
                run,
                interval: Duration::from_secs((*interval).max(1)),
                rounds: None,
            },
            &mut std::io::stdout().lock(),
        )?;
        eprintln!("zooid feed {name}: ended ({end:?})");
        return Ok(());
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
                name,
                resident,
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
                    zooid: ZooidWish {
                        name,
                        resident,
                        ..Default::default()
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
                name,
                resident,
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
                    zooid: ZooidWish {
                        name,
                        resident,
                        ..Default::default()
                    },
                })?;
                print_launched(&launched)
            }
            Command::Colony { personas } => {
                let view = spawn.colony(&super::ColonyOptions { personas })?;
                crate::cli::with_output("zooid", |out| out.text(super::render_colony(&view)))
            }
            Command::Declare {
                name,
                here,
                host,
                any_host,
                rest,
                model_spec,
                model_label,
                why,
                persona,
            } => {
                let persona = persona
                    .or_else(|| std::env::var("PERSONA").ok())
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow!("pass --persona or set PERSONA"))?;
                let declaring = spawn.declare(&DeclareRequest {
                    place: PlaceWish::one_of(here, host.as_deref(), any_host, rest)?,
                    model_spec: model_spec
                        .map(|text| {
                            Id::from_hex(text.trim())
                                .ok_or_else(|| anyhow!("invalid model specification id {text}"))
                        })
                        .transpose()?,
                    model_label,
                    reason: why,
                    persona,
                    name: name.clone(),
                })?;
                crate::cli::with_output("zooid", |out| out.line(declaring.line(&name)))
            }
            Command::Continue { name, check } => {
                use super::life::{Continued, NextLife};
                let text = if check {
                    match spawn.next_life(&name)? {
                        NextLife::Follows(next) => format!(
                            "{} follows on: {}; its next life would realise declaration {:x}.\n",
                            next.name, next.evidence, next.declaration
                        ),
                        NextLife::Nothing { reason } => format!("nothing follows: {reason}\n"),
                    }
                } else {
                    match spawn.continue_life(&name)? {
                        Continued::Started {
                            continuation,
                            launched,
                        } => format!(
                            "{} continues: {}. Run {:x} (unit {}) follows life {:x} and realises \
                             declaration {:x}.\n",
                            continuation.name,
                            continuation.evidence,
                            launched.run,
                            launched.unit,
                            continuation.previous,
                            continuation.declaration
                        ),
                        Continued::Nothing { reason } => {
                            format!("nothing started: {reason}\n")
                        }
                    }
                };
                crate::cli::with_output("zooid", |out| out.text(text))
            }
            Command::List => {
                let spawns = spawn.list()?;
                crate::cli::with_output("zooid", |out| out.text(render_list(&spawns)))
            }
            Command::Show { name, brief } => {
                let view = spawn.show(&name)?;
                crate::cli::with_output("zooid", |out| out.text(render_show(&view, brief)))
            }
            Command::Wait { .. }
            | Command::Feed { .. }
            | Command::Inbox { .. }
            | Command::DriveTask { .. } => {
                unreachable!("handled above")
            }
            Command::Settle {
                name,
                run,
                code,
                status,
                result,
                report,
                usage,
            } => {
                let run =
                    Id::from_hex(run.trim()).ok_or_else(|| anyhow!("invalid run id {run}"))?;
                let exit = Exit::from_systemd(&code, &status, &result);
                let settled = spawn.settle(&SettleRequest {
                    name: name.clone(),
                    run,
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
    fn declare_takes_exactly_one_place_and_no_model_at_rest() {
        let parses = |arguments: &[&str]| {
            Cli::try_parse_from(
                ["zooid", "--pile", "p", "declare", "a-name"]
                    .iter()
                    .chain(arguments),
            )
            .is_ok()
        };
        for good in [
            &["--here"][..],
            &["--any-host"],
            &["--rest", "--why", "tired"],
            &["--host", "00", "--model-spec", "ab", "--model-label", "m"],
        ] {
            assert!(parses(good), "{good:?}");
        }
        for bad in [
            &[][..],
            &["--here", "--rest"],
            &["--host", "00", "--any-host"],
            &["--rest", "--model-spec", "ab"],
            &["--rest", "--model-label", "m"],
        ] {
            assert!(!parses(bad), "{bad:?}");
        }
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
