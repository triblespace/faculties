//! Standing intentions with resident inputs and explicit predicate evaluation.

use super::{render, DeclaredState, Habits as Operations};
use crate::mcp::{decode_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

const EMPTY: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
const SELECTOR: &str = r#"{"type":"object","properties":{"habit":{"type":"string"}},"required":["habit"],"additionalProperties":false}"#;
const TOOLS: &[Tool] = &[
    Tool { name: "habit_add", description: "Store an immutable standing intention from literal text and an optional carried script, given as at most one of literal script text, a Files script_file_id, or an absolute script_path on the host the MCP server runs on (read with the server process's permissions, which are a capability, not authority: pass only paths the user or task authorizes, and workspace selection authorizes none). This does not execute the predicate or script. Optional personas are literal labels or exact IDs; omitted or empty personas notify everyone. A label is not unique; replacements require explicit supersedes IDs.", input_schema: r#"{"type":"object","properties":{"label":{"type":"string"},"when":{"type":"string"},"nudge":{"type":"string"},"script":{"type":"string","description":"Literal script text."},"script_file_id":{"type":"string","pattern":"^[0-9a-fA-F]{32}$","description":"Exact 32-hex Files entity ID or single-file import ID of the script."},"script_path":{"type":"string","description":"Absolute path of the script on the MCP server's host."},"supersedes":{"type":"array","items":{"type":"string"},"default":[]},"personas":{"type":"array","items":{"type":"string"},"default":[]}},"required":["label","when","nudge"],"additionalProperties":false}"# },
    Tool { name: "habit_list", description: "Inspect current definitions and activation without evaluating stored predicates by default. Explicit evaluate_conditions=true executes stored shell predicates on the server, as the CLI list command does. Opt-in when-result-v1 predicates may report Deferred, distinct from false or failure.", input_schema: r#"{"type":"object","properties":{"evaluate_conditions":{"type":"boolean","default":false}},"additionalProperties":false}"# },
    Tool { name: "habit_show", description: "Inspect an exact definition or unambiguous label, including historical superseded definitions. Does not evaluate predicates.", input_schema: SELECTOR },
    Tool { name: "habit_due", description: "Explicitly evaluate stored standing-intention predicates on the server and return confirmed due intentions plus neutral Deferred notices. Predicates can execute carried scripts or shell commands; failed evaluations are reported, not silently treated as not due. Success means inspection succeeded, not that all verdicts are known. when-result-v1 opts into typed JSON success; ordinary when exit semantics are unchanged.", input_schema: EMPTY },
    Tool { name: "habit_done", description: "Publish a completion occurrence for one live intention by label or ID.", input_schema: SELECTOR },
    Tool { name: "habit_pause", description: "Publish paused activation, reconciling all observed state heads without changing the definition.", input_schema: SELECTOR },
    Tool { name: "habit_resume", description: "Publish active activation, reconciling all observed state heads without changing the definition.", input_schema: SELECTOR },
    Tool { name: "habit_check", description: "Explicitly read the complete Habit collection and its resident attachments, without executing carried predicates. Reports counts, plus notes on facts readers skip or wait for (a completion before its definition, two values for one attribute); it never calls the collection invalid.", input_schema: EMPTY },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Add {
    label: String,
    when: String,
    nudge: String,
    script: Option<String>,
    script_file_id: Option<String>,
    script_path: Option<String>,
    #[serde(default)]
    supersedes: Vec<String>,
    #[serde(default)]
    personas: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    #[serde(default)]
    evaluate_conditions: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selector {
    habit: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

pub struct Habits {
    operations: Operations,
    files: crate::files::operations::Files,
}
impl Habits {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: Operations::with_storage(storage.clone()),
            files: crate::files::operations::Files::with_storage(storage),
        }
    }
}
impl Faculty for Habits {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "habit_add" => {
                let args: Add = decode_arguments(arguments)?;
                let script = match (&args.script, &args.script_file_id, &args.script_path) {
                    (None, None, None) => None,
                    _ => Some(
                        match crate::mcp::input::exactly_one([
                            ("script", args.script.as_deref()),
                            ("script_file_id", args.script_file_id.as_deref()),
                            ("script_path", args.script_path.as_deref()),
                        ])? {
                            ("script", text) => Bytes::from(text.to_owned()),
                            ("script_file_id", id) => crate::mcp::input::InputBudget::per_call()
                                .file_id(&self.files, id)?,
                            (_, path) => {
                                crate::mcp::input::InputBudget::per_call().host_file(path)?
                            }
                        },
                    ),
                };
                render::added(
                    &self.operations.add(
                        &args.label,
                        &args.when,
                        &args.nudge,
                        script.as_deref(),
                        &args.supersedes,
                        &args.personas,
                    )?,
                    out,
                )
            }
            "habit_list" | "habit_due" => {
                let due = name == "habit_due";
                let evaluate = if due {
                    let _: Empty = decode_arguments(arguments)?;
                    true
                } else {
                    let args: List = decode_arguments(arguments)?;
                    args.evaluate_conditions
                };
                let listing = render::listed(&self.operations.list(evaluate)?, due);
                out.text(listing.text)?;
                for diagnostic in &listing.diagnostics {
                    out.line(diagnostic)?;
                }
                if !listing.diagnostics.is_empty() {
                    bail!(
                        "{} standing intention(s) could not be evaluated",
                        listing.diagnostics.len()
                    );
                }
                Ok(())
            }
            "habit_show" | "habit_done" | "habit_pause" | "habit_resume" => {
                let args: Selector = decode_arguments(arguments)?;
                match name {
                    "habit_show" => render::shown(&self.operations.show(&args.habit)?, out),
                    "habit_done" => render::completed(&self.operations.done(&args.habit)?, out),
                    _ => render::state_changed(
                        &self.operations.set_state(
                            &args.habit,
                            if name == "habit_pause" {
                                DeclaredState::Paused
                            } else {
                                DeclaredState::Active
                            },
                        )?,
                        out,
                    ),
                }
            }
            "habit_check" => {
                let _: Empty = decode_arguments(arguments)?;
                render::checked(&self.operations.check()?, out)
            }
            _ => bail!("Habit MCP has no tool {name:?}"),
        }
    }
}
