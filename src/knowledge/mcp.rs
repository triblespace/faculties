//! Explicit resident Knowledge MCP tools. Caller content is literal, never @file or
//! stdin syntax. Pile/key configuration belongs to the trusted launcher.
//!
//! This is a local faculty adapter, not a hosted execution sandbox. Create/edit
//! retain normal Typst validation (a world which rejects external files), and
//! index/similar bind the WeMM model the launcher names. Untrusted hosted deployments
//! still need independent CPU/memory limits and model-execution policy.

use std::path::PathBuf;

use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use triblespace::prelude::Id;

use super::operations::{ImportDocument, Knowledge as Operations, ListOptions};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;

pub struct Knowledge {
    operations: Operations,
}

impl Knowledge {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }

    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: Operations::with_storage(storage),
        }
    }
}

const EMPTY: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
const ID: &str = r#"{"type":"object","properties":{"id":{"type":"string","description":"Revision id or unambiguous prefix"}},"required":["id"],"additionalProperties":false}"#;
const READ: &str = r#"{"type":"object","properties":{"id":{"type":"string"},"exact":{"type":"boolean","default":false}},"required":["id"],"additionalProperties":false}"#;
const TAG: &str = r#"{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"}},"required":["id","name"],"additionalProperties":false}"#;
const TOOLS: &[Tool] = &[
    Tool { name: "knowledge_create", description: "Create a revision from literal title/content and optional tags. Performs normal Typst/link validation; content is not a host path.", input_schema: r#"{"type":"object","properties":{"title":{"type":"string"},"content":{"type":"string"},"tags":{"type":"array","items":{"type":"string"},"default":[]},"force":{"type":"boolean","default":false}},"required":["title","content"],"additionalProperties":false}"# },
    Tool { name: "knowledge_edit", description: "Join the complete current frontier with a successor. Omitted fields inherit only when the frontier agrees; tags omitted or empty inherit existing tags. Content is literal and validated.", input_schema: r#"{"type":"object","properties":{"id":{"type":"string"},"content":{"type":"string"},"title":{"type":"string"},"tags":{"type":"array","items":{"type":"string"},"default":[]},"force":{"type":"boolean","default":false}},"required":["id"],"additionalProperties":false}"# },
    Tool { name: "knowledge_show", description: "Display the current frontier, including every fork head. exact=true pins the named immutable revision.", input_schema: READ },
    Tool { name: "knowledge_export", description: "Export exact stored UTF-8 content bytes as a binary resource, without headers or added newlines. Follows the current frontier unless exact=true; an unresolved fork is an error. Never writes a host path.", input_schema: READ },
    Tool { name: "knowledge_diff", description: "Compare revisions at one-based causal-history positions; defaults to the last two.", input_schema: r#"{"type":"object","properties":{"id":{"type":"string"},"from":{"type":"integer","minimum":1},"to":{"type":"integer","minimum":1}},"required":["id"],"additionalProperties":false}"# },
    Tool { name: "knowledge_archive", description: "Publish an archived-tag successor without deleting history.", input_schema: ID },
    Tool { name: "knowledge_restore", description: "Publish a successor without the archived tag.", input_schema: ID },
    Tool { name: "knowledge_revert", description: "Publish a successor using a one-based historical revision; never rewrites old content.", input_schema: r#"{"type":"object","properties":{"id":{"type":"string"},"to":{"type":"integer","minimum":1}},"required":["id","to"],"additionalProperties":false}"# },
    Tool { name: "knowledge_links", description: "Inspect one entry's citations or audit all frontier links. strict reports archived-target breakage as an error; unwritten targets are forward references.", input_schema: r#"{"type":"object","properties":{"id":{"type":"string"},"top":{"type":"integer","minimum":0,"default":15},"strict":{"type":"boolean","default":false}},"additionalProperties":false}"# },
    Tool { name: "knowledge_list", description: "List current entries with optional tag/backlink filters; all includes archived entries.", input_schema: r#"{"type":"object","properties":{"tags":{"type":"array","items":{"type":"string"},"default":[]},"with_backlink_tag":{"type":"array","items":{"type":"string"},"default":[]},"without_backlink_tag":{"type":"array","items":{"type":"string"},"default":[]},"with_backlink_type":{"type":"array","items":{"type":"string"},"default":[]},"without_backlink_type":{"type":"array","items":{"type":"string"},"default":[]},"all":{"type":"boolean","default":false}},"additionalProperties":false}"# },
    Tool { name: "knowledge_history", description: "Inspect causal revision history with all predecessor/frontier identities.", input_schema: ID },
    Tool { name: "knowledge_tag_add", description: "Add a tag by publishing a successor when necessary.", input_schema: TAG },
    Tool { name: "knowledge_tag_remove", description: "Remove a tag by publishing a successor when necessary.", input_schema: TAG },
    Tool { name: "knowledge_tag_list", description: "List known tags and revision counts.", input_schema: EMPTY },
    Tool { name: "knowledge_tag_mint", description: "Create or find a content-derived named tag and return its ID.", input_schema: r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}"# },
    Tool { name: "knowledge_search", description: "Search current revision titles/content with optional matching-line context.", input_schema: r#"{"type":"object","properties":{"query":{"type":"string"},"context":{"type":"boolean","default":false},"all":{"type":"boolean","default":false}},"required":["query"],"additionalProperties":false}"# },
    Tool { name: "knowledge_similar", description: "Rank current revisions by meaning in the one WeMM space. Requires the wemm build on a GB10 and binds the model per call. `faculties maintain` derives the rows in the background; a partial index says so.", input_schema: r#"{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}"# },
    Tool { name: "knowledge_check", description: "Report live-frontier link integrity without requesting optional whole-corpus Typst compilation.", input_schema: EMPTY },
    Tool { name: "knowledge_lint", description: "Report markup/reference normalization; fix publishes successors, check returns an error if changes are needed. Exact revision citations remain pinned.", input_schema: r#"{"type":"object","properties":{"fix":{"type":"boolean","default":false},"check":{"type":"boolean","default":false}},"additionalProperties":false}"# },
    Tool { name: "knowledge_fix_truncated", description: "Resolve literal scheme:prefix lines and report expansions/errors. Does not read a local file or stdin and does not edit stored content.", input_schema: r#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false}"# },
    Tool { name: "knowledge_import", description: "Import resident text documents in one publication. Titles are fallback labels, not paths; a Typst '= ' heading takes precedence. Validates content and permits forward references.", input_schema: r#"{"type":"object","properties":{"documents":{"type":"array","items":{"type":"object","properties":{"title":{"type":"string"},"content":{"type":"string"}},"required":["title","content"],"additionalProperties":false}},"tags":{"type":"array","items":{"type":"string"},"default":[]}},"required":["documents"],"additionalProperties":false}"# },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selector {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    id: String,
    #[serde(default)]
    exact: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    title: String,
    content: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    force: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    id: String,
    content: Option<String>,
    title: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    force: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Diff {
    id: String,
    from: Option<usize>,
    to: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revert {
    id: String,
    to: usize,
}
fn default_top() -> usize {
    15
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Links {
    id: Option<String>,
    #[serde(default = "default_top")]
    top: usize,
    #[serde(default)]
    strict: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    with_backlink_tag: Vec<String>,
    #[serde(default)]
    without_backlink_tag: Vec<String>,
    #[serde(default)]
    with_backlink_type: Vec<String>,
    #[serde(default)]
    without_backlink_type: Vec<String>,
    #[serde(default)]
    all: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tag {
    id: String,
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Name {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    #[serde(default)]
    context: bool,
    #[serde(default)]
    all: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    query: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lint {
    #[serde(default)]
    fix: bool,
    #[serde(default)]
    check: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    input: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    title: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Import {
    #[serde(deserialize_with = "crate::mcp::object::vec")]
    documents: Vec<Document>,
    #[serde(default)]
    tags: Vec<String>,
}

fn revision(out: &mut Out<'_>, id: Id) -> Result<()> {
    out.line(format!("revision {id:x}"))
}
fn tag_change(out: &mut Out<'_>, id: Option<Id>, name: &str, add: bool) -> Result<()> {
    match id {
        Some(id) => revision(out, id),
        None => out.line(format!(
            "already {} #{}",
            if add { "tagged" } else { "untagged" },
            name.trim().to_ascii_lowercase()
        )),
    }
}

impl Faculty for Knowledge {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }

    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        let knowledge = &self.operations;
        match name {
            "knowledge_create" => {
                let args: Create = decode_arguments(arguments)?;
                revision(
                    out,
                    knowledge.create(&args.title, &args.content, &args.tags, args.force)?,
                )
            }
            "knowledge_edit" => {
                let args: Edit = decode_arguments(arguments)?;
                revision(
                    out,
                    knowledge.edit(
                        &args.id,
                        args.content.as_deref(),
                        args.title.as_deref(),
                        &args.tags,
                        args.force,
                    )?,
                )
            }
            "knowledge_show" => {
                let args: Read = decode_arguments(arguments)?;
                out.text(knowledge.show(&args.id, args.exact)?)
            }
            "knowledge_export" => {
                let args: Read = decode_arguments(arguments)?;
                let export = knowledge.export(&args.id, args.exact)?;
                let uri = export.uri();
                out.blob(export.bytes, "text/plain; charset=utf-8", uri)
            }
            "knowledge_diff" => {
                let args: Diff = decode_arguments(arguments)?;
                if args.from == Some(0) || args.to == Some(0) {
                    return Err(invalid_arguments("revision positions are one-based"));
                }
                out.text(knowledge.diff(&args.id, args.from, args.to)?)
            }
            "knowledge_archive" => {
                let args: Selector = decode_arguments(arguments)?;
                tag_change(out, knowledge.archive(&args.id)?, "archived", true)
            }
            "knowledge_restore" => {
                let args: Selector = decode_arguments(arguments)?;
                tag_change(out, knowledge.restore(&args.id)?, "archived", false)
            }
            "knowledge_revert" => {
                let args: Revert = decode_arguments(arguments)?;
                if args.to == 0 {
                    return Err(invalid_arguments("revision positions are one-based"));
                }
                revision(out, knowledge.revert(&args.id, args.to)?)
            }
            "knowledge_links" => {
                let args: Links = decode_arguments(arguments)?;
                knowledge.links(args.id.as_deref(), args.top, args.strict, out)
            }
            "knowledge_list" => {
                let args: List = decode_arguments(arguments)?;
                out.text(knowledge.list(&ListOptions {
                    tags: args.tags,
                    with_backlink_tag: args.with_backlink_tag,
                    without_backlink_tag: args.without_backlink_tag,
                    with_backlink_type: args.with_backlink_type,
                    without_backlink_type: args.without_backlink_type,
                    all: args.all,
                })?)
            }
            "knowledge_history" => {
                let args: Selector = decode_arguments(arguments)?;
                out.text(knowledge.history(&args.id)?)
            }
            "knowledge_tag_add" | "knowledge_tag_remove" => {
                let args: Tag = decode_arguments(arguments)?;
                let add = name == "knowledge_tag_add";
                tag_change(
                    out,
                    knowledge.tag(&args.id, &args.name, add)?,
                    &args.name,
                    add,
                )
            }
            "knowledge_tag_list" => {
                let _: Empty = decode_arguments(arguments)?;
                knowledge.tags(out)
            }
            "knowledge_tag_mint" => {
                let args: Name = decode_arguments(arguments)?;
                out.line(format!(
                    "{:x}  {}",
                    knowledge.mint_tag(&args.name)?,
                    args.name.trim().to_ascii_lowercase()
                ))
            }
            "knowledge_search" => {
                let args: Search = decode_arguments(arguments)?;
                out.text(knowledge.search(&args.query, args.context, args.all)?)
            }
            "knowledge_similar" => {
                let args: Query = decode_arguments(arguments)?;
                out.text(knowledge.similar(&args.query)?)
            }
            "knowledge_check" => {
                let _: Empty = decode_arguments(arguments)?;
                knowledge.check(false, out)
            }
            "knowledge_lint" => {
                let args: Lint = decode_arguments(arguments)?;
                knowledge.lint(args.fix, args.check, out)
            }
            "knowledge_fix_truncated" => {
                let args: Input = decode_arguments(arguments)?;
                knowledge.fix_truncated(&args.input, out)
            }
            "knowledge_import" => {
                let args: Import = decode_arguments(arguments)?;
                let documents = args
                    .documents
                    .into_iter()
                    .map(|document| ImportDocument {
                        title: document.title,
                        content: document.content,
                    })
                    .collect();
                for id in knowledge.import_texts(documents, &args.tags)? {
                    revision(out, id)?;
                }
                Ok(())
            }
            _ => bail!("unknown Knowledge MCP tool {name:?}"),
        }
    }
}
