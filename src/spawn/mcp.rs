//! Spawn MCP: launch from brief text, read what was spawned. No waiting tool:
//! a call must be finite, and a launched zooid messages its spawner itself.

use std::path::PathBuf;

use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;

use super::operations::{render_list, render_requests, render_show};
use super::{
    CoverWish, GoalRequest, LaunchConfig, Launched, Provider, RequestInput, RunRequest, ZooidRef,
};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;

const TOOLS: &[Tool] = &[
    Tool {
        name: "spawn_run",
        description: "Start a task zooid on the MCP server's host: a headless agent session (Claude by default) in its own systemd user unit that outlives the caller. The brief text is the prompt, verbatim, and names the zooid (its hash as two proquints); an identical brief relaunches the same zooid once its last run settled, and refuses while it is still running. The zooid wakes into the shared self plus recollection chosen for the brief. When it exits, its report is recorded and the spawner is messaged by the zooid. Returns the zooid's name.",
        input_schema: r#"{"type":"object","properties":{"brief":{"type":"string","minLength":1,"description":"The whole brief as text."},"workdir":{"type":"string","minLength":1,"description":"Directory on the MCP server's host the zooid starts in."},"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]},"spawner":{"type":"string","minLength":1,"description":"The spawning persona (label or id), messaged when the zooid ends."},"provider":{"type":"string","enum":["claude","codex","mistral","auto","local"],"description":"The agent. mistral runs Mistral Large 4 through vibe and bills money per token, capped per run; it is only used when named. auto picks whichever of claude and codex the latest fresh quota readings show with the most headroom, never mistral; local runs the host's own executor, configured by SPAWN_LOCAL_EXECUTOR (an argv template with {prompt}, {message}, {name}, {effort}, {script} and {runtime}; it reads the prompt on stdin and may write the zooid's closing message to the {message} file, which is optional), and is refused on a host without one, never falling back to a hosted model; SPAWN_LOCAL_MAX_RUNTIME (seconds) optionally bounds a local unit, and a run stopped by it settles as a failure. Default claude."},"cover_percent":{"type":"number","exclusiveMinimum":0,"maximum":100,"description":"The cover's wished share of the worker's context window, in percent, for memories, beliefs and goals together (e.g. 20). Best effort: beliefs and goals render whole, so the cover may come out larger; the sizing is returned and recorded. Omitted: the plain cover of 240,000 memory characters."},"window":{"type":"integer","minimum":1,"description":"The worker model's context window in tokens, before any other source. Only used with cover_percent."}},"required":["brief","workdir","spawner"],"additionalProperties":false}"#,
    },
    Tool {
        name: "spawn_goal",
        description: "Start a task zooid for a Compass goal, like spawn_run, with the brief rendered from the goal's current revision: its title, its parent chain, its latest notes, and the zooid's authority to note, move and check off that goal itself. The spawn records the goal and the revision, and the zooid notes its launch on the goal; nothing moves the goal when the zooid exits. A new note on the goal makes a new brief and so a new zooid. Returns the zooid's name.",
        input_schema: r#"{"type":"object","properties":{"goal":{"type":"string","minLength":1,"description":"The goal id or a unique prefix."},"workdir":{"type":"string","minLength":1,"description":"Directory on the MCP server's host the zooid starts in."},"extra":{"type":"string","description":"The spawner's own instructions, appended to the rendered brief."},"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]},"spawner":{"type":"string","minLength":1,"description":"The spawning persona (label or id), messaged when the zooid ends."},"provider":{"type":"string","enum":["claude","codex","mistral","auto","local"]},"cover_percent":{"type":"number","exclusiveMinimum":0,"maximum":100,"description":"The cover's wished share of the worker's context window, in percent, for memories, beliefs and goals together (e.g. 20). Best effort: beliefs and goals render whole, so the cover may come out larger; the sizing is returned and recorded. Omitted: the plain cover of 240,000 memory characters."},"window":{"type":"integer","minimum":1,"description":"The worker model's context window in tokens, before any other source. Only used with cover_percent."}},"required":["goal","workdir","spawner"],"additionalProperties":false}"#,
    },
    Tool {
        name: "spawn_request",
        description: "Ask one host's keeper to start a run of a zooid there, by writing a run request into the spawn collection, which syncs like any other. Name an existing zooid, or give a new brief. The request names no workdir, command or budget: the target host resolves those and records accepted, running, refused or task-terminal. local is refused by a host whose SPAWN_LOCAL_EXECUTOR is unset; nothing falls back to a hosted model. mistral is refused too: it spends money, and a request runs unattended; only spawn_run may choose it. A request with a handoff is refused by today's keeper, which has no disclosure check; every reader of the spawn collection can read a handoff. Returns the request id.",
        input_schema: r#"{"type":"object","properties":{"zooid":{"type":"string","minLength":1,"description":"An existing zooid's name."},"brief":{"type":"string","minLength":1,"description":"A new zooid's whole brief, as text."},"host":{"type":"string","pattern":"^[0-9a-fA-F]{64}$","description":"The target host's node key. Default: the MCP server's host."},"host_label":{"type":"string","minLength":1,"description":"A readable name beside the key."},"provider":{"type":"string","enum":["claude","codex","mistral","auto","local"]},"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]},"spawner":{"type":"string","minLength":1,"description":"The asking persona, messaged when the run ends."},"previous":{"type":"string","description":"The run this one continues (hex id), for a move."},"handoff":{"type":"string","description":"Text delivered once, after the brief."},"cover_percent":{"type":"number","exclusiveMinimum":0,"maximum":100,"description":"The cover's wished share of the worker's context window, in percent (best effort). The target host finds the window."}},"required":["spawner"],"additionalProperties":false}"#,
    },
    Tool {
        name: "spawn_requests",
        description: "Every run request with its outcomes: accepted, running, refused with its reason, task-terminal.",
        input_schema: r#"{"type":"object","properties":{"here":{"type":"boolean","description":"Only requests addressed to the MCP server's host."}},"additionalProperties":false}"#,
    },
    Tool {
        name: "spawn_list",
        description: "Every spawned zooid, one line each: name, number of runs, how the latest ended, the brief's first line.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    Tool {
        name: "spawn_show",
        description: "One spawned zooid by name with all its runs: start and end, exit, unit, workdir, persona, spawner, and the latest report.",
        input_schema: r#"{"type":"object","properties":{"name":{"type":"string","minLength":1},"brief":{"type":"boolean","description":"Also print the brief."}},"required":["name"],"additionalProperties":false}"#,
    },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    brief: String,
    workdir: String,
    #[serde(default)]
    effort: Option<String>,
    spawner: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    cover_percent: Option<f64>,
    #[serde(default)]
    window: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Goal {
    goal: String,
    workdir: String,
    #[serde(default)]
    extra: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    spawner: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    cover_percent: Option<f64>,
    #[serde(default)]
    window: Option<u64>,
}

fn provider(name: Option<&str>) -> Result<Provider> {
    name.map_or(Ok(Provider::Claude), |name| {
        Provider::parse(name).map_err(|error| invalid_arguments(format!("{error:#}")))
    })
}

fn launched_line(launched: &Launched) -> String {
    let mut line = format!(
        "{} started in {} (run {:x}, {}: {}); it messages the spawner when it ends",
        launched.name,
        launched.unit,
        launched.run,
        launched.provider.name(),
        launched.provider_reason
    );
    match (&launched.model, &launched.model_unset) {
        (Some(model), _) => line.push_str(&format!("\nmodel: {model}")),
        (None, Some(why)) => line.push_str(&format!("\nmodel: none named; {why}")),
        (None, None) => {}
    }
    if let Some(why) = &launched.untinted {
        line.push_str(&format!("\nrecollection untinted: {why}"));
    }
    if let Some(cover) = &launched.cover {
        line.push_str(&format!("\ncover sizing: {}", cover.summary));
    }
    for warning in &launched.warnings {
        line.push_str(&format!("\nwarning: {warning}"));
    }
    line
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(default)]
    zooid: Option<String>,
    #[serde(default)]
    brief: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    host_label: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    spawner: String,
    #[serde(default)]
    previous: Option<String>,
    #[serde(default)]
    handoff: Option<String>,
    #[serde(default)]
    cover_percent: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Requests {
    #[serde(default)]
    here: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Show {
    name: String,
    #[serde(default)]
    brief: bool,
}

pub struct Spawn {
    storage: crate::storage::Storage,
    launch: Option<LaunchConfig>,
}

impl Spawn {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }

    /// The launch layout is read from `$HOME` when `spawn_run` is called,
    /// never at construction or discovery.
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            storage,
            launch: None,
        }
    }

    pub fn with_launch(mut self, launch: LaunchConfig) -> Self {
        self.launch = Some(launch);
        self
    }

    fn operations(&self) -> Result<super::Spawn> {
        let launch = match &self.launch {
            Some(launch) => launch.clone(),
            None => LaunchConfig::from_home_env()?,
        };
        Ok(super::Spawn::new(self.storage.clone(), launch))
    }
}

impl Faculty for Spawn {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }

    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "spawn_run" => {
                let args: Run = decode_arguments(arguments)?;
                if args.brief.trim().is_empty() {
                    return Err(invalid_arguments("brief is empty"));
                }
                let provider = provider(args.provider.as_deref())?;
                let launched = self.operations()?.run(&RunRequest {
                    brief: args.brief,
                    workdir: PathBuf::from(args.workdir),
                    effort: args.effort.unwrap_or_else(|| "high".to_owned()),
                    spawner: args.spawner,
                    provider,
                    cover: CoverWish {
                        percent: args.cover_percent,
                        window: args.window,
                    },
                })?;
                out.line(launched_line(&launched))
            }
            "spawn_goal" => {
                let args: Goal = decode_arguments(arguments)?;
                let provider = provider(args.provider.as_deref())?;
                let launched = self.operations()?.goal(&GoalRequest {
                    goal: args.goal,
                    extra: args.extra,
                    workdir: PathBuf::from(args.workdir),
                    effort: args.effort.unwrap_or_else(|| "high".to_owned()),
                    spawner: args.spawner,
                    provider,
                    cover: CoverWish {
                        percent: args.cover_percent,
                        window: args.window,
                    },
                })?;
                out.line(launched_line(&launched))
            }
            "spawn_request" => {
                let args: Request = decode_arguments(arguments)?;
                let zooid = match (args.zooid, args.brief) {
                    (Some(name), None) => ZooidRef::Name(name),
                    (None, Some(brief)) if !brief.trim().is_empty() => ZooidRef::Brief(brief),
                    _ => return Err(invalid_arguments("give exactly one of zooid and brief")),
                };
                let host = args
                    .host
                    .as_deref()
                    .map(super::parse_host)
                    .transpose()
                    .map_err(|error| invalid_arguments(format!("{error:#}")))?;
                let previous = args
                    .previous
                    .as_deref()
                    .map(|run| {
                        triblespace::prelude::Id::from_hex(run.trim())
                            .ok_or_else(|| invalid_arguments(format!("invalid run id {run}")))
                    })
                    .transpose()?;
                let requested = self.operations()?.request(&RequestInput {
                    zooid,
                    host,
                    host_label: args.host_label,
                    provider: provider(args.provider.as_deref())?,
                    effort: args.effort.unwrap_or_else(|| "high".to_owned()),
                    spawner: args.spawner,
                    previous,
                    handoff: args.handoff,
                    cover_percent: args.cover_percent,
                })?;
                out.line(format!(
                    "request {:x}: {} on host {}{}; its keeper starts it in {}",
                    requested.request,
                    requested.name,
                    super::host_text(requested.host),
                    requested
                        .host_label
                        .as_deref()
                        .map(|label| format!(" ({label})"))
                        .unwrap_or_default(),
                    requested.unit
                ))
            }
            "spawn_requests" => {
                let args: Requests = decode_arguments(arguments)?;
                let spawn = self.operations()?;
                let host = if args.here {
                    Some(spawn.local_host()?)
                } else {
                    None
                };
                out.text(render_requests(&spawn.requests(host)?))
            }
            "spawn_list" => {
                let Empty {} = decode_arguments(arguments)?;
                out.text(render_list(&self.operations()?.list()?))
            }
            "spawn_show" => {
                let args: Show = decode_arguments(arguments)?;
                out.text(render_show(
                    &self.operations()?.show(&args.name)?,
                    args.brief,
                ))
            }
            _ => bail!("Spawn MCP has no tool {name:?}"),
        }
    }
}
