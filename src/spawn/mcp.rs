//! Zooid MCP: launch from brief text, declare where a zooid lives, read what
//! was spawned. No waiting tool:
//! a call must be finite, and a launched zooid messages its spawner itself.

use std::path::PathBuf;

use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use triblespace::prelude::Id;

use super::operations::{render_list, render_show};
use super::{
    CoverWish, DeclareRequest, GoalRequest, LaunchConfig, Launched, PlaceWish, Provider, RunRequest,
};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;

const TOOLS: &[Tool] = &[
    Tool {
        name: "zooid_spawn",
        description: "Start a task zooid on the MCP server's host: a headless agent session (Claude by default) in its own systemd user unit that outlives the caller. The brief text is the prompt, verbatim, and names the zooid (its hash as two proquints); an identical brief relaunches the same zooid once its last run settled, and refuses while it is still running. The zooid wakes into the shared self plus recollection chosen for the brief. When it exits, its report is recorded and the spawner is messaged by the zooid. Returns the zooid's name.",
        input_schema: r#"{"type":"object","properties":{"brief":{"type":"string","minLength":1,"description":"The whole brief as text."},"workdir":{"type":"string","minLength":1,"description":"Directory on the MCP server's host the zooid starts in."},"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]},"spawner":{"type":"string","minLength":1,"description":"The spawning persona (label or id), messaged when the zooid ends."},"provider":{"type":"string","enum":["claude","codex","mistral","auto","local"],"description":"The agent. mistral runs Mistral Large 4 through vibe and bills money per token, capped per run; it is only used when named. auto picks whichever of claude and codex the latest fresh quota readings show with the most headroom, never mistral; local runs the host's own executor, configured by SPAWN_LOCAL_EXECUTOR (an argv template with {prompt}, {message}, {name}, {effort}, {script}, {runtime}, {brief} (the zooid's first input: who she is and the brief, without the wake), {spawner} (the spawning persona) and {resume}, the archived node the zooid's history ends at or empty; it reads the prompt on stdin and may write the zooid's closing message to the {message} file, which is optional), and is refused on a host without one, never falling back to a hosted model; SPAWN_LOCAL_MAX_RUNTIME (seconds) optionally bounds a local unit, and a run stopped by it settles as a failure. Default claude."},"cover_percent":{"type":"number","exclusiveMinimum":0,"maximum":100,"description":"The cover's wished share of the worker's context window, in percent, for memories, beliefs and goals together (e.g. 20). Ranked Knowledge pages have an allowance of their own beside the journal's budget. Goals render whole. Sizing is returned and recorded. Default: 240,000 journal characters, and up to 40,000 of Knowledge readings."},"window":{"type":"integer","minimum":1,"description":"The worker model's context window in tokens, before any other source. Only used with cover_percent."}},"required":["brief","workdir","spawner"],"additionalProperties":false}"#,
    },
    Tool {
        name: "zooid_goal",
        description: "Start a task zooid for a goal, like zooid_spawn, with the brief rendered from the goal's current revision: its title, its parent chain, its latest notes, and the zooid's authority to note, move and check off that goal itself. The spawn records the goal and the revision, and the zooid notes its launch on the goal; nothing moves the goal when the zooid exits. A new note on the goal makes a new brief and so a new zooid. Returns the zooid's name.",
        input_schema: r#"{"type":"object","properties":{"goal":{"type":"string","minLength":1,"description":"The goal id or a unique prefix."},"workdir":{"type":"string","minLength":1,"description":"Directory on the MCP server's host the zooid starts in."},"extra":{"type":"string","description":"The spawner's own instructions, appended to the rendered brief."},"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]},"spawner":{"type":"string","minLength":1,"description":"The spawning persona (label or id), messaged when the zooid ends."},"provider":{"type":"string","enum":["claude","codex","mistral","auto","local"]},"cover_percent":{"type":"number","exclusiveMinimum":0,"maximum":100,"description":"The cover's wished share of the worker's context window, in percent, for memories, beliefs and goals together (e.g. 20). Ranked Knowledge pages have an allowance of their own beside the journal's budget. Goals render whole. Sizing is returned and recorded. Default: 240,000 journal characters, and up to 40,000 of Knowledge readings."},"window":{"type":"integer","minimum":1,"description":"The worker model's context window in tokens, before any other source. Only used with cover_percent."}},"required":["goal","workdir","spawner"],"additionalProperties":false}"#,
    },
    Tool {
        name: "zooid_declare",
        description: "Record where and how a zooid lives: here (the MCP server's host), on one host, on any host, or at rest. Name exactly one of them. The declaration follows every one standing in this observation; zooid_show shows it. It starts, stops and asks nothing, and is valid whether or not anything realises it. When the one standing declaration already says the same place and model, nothing is written. persona is who records it; a zooid may declare for itself. Recording needs WRITE on the Spawn collection, as every Spawn fact does: that shows who may record, not who may speak for a zooid.",
        input_schema: r#"{"type":"object","properties":{"name":{"type":"string","minLength":1,"description":"The zooid's name."},"persona":{"type":"string","minLength":1,"description":"The active persona recording it (label or id). No PERSONA fallback."},"here":{"type":"boolean","description":"On the MCP server's host."},"host":{"type":"string","minLength":1,"description":"On the host with this node key (64 hex digits)."},"any_host":{"type":"boolean","description":"On whichever host takes it up."},"rest":{"type":"boolean","description":"Nowhere: the zooid exists and rests. Binds no model."},"model_spec":{"type":"string","minLength":1,"description":"Bind this model (hex id): the immutable entity a native runtime names it by in the archive, for these exact weights, configuration and tokenizer. Without it the node chooses."},"model_label":{"type":"string","minLength":1,"description":"A model label to show. It binds nothing."},"why":{"type":"string","minLength":1}},"required":["name","persona"],"additionalProperties":false}"#,
    },
    Tool {
        name: "zooid_list",
        description: "Every spawned zooid, one line each: name, number of runs, how the latest ended, the brief's first line.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    Tool {
        name: "zooid_show",
        description: "One spawned zooid by name, where it declares that it lives, and all its runs: start and end, exit, unit, workdir, persona, spawner, and the latest report; and any past request of the removed host queue, with its outcomes.",
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
    if let Some(why) = &launched.unprefixed {
        line.push_str(&format!("\n{why}"));
    }
    if let Some(why) = &launched.lexical_fallback {
        line.push_str(&format!(
            "\nsemantic tint unavailable (used the lexical cue): {why}"
        ));
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
struct Declare {
    name: String,
    persona: String,
    #[serde(default)]
    here: bool,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    any_host: bool,
    #[serde(default)]
    rest: bool,
    #[serde(default)]
    model_spec: Option<String>,
    #[serde(default)]
    model_label: Option<String>,
    #[serde(default)]
    why: Option<String>,
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

    /// The launch layout is read from `$HOME` when `zooid_spawn` is called,
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
            "zooid_spawn" => {
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
            "zooid_goal" => {
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
            "zooid_declare" => {
                let args: Declare = decode_arguments(arguments)?;
                let place =
                    PlaceWish::one_of(args.here, args.host.as_deref(), args.any_host, args.rest)
                        .map_err(|error| invalid_arguments(format!("{error:#}")))?;
                let model_spec = args
                    .model_spec
                    .map(|text| {
                        Id::from_hex(text.trim()).ok_or_else(|| {
                            invalid_arguments(format!("invalid model specification id {text}"))
                        })
                    })
                    .transpose()?;
                let declaring = self.operations()?.declare(&DeclareRequest {
                    name: args.name.clone(),
                    place,
                    model_spec,
                    model_label: args.model_label,
                    reason: args.why,
                    persona: args.persona,
                })?;
                out.line(declaring.line(&args.name))
            }
            "zooid_list" => {
                let Empty {} = decode_arguments(arguments)?;
                out.text(render_list(&self.operations()?.list()?))
            }
            "zooid_show" => {
                let args: Show = decode_arguments(arguments)?;
                out.text(render_show(
                    &self.operations()?.show(&args.name)?,
                    args.brief,
                ))
            }
            _ => bail!("Zooid MCP has no tool {name:?}"),
        }
    }
}
