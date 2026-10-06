//! Purpose-built Files MCP tools. No shell paths, stdin markers, or ambient
//! configuration are accepted as tool arguments.

use super::operations::{FetchOptions, Files as Operations, SimilarityOptions};
use super::presentation::ViewOptions;
use crate::mcp::{decode_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde::Deserialize;
use std::path::PathBuf;

pub struct Files {
    operations: Operations,
}

impl Files {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }

    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: Operations::with_storage(storage),
        }
    }
}

const ID_SCHEMA: &str = r#"{"type":"object","properties":{"id":{"type":"string","description":"Stored entity id, content hash, or unambiguous prefix"}},"required":["id"],"additionalProperties":false}"#;
const EMPTY_SCHEMA: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
const TOOLS: &[Tool] = &[
    Tool {
        name: "files_add",
        description: "Import a file from base64-encoded original bytes. Name is a leaf filename, never a server path. Importing never embeds; `files_index` does.",
        input_schema: r#"{"type":"object","properties":{"data":{"type":"string","description":"Base64-encoded file bytes"},"name":{"type":"string"},"mime":{"type":"string"},"tags":{"type":"array","items":{"type":"string"},"default":[]}},"required":["data","name","mime"],"additionalProperties":false}"#,
    },
    Tool {
        name: "files_list",
        description: "List imported files, optionally filtered by tags and MIME prefix.",
        input_schema: r#"{"type":"object","properties":{"tags":{"type":"array","items":{"type":"string"},"default":[]},"mime":{"type":"string"}},"additionalProperties":false}"#,
    },
    Tool { name: "files_show", description: "Inspect metadata for a stored file, directory, or import.", input_schema: ID_SCHEMA },
    Tool {
        name: "files_view",
        description: "Present UTF-8 text, a still image, or accepted audio. Converts supported images to accepted PNG/JPEG, optionally resizing. Originals are unchanged; get retrieves exact bytes. Audio transcoding and PDF rendering are not supported.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string"},"accept":{"type":"array","items":{"type":"string"},"default":["text/*","image/png","image/jpeg","audio/*"]},"max_bytes":{"type":"integer","minimum":1,"default":4194304},"max_dimension":{"type":"integer","minimum":1}},"required":["id"],"additionalProperties":false}"#,
    },
    Tool { name: "files_get", description: "Return the original file bytes as an embedded binary resource. Never presents or converts content and never writes a server-local path. Directories require CLI extraction.", input_schema: ID_SCHEMA },
    Tool {
        name: "files_tag", description: "Add a tag to a stored file.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"}},"required":["id","name"],"additionalProperties":false}"#,
    },
    Tool {
        name: "files_fetch", description: "Fetch a URL and import its bytes as a file. Importing never embeds; `files_index` does.",
        input_schema: r#"{"type":"object","properties":{"url":{"type":"string"},"mime":{"type":"string"},"name":{"type":"string"},"tags":{"type":"array","items":{"type":"string"},"default":[]},"max_bytes":{"type":"integer","minimum":1,"default":8388608}},"required":["url"],"additionalProperties":false}"#,
    },
    Tool {
        name: "files_search", description: "Search stored file names, media types, and tags.",
        input_schema: r#"{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}"#,
    },
    Tool { name: "files_index", description: "Derive the WeMM index over every stored file's content, whoever saved it: images, text, HTML text and PDF text layers through one model. Requires the wemm build on a GB10 and the WEMM_PILE, WEMM_ASSETS and WEMM_ROOT model environment; binds the model (tens of seconds) and may run for hours.", input_schema: EMPTY_SCHEMA },
    Tool {
        name: "files_similar", description: "Rank stored files by meaning in the one WeMM space for text, images and documents. Supply exactly one of id or text. Scores are reconstructed cosines, not calibrated relevance; floor is an optional cosine in [-1,1]. A content held by several files is one hit. Requires the wemm build on a GB10 and binds the model per call.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string"},"text":{"type":"string"},"floor":{"type":"number","minimum":-1,"maximum":1},"limit":{"type":"integer","minimum":0,"default":10},"tags":{"type":"array","items":{"type":"string"},"default":[]}},"additionalProperties":false}"#,
    },
    Tool { name: "files_imports", description: "List stored imports.", input_schema: EMPTY_SCHEMA },
    Tool {
        name: "files_tree", description: "Inspect a stored directory or import tree.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string"},"depth":{"type":"integer","minimum":0}},"required":["id"],"additionalProperties":false}"#,
    },
    Tool {
        name: "files_resolve", description: "Resolve literal selectors in one collection view. Returns one result or diagnostic for each selector; never reads a local file or stdin.",
        input_schema: r#"{"type":"object","properties":{"selectors":{"type":"array","items":{"type":"string"}}},"required":["selectors"],"additionalProperties":false}"#,
    },
    Tool {
        name: "files_diff", description: "Compare stored files, directories, or imports.",
        input_schema: r#"{"type":"object","properties":{"left":{"type":"string"},"right":{"type":"string"}},"required":["left","right"],"additionalProperties":false}"#,
    },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Id {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Add {
    data: String,
    name: String,
    mime: String,
    #[serde(default)]
    tags: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    #[serde(default)]
    tags: Vec<String>,
    mime: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct View {
    id: String,
    accept: Option<Vec<String>>,
    #[serde(default = "view_budget")]
    max_bytes: usize,
    max_dimension: Option<u32>,
}
fn view_budget() -> usize {
    ViewOptions::default().max_bytes
}
fn fetch_budget() -> usize {
    8 * 1024 * 1024
}
fn similarity_limit() -> usize {
    10
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tag {
    id: String,
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fetch {
    url: String,
    mime: Option<String>,
    name: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "fetch_budget")]
    max_bytes: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Similar {
    id: Option<String>,
    text: Option<String>,
    #[serde(default)]
    floor: Option<f64>,
    #[serde(default = "similarity_limit")]
    limit: usize,
    #[serde(default)]
    tags: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tree {
    id: String,
    depth: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    selectors: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Diff {
    left: String,
    right: String,
}

impl Faculty for Files {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }

    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        let files = &self.operations;
        match name {
            "files_add" => {
                let args: Add = decode_arguments(arguments)?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(args.data)
                    .context("file data must be base64")?;
                let id = files.add_bytes(bytes.into(), &args.name, &args.mime, &args.tags)?;
                out.line(format!("files:{id:x}"))
            }
            "files_list" => {
                let args: List = decode_arguments(arguments)?;
                out.text(files.list(&args.tags, args.mime.as_deref())?)
            }
            "files_show" => {
                let args: Id = decode_arguments(arguments)?;
                out.text(files.show(&args.id)?)
            }
            "files_view" => {
                let args: View = decode_arguments(arguments)?;
                let options = ViewOptions {
                    accept: args.accept.unwrap_or_else(|| ViewOptions::default().accept),
                    max_bytes: args.max_bytes,
                    max_dimension: args.max_dimension,
                };
                options.validate().map_err(crate::mcp::invalid_arguments)?;
                out.emit(files.view(&args.id, &options)?)
            }
            "files_get" => {
                let args: Id = decode_arguments(arguments)?;
                let export = files.get(&args.id)?;
                let uri = export.uri();
                out.blob(export.bytes, "application/octet-stream", uri)
            }
            "files_tag" => {
                let args: Tag = decode_arguments(arguments)?;
                files.tag(&args.id, &args.name, out)
            }
            "files_fetch" => {
                let args: Fetch = decode_arguments(arguments)?;
                files.fetch(
                    &FetchOptions {
                        url: &args.url,
                        mime: args.mime.as_deref(),
                        name: args.name.as_deref(),
                        tags: &args.tags,
                        max_bytes: args.max_bytes,
                    },
                    out,
                )
            }
            "files_search" => {
                let args: Search = decode_arguments(arguments)?;
                out.text(files.search(&args.query)?)
            }
            "files_similar" => {
                let args: Similar = decode_arguments(arguments)?;
                files.similar(
                    &SimilarityOptions {
                        id: args.id.as_deref(),
                        text: args.text.as_deref(),
                        floor: args.floor,
                        limit: args.limit,
                        tags: &args.tags,
                    },
                    out,
                )
            }
            "files_index" => {
                let _: Empty = decode_arguments(arguments)?;
                files.index(out)
            }
            "files_imports" => {
                let _: Empty = decode_arguments(arguments)?;
                out.text(files.imports()?)
            }
            "files_tree" => {
                let args: Tree = decode_arguments(arguments)?;
                out.text(files.tree(&args.id, args.depth)?)
            }
            "files_resolve" => {
                let args: Resolve = decode_arguments(arguments)?;
                for (selector, result) in args.selectors.iter().zip(files.resolve(&args.selectors)?)
                {
                    match result {
                        Ok(reference) => {
                            out.line(format!("{selector}\tfiles:{}", reference.hex()))?
                        }
                        Err(error) => out.line(format!("UNRESOLVED: {selector} — {error}"))?,
                    }
                }
                Ok(())
            }
            "files_diff" => {
                let args: Diff = decode_arguments(arguments)?;
                out.text(files.diff(&args.left, &args.right)?)
            }
            other => bail!("Files MCP has no tool {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults the schema advertises are the defaults serde applies,
    /// and there is no floor default: no WeMM relevance threshold is
    /// calibrated, so an omitted floor ranks every content.
    #[test]
    fn similar_schema_defaults_match_the_deserializer() {
        let tool = TOOLS
            .iter()
            .find(|tool| tool.name == "files_similar")
            .expect("files_similar tool");
        let schema: serde_json::Value = serde_json::from_str(tool.input_schema).unwrap();
        let properties = &schema["properties"];
        assert!(properties["floor"].get("default").is_none());
        assert_eq!(
            properties["limit"]["default"].as_u64().unwrap() as usize,
            similarity_limit()
        );
        let args: Similar = decode_arguments(Bytes::from(br#"{"text":"q"}"#.to_vec())).unwrap();
        assert_eq!(args.floor, None);
        assert_eq!(args.limit, similarity_limit());
    }

    #[test]
    fn retired_semantic_tools_and_arguments_are_gone() {
        assert!(!TOOLS.iter().any(|tool| tool.name == "files_embed7b"));
        for retired in [
            r#"{"text":"q","kind":"image"}"#,
            r#"{"text":"q","mm7b":true}"#,
        ] {
            assert!(decode_arguments::<Similar>(Bytes::from(retired.as_bytes().to_vec())).is_err());
        }
    }
}
