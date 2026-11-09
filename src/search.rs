//! One lexical request over the caller's Wiki, Memory and Files bindings.
//!
//! Results are request-local projections, not a second catalog. Each source
//! observes its own frozen view; this is not an atomic cross-source snapshot.
//! No model is discovered, and no semantic index is required or maintained.

pub mod cli;
pub mod mcp;
#[cfg(test)]
mod tests;

use anyhow::{ensure, Result};
use serde::Serialize;
use triblespace::core::collection::CollectionHandle;
use triblespace::prelude::Id;

use crate::storage::Storage;

pub const DEFAULT_LIMIT: usize = 10;

/// The exact source selected by a faculty alongside its deduplicated readings.
/// This value lives only for the request which asked for it.
pub struct SourceRead<T> {
    pub collection: CollectionHandle,
    pub hits: Vec<T>,
}

#[derive(Clone, Debug)]
pub struct Search {
    storage: Storage,
}

#[derive(Clone, Copy, Debug)]
pub struct Options<'a> {
    pub query: &'a str,
    /// A limit on readings within each source, never a shared relevance rank.
    pub limit_per_source: usize,
}

impl Options<'_> {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.query.trim().is_empty(),
            "search requires a nonblank query"
        );
        ensure!(
            self.limit_per_source > 0,
            "limit_per_source must be positive"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub query: String,
    /// The launcher's exact selected workspace revision, when one was pinned.
    pub workspace_revision: Option<String>,
    pub limit_per_source: usize,
    pub sources: Vec<Source>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub role: &'static str,
    /// Configured root on failure; actual resolved root on success. None means
    /// the root could not be resolved, not that another root was searched.
    pub collection: Option<String>,
    pub method: &'static str,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Answered {
        total_matches: usize,
        truncated: bool,
        hits: Vec<Hit>,
    },
    /// These searches require complete selected facts and required payloads.
    /// They do not claim to return a partial source when preparation fails.
    Unavailable { message: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", content = "reading", rename_all = "snake_case")]
pub enum Hit {
    Wiki(crate::wiki::operations::SearchHit),
    Memory(crate::memory::operations::SearchHit),
    Files(crate::files::operations::SearchHit),
}

/// Native operation arguments, scoped by the enclosing collection/workspace.
/// A URI alone would lose both that scope and Wiki's exact-revision choice.
/// Opening an entity/revision can expose additional current readings of that
/// identity. The explicit matched content handles pin the bytes searched;
/// these opening arguments do not promise replay of the search snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", content = "arguments")]
pub enum OpenTarget {
    #[serde(rename = "wiki_show")]
    Wiki { id: String, exact: bool },
    #[serde(rename = "memory_show")]
    Memory { selector: String },
    #[serde(rename = "files_get")]
    Files { id: String },
}

impl Search {
    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    pub fn query(&self, options: Options<'_>) -> Result<Report> {
        options.validate()?;
        // One scoped owner across the request, with independent source reads.
        self.storage.scope(|storage| {
            let wiki = crate::wiki::Wiki::with_storage(storage.clone());
            let memory = crate::memory::Memory::with_storage(storage.clone());
            let files = crate::files::Files::with_storage(storage.clone());
            let sources = vec![
                source(
                    storage,
                    crate::schemas::wiki::DEFAULT_SCOPE_ID,
                    "wiki",
                    "ascii_substring_current_title_and_body",
                    options.limit_per_source,
                    || wiki.search_hits(options.query, false, false),
                    Hit::Wiki,
                ),
                source(
                    storage,
                    crate::schemas::memory::DEFAULT_SCOPE_ID,
                    "memory",
                    "bm25_least_summary_handle_per_chunk",
                    options.limit_per_source,
                    || memory.search_hits(options.query),
                    Hit::Memory,
                ),
                source(
                    storage,
                    crate::schemas::files::DEFAULT_SCOPE_ID,
                    "files",
                    "substring_name_media_type_and_tags_only",
                    options.limit_per_source,
                    || files.search_hits(options.query),
                    Hit::Files,
                ),
            ];
            Ok(Report {
                query: options.query.to_owned(),
                workspace_revision: storage.selected_workspace().map(|w| format!("{:x}", w.id)),
                limit_per_source: options.limit_per_source,
                sources,
            })
        })
    }
}

fn source<T>(
    storage: &Storage,
    scope: Id,
    role: &'static str,
    method: &'static str,
    limit: usize,
    read: impl FnOnce() -> Result<SourceRead<T>>,
    hit: impl Fn(T) -> Hit,
) -> Source {
    let mut collection = None;
    let result = storage.collection_handle(scope).and_then(|selected| {
        collection = selected.map(|handle| hex::encode(handle.raw));
        read()
    });
    let outcome = match result {
        Ok(read) => {
            collection = Some(hex::encode(read.collection.raw));
            let total_matches = read.hits.len();
            Outcome::Answered {
                total_matches,
                truncated: total_matches > limit,
                hits: read.hits.into_iter().take(limit).map(hit).collect(),
            }
        }
        Err(error) => Outcome::Unavailable {
            message: format!("{error:#}"),
        },
    };
    Source {
        role,
        collection,
        method,
        outcome,
    }
}

impl Report {
    /// CLI and MCP carry the same explicit, source-labelled result contract.
    pub fn json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}
