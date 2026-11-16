//! Spawn MCP: launch from brief text, read what was spawned. No waiting tool:
//! a call must be finite, and a launched zooid messages its spawner itself.

use std::path::PathBuf;

use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;

use super::operations::{render_list, render_show};
use super::{GoalRequest, LaunchConfig, Launched, Provider, RunRequest};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;

const TOOLS: &[Tool] = &[
    Tool {
        name: "spawn_run",
        description: "Start a task zooid on the MCP server's host: a headless Claude session in its own systemd user unit that outlives the caller. The brief text is the prompt, verbatim, and names the zooid (its hash as two proquints); an identical brief relaunches the same zooid once its last run settled, and refuses while it is still running. The zooid wakes into the shared self plus recollection chosen for the brief. When it exits, its report is recorded and the spawner is messaged by the zooid. Returns the zooid's name.",
        input_schema: r#"{"type":"object","properties":{"brief":{"type":"string","minLength":1,"description":"The whole brief as text."},"workdir":{"type":"string","minLength":1,"description":"Directory on the MCP server's host the zooid starts in."},"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]},"spawner":{"type":"string","minLength":1,"description":"The spawning persona (label or id), messaged when the zooid ends."},"provider":{"type":"string","enum":["claude","codex","auto","local"],"description":"The agent. auto picks the one the latest fresh quota readings show with the most headroom; local is not available yet. Default claude."}},"required":["brief","workdir","spawner"],"additionalProperties":false}"#,
    },
    Tool {
        name: "spawn_goal",
        description: "Start a task zooid for a Compass goal, like spawn_run, with the brief rendered from the goal's current revision: its title, its parent chain, its latest notes, and the zooid's authority to note, move and check off that goal itself. The spawn records the goal and the revision, and the zooid notes its launch on the goal; nothing moves the goal when the zooid exits. A new note on the goal makes a new brief and so a new zooid. Returns the zooid's name.",
        input_schema: r#"{"type":"object","properties":{"goal":{"type":"string","minLength":1,"description":"The goal id or a unique prefix."},"workdir":{"type":"string","minLength":1,"description":"Directory on the MCP server's host the zooid starts in."},"extra":{"type":"string","description":"The spawner's own instructions, appended to the rendered brief."},"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]},"spawner":{"type":"string","minLength":1,"description":"The spawning persona (label or id), messaged when the zooid ends."},"provider":{"type":"string","enum":["claude","codex","auto","local"]}},"required":["goal","workdir","spawner"],"additionalProperties":false}"#,
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
    if let Some(why) = &launched.untinted {
        line.push_str(&format!("\nrecollection untinted: {why}"));
    }
    for warning in &launched.warnings {
        line.push_str(&format!("\nwarning: {warning}"));
    }
    line
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
                })?;
                out.line(launched_line(&launched))
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
