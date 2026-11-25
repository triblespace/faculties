//! Explicit finite read-only MCP inspections over the reusable Diagnostics API.
use super::InspectOptions;
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

pub struct Diagnostics {
    operations: super::Diagnostics,
}
impl Diagnostics {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: super::Diagnostics::with_storage(storage),
        }
    }
}
static TOOLS: &[Tool] = &[
Tool { name: "diagnostics_scan", description: "Inspect queues, unresolved Profiles/Relations states, unread messages and loop heuristics from one frozen maintained snapshot.", input_schema: r#"{"type":"object","properties":{"recent":{"type":"integer","minimum":0,"default":40},"loop_min":{"type":"integer","minimum":0,"default":3},"stale_min":{"type":"integer","minimum":0,"default":15}},"required":[],"additionalProperties":false}"# },
Tool { name: "diagnostics_loops", description: "Inspect recent execution attempts and repeated failure patterns; never executes or retries a command.", input_schema: r#"{"type":"object","properties":{"recent":{"type":"integer","minimum":0,"default":40},"min_repeat":{"type":"integer","minimum":0,"default":3}},"required":[],"additionalProperties":false}"# },
Tool { name: "diagnostics_timeline", description: "Inspect interleaved exec/model/reason activity newest first; zero recent yields no rows.", input_schema: r#"{"type":"object","properties":{"recent":{"type":"integer","minimum":0,"default":80}},"required":[],"additionalProperties":false}"# },
Tool { name: "diagnostics_cover", description: "Inspect all canonical Memory episodes and context budget in ID order, not the density recollection sampler.", input_schema: r#"{"type":"object","properties":{"full":{"type":"boolean","default":false}},"required":[],"additionalProperties":false}"# },
Tool { name: "diagnostics_chunk", description: "Inspect every canonical memory/alias matching an id prefix, preserving overlapping episode identity.", input_schema: r#"{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}"# },
Tool { name: "diagnostics_turn", description: "Inspect a recent turn cycle without model or command execution. turn is one-based.", input_schema: r#"{"type":"object","properties":{"turn":{"type":"integer","minimum":1,"default":1},"full":{"type":"boolean","default":false}},"required":[],"additionalProperties":false}"# },
Tool { name: "diagnostics_context", description: "Inspect every context candidate for a recent turn. raw produces reconstructed JSON text, not an original-byte export; text is never interpreted as host paths.", input_schema: r#"{"type":"object","properties":{"turn":{"type":"integer","minimum":1,"default":1},"full":{"type":"boolean","default":false},"raw":{"type":"boolean","default":false}},"required":[],"additionalProperties":false}"# },
];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scan {
    recent: Option<usize>,
    loop_min: Option<usize>,
    stale_min: Option<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Loops {
    recent: Option<usize>,
    min_repeat: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Timeline {
    recent: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cover {
    full: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chunk {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    turn: Option<usize>,
    full: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextArgs {
    turn: Option<usize>,
    full: Option<bool>,
    raw: Option<bool>,
}
fn turn(value: Option<usize>) -> Result<usize> {
    let value = value.unwrap_or(1);
    if value == 0 {
        return Err(invalid_arguments("turn is one-based"));
    }
    Ok(value)
}
impl Faculty for Diagnostics {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        let diagnostics = &self.operations;
        match name {
            "diagnostics_scan" => {
                let args: Scan = decode_arguments(arguments)?;
                let options = InspectOptions {
                    recent: args.recent.unwrap_or(40),
                    loop_min: args.loop_min.unwrap_or(3),
                    stale_min: args.stale_min.unwrap_or(15),
                };
                if options.stale_min < 0 {
                    return Err(invalid_arguments("stale_min must be nonnegative"));
                }
                diagnostics.scan(&options, out)
            }
            "diagnostics_loops" => {
                let args: Loops = decode_arguments(arguments)?;
                diagnostics.loops(args.recent.unwrap_or(40), args.min_repeat.unwrap_or(3), out)
            }
            "diagnostics_timeline" => {
                let args: Timeline = decode_arguments(arguments)?;
                diagnostics.timeline(args.recent.unwrap_or(80), out)
            }
            "diagnostics_cover" => {
                let args: Cover = decode_arguments(arguments)?;
                diagnostics.cover(args.full.unwrap_or(false), out)
            }
            "diagnostics_chunk" => {
                let args: Chunk = decode_arguments(arguments)?;
                diagnostics.chunk(&args.id, out)
            }
            "diagnostics_turn" => {
                let args: Turn = decode_arguments(arguments)?;
                diagnostics.turn(turn(args.turn)?, args.full.unwrap_or(false), out)
            }
            "diagnostics_context" => {
                let args: ContextArgs = decode_arguments(arguments)?;
                diagnostics.context(
                    turn(args.turn)?,
                    args.full.unwrap_or(false),
                    args.raw.unwrap_or(false),
                    out,
                )
            }
            _ => bail!("unknown Diagnostics MCP tool {name}"),
        }
    }
}
