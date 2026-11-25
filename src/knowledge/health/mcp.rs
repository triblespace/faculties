//! A native read-only MCP lens. Arguments never address server files.
use super::{presentation, KnowledgeHealth as Operations};
use crate::mcp::{decode_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

const EMPTY: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
const TOOLS: &[Tool] = &[
    Tool {name:"knowledge_health", description:"Read research-health counts from one frozen Knowledge frontier. Logical entries and current states are distinct; divergent heads remain visible.", input_schema:EMPTY},
    Tool {name:"knowledge_tag_counts", description:"Count current tag evidence both by state occurrence and logical entry, without choosing a fork winner.", input_schema:EMPTY},
    Tool {name:"knowledge_quality", description:"Show every published or refuted current Knowledge state, including divergent heads.", input_schema:EMPTY},
    Tool {name:"knowledge_hubs", description:"Count uniquely resolved incoming references to active Knowledge entries. Ambiguous and missing selectors are reported separately.", input_schema:r#"{"type":"object","properties":{"top":{"type":"integer","minimum":0,"default":15}},"additionalProperties":false}"#},
    Tool {name:"knowledge_risk", description:"Show current entries citing refuted or audit-warned evidence. Ambiguous references are reported with all candidate entries, not resolved arbitrarily.", input_schema:EMPTY},
    Tool {name:"knowledge_orphans", description:"List Knowledge entries whose every current state has no outgoing references, with stable entry IDs and fork visibility.", input_schema:r#"{"type":"object","properties":{"top":{"type":"integer","minimum":0,"default":20}},"additionalProperties":false}"#},
];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Limit {
    top: Option<usize>,
}
pub struct KnowledgeHealth {
    operations: Operations,
}
impl KnowledgeHealth {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: Operations::with_storage(storage),
        }
    }
}
impl Faculty for KnowledgeHealth {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "knowledge_hubs" | "knowledge_orphans" => {
                let args: Limit = decode_arguments(arguments)?;
                if name == "knowledge_hubs" {
                    presentation::hubs(&self.operations.hubs(args.top.unwrap_or(15))?, out)
                } else {
                    presentation::orphans(
                        &self.operations.orphans(args.top.unwrap_or(20))?,
                        false,
                        out,
                    )
                }
            }
            "knowledge_health"
            | "knowledge_tag_counts"
            | "knowledge_quality"
            | "knowledge_risk" => {
                let _: Empty = decode_arguments(arguments)?;
                match name {
                    "knowledge_health" => presentation::health(&self.operations.health()?, out),
                    "knowledge_tag_counts" => presentation::tags(&self.operations.tags()?, out),
                    "knowledge_quality" => presentation::quality(&self.operations.quality()?, out),
                    _ => presentation::risk(&self.operations.risk()?, out),
                }
            }
            _ => bail!("Knowledge health MCP has no tool {name:?}"),
        }
    }
}
