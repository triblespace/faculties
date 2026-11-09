use super::{Options, DEFAULT_LIMIT};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;

pub struct Search {
    operations: super::Search,
}

impl Search {
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: super::Search::with_storage(storage),
        }
    }
}

static TOOLS: &[Tool] = &[Tool {
    name: "search",
    description: "One lexical query across the selected workspace's Wiki, Memory and Files. Returns JSON grouped by source, exact collection, native selector, method and availability. Wiki searches current titles/bodies; Memory uses BM25 on each chunk's least summary handle; Files searches names/media types/tags only, not file bodies. Limit applies per source after identical-reading deduplication. No shared relevance rank, model or semantic index. Each source has its own frozen observation, not an atomic cross-source snapshot.",
    input_schema: r#"{"type":"object","properties":{"query":{"type":"string","minLength":1},"limit_per_source":{"type":"integer","minimum":1,"default":10}},"required":["query"],"additionalProperties":false}"#,
}];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    query: String,
    #[serde(default = "default_limit")]
    limit_per_source: usize,
}

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

impl Faculty for Search {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }

    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        if name != "search" {
            bail!("unknown Search tool {name}");
        }
        let args: Query = decode_arguments(arguments)?;
        let options = Options {
            query: &args.query,
            limit_per_source: args.limit_per_source,
        };
        options
            .validate()
            .map_err(|error| invalid_arguments(error.to_string()))?;
        out.text(self.operations.query(options)?.json()?)
    }
}
