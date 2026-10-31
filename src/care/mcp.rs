//! Care MCP tools: literal arguments; no host files, stdin or environment.

use super::{render, AddInput, Care as Operations};
use crate::mcp::{decode_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

const TOOLS: &[Tool] = &[
    Tool {
        name: "care_add",
        description: "Record one health observation, sealed to this node's key through Secrets. Subject is a Relations label/alias or exact person id. kind: systolic, diastolic, heart-rate, temperature, urine, intake, pain (or LOINC code). unit: mm[Hg], /min, Cel, mL, {score}. method: measured-device, photo-estimate, reported-by-subject, administered. Times are UTC YYYY-MM-DDTHH:MM[:SS], or A..B when approximate; reported defaults to now. Blood pressure is two observations sharing observed and source. Records only; no interpretation.",
        input_schema: r#"{"type":"object","properties":{"subject":{"type":"string"},"kind":{"type":"string"},"value":{"type":"number"},"low":{"type":"number"},"high":{"type":"number"},"unit":{"type":"string"},"observed":{"type":"string"},"reported":{"type":"string"},"method":{"type":"string"},"message":{"type":"string"},"note":{"type":"string"},"supersedes":{"type":"array","items":{"type":"string"}}},"required":["subject","kind","value","unit","observed","method"],"additionalProperties":false}"#,
    },
    Tool {
        name: "care_table",
        description: "Show a subject's readable health observations, newest correction of each, marked [corrected] when it corrects an earlier one. Display only.",
        input_schema: r#"{"type":"object","properties":{"subject":{"type":"string"},"since":{"type":"string"},"kind":{"type":"string"}},"required":["subject"],"additionalProperties":false}"#,
    },
    Tool {
        name: "care_grant",
        description: "Share a subject's health observations (optionally one kind) with another key through Secrets group grants, then deliver their keys. to is an Ed25519 public key as 64 hex characters. Later readings in the same groups reach that key at the next delivery maintenance.",
        input_schema: r#"{"type":"object","properties":{"subject":{"type":"string"},"to":{"type":"string"},"kind":{"type":"string"}},"required":["subject","to"],"additionalProperties":false}"#,
    },
    Tool {
        name: "care_export",
        description: "Export a subject's readable health observations as CSV for a doctor (newest correction of each). Display only.",
        input_schema: r#"{"type":"object","properties":{"subject":{"type":"string"},"since":{"type":"string"},"kind":{"type":"string"}},"required":["subject"],"additionalProperties":false}"#,
    },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddArguments {
    subject: String,
    kind: String,
    value: f64,
    #[serde(default)]
    low: Option<f64>,
    #[serde(default)]
    high: Option<f64>,
    unit: String,
    observed: String,
    #[serde(default)]
    reported: Option<String>,
    method: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    supersedes: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TableArguments {
    subject: String,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantArguments {
    subject: String,
    to: String,
    #[serde(default)]
    kind: Option<String>,
}

pub struct Care {
    operations: Operations,
}
impl Care {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: Operations::with_storage(storage),
        }
    }
}
impl Faculty for Care {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, output: &mut Out<'_>) -> Result<()> {
        match name {
            "care_add" => {
                let args: AddArguments = decode_arguments(arguments)?;
                let added = self.operations.add(&AddInput {
                    subject: args.subject,
                    kind: args.kind,
                    value: args.value,
                    low: args.low,
                    high: args.high,
                    unit: args.unit,
                    observed: args.observed,
                    reported: args.reported,
                    method: args.method,
                    photo: None,
                    message: args.message,
                    note: args.note,
                    supersedes: args.supersedes,
                })?;
                render::add(&added, output)
            }
            "care_table" => {
                let args: TableArguments = decode_arguments(arguments)?;
                render::table(
                    &self.operations.table(
                        &args.subject,
                        args.since.as_deref(),
                        args.kind.as_deref(),
                    )?,
                    output,
                )
            }
            "care_grant" => {
                let args: GrantArguments = decode_arguments(arguments)?;
                render::grant(
                    &self
                        .operations
                        .grant(&args.subject, &args.to, args.kind.as_deref())?,
                    output,
                )
            }
            "care_export" => {
                let args: TableArguments = decode_arguments(arguments)?;
                render::export_csv(
                    &self.operations.table(
                        &args.subject,
                        args.since.as_deref(),
                        args.kind.as_deref(),
                    )?,
                    output,
                )
            }
            other => bail!("Care MCP has no tool {other:?}"),
        }
    }
}
