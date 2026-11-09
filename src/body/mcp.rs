//! Finite resident tools. This adapter has no daemon, Python, device or host
//! export path. Captures are declarations of acquired signals, not camera calls.
use super::{presentation, Body as Operations, CaptureInput, Signal};
use crate::files::presentation::ViewOptions;
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

pub struct Body {
    operations: Operations,
    files: crate::files::operations::Files,
}
impl Body {
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
const TOOLS: &[Tool] = &[
    Tool {
        name: "body_capture",
        description: "Keep an already acquired signal and literal pose/note in one immutable capture. No camera, microphone or robot is contacted. Vision and audio take their bytes from exactly one of a Files file_id or an absolute path on the host the MCP server runs on (read with the server process's permissions, which are a capability, not authority: pass only paths the user or task authorizes, and workspace selection authorizes none). Vision needs an image MIME and positive width/height; audio needs an audio MIME without image dimensions; touch has only pose/note. Bytes and declared metadata are stored exactly, not decoded or normalized; use body_view for bounded perception.",
        input_schema: r#"{"type":"object","properties":{"modality":{"type":"string","enum":["vision","audio","touch"]},"file_id":{"type":"string","pattern":"^[0-9a-fA-F]{32}$","description":"Exact 32-hex Files entity ID or single-file import ID of the signal."},"path":{"type":"string","description":"Absolute path of the signal on the MCP server's host."},"mime":{"type":"string"},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1},"pose":{"type":"string"},"note":{"type":"string"}},"required":["modality","pose"],"additionalProperties":false}"#,
    },
    Tool {
        name: "body_list",
        description: "List deliberate captures newest first, reading their notes but not frame or pose payloads. Does not inspect hardware.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    Tool {
        name: "body_get",
        description: "Export a selected capture's exact original signal bytes as a file resource, with its ID and declared MIME. This does not display, decode, normalize or play the signal. Touch captures have no file payload. Use body_view for bounded perception; no host output path is accepted.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string","minLength":1}},"required":["id"],"additionalProperties":false}"#,
    },
    Tool {
        name: "body_view",
        description: "Present a stored signal through the bounded Files presenter: images may be resized/converted, audio is size-limited passthrough only. Original bytes do not change; unsupported formats or missing raw-audio metadata are explicit errors. No universal file representation is invented for touch/pose.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string","minLength":1},"accept":{"type":"array","items":{"type":"string"},"minItems":1},"max_bytes":{"type":"integer","minimum":1,"default":4194304},"max_dimension":{"type":"integer","minimum":1}},"required":["id"],"additionalProperties":false}"#,
    },
    Tool {
        name: "body_intent_get",
        description: "Read the current sparse VLA intent through the maintained intent register and its frozen fact/payload snapshot. This does not execute it or contact a robot.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    Tool {
        name: "body_intent_set",
        description: "Append a timestamped literal VLA intent; newest creation time wins with intrinsic event ID breaking ties. This records an instruction, not proof of physical execution. @file and @- are ordinary text.",
        input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}"#,
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
struct Intent {
    text: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Modality {
    Vision,
    Audio,
    Touch,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    modality: Modality,
    file_id: Option<String>,
    path: Option<String>,
    mime: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
    pose: String,
    note: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct View {
    id: String,
    accept: Option<Vec<String>>,
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
    max_dimension: Option<u32>,
}
fn default_max_bytes() -> usize {
    ViewOptions::default().max_bytes
}
impl Capture {
    fn input(self, files: &crate::files::operations::Files) -> Result<CaptureInput> {
        let media = || -> Result<(Bytes, String)> {
            let mime = self
                .mime
                .clone()
                .ok_or_else(|| invalid_arguments("media capture requires mime"))?;
            let bytes = crate::mcp::input::read_reference(
                files,
                self.file_id.as_deref(),
                self.path.as_deref(),
                crate::mcp::input::MAX_HOST_FILE_BYTES,
            )?;
            Ok((bytes, mime))
        };
        let signal = match self.modality {
            Modality::Vision => {
                let width = self
                    .width
                    .ok_or_else(|| invalid_arguments("vision capture requires width"))?;
                let height = self
                    .height
                    .ok_or_else(|| invalid_arguments("vision capture requires height"))?;
                let (bytes, mime) = media()?;
                Signal::Vision {
                    bytes,
                    mime,
                    width,
                    height,
                }
            }
            Modality::Audio => {
                if self.width.is_some() || self.height.is_some() {
                    return Err(invalid_arguments(
                        "audio capture cannot have image dimensions",
                    ));
                }
                let (bytes, mime) = media()?;
                Signal::Audio { bytes, mime }
            }
            Modality::Touch => {
                if self.file_id.is_some()
                    || self.path.is_some()
                    || self.mime.is_some()
                    || self.width.is_some()
                    || self.height.is_some()
                {
                    return Err(invalid_arguments(
                        "touch capture has no media bytes, MIME or image dimensions",
                    ));
                }
                Signal::Touch
            }
        };
        let input = CaptureInput {
            signal,
            pose: self.pose,
            note: self.note,
        };
        input.validate().map_err(invalid_arguments)?;
        Ok(input)
    }
}
impl Faculty for Body {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "body_capture" => {
                let args: Capture = decode_arguments(arguments)?;
                presentation::captured(&self.operations.capture(&args.input(&self.files)?)?, out)
            }
            "body_list" => {
                let _: Empty = decode_arguments(arguments)?;
                presentation::list(&self.operations.list()?, out)
            }
            "body_get" => {
                let args: Id = decode_arguments(arguments)?;
                super::validate_selector(&args.id).map_err(invalid_arguments)?;
                let export = self.operations.get(&args.id)?;
                out.line(format!(
                    "body:{:x}  {} bytes  {}",
                    export.id,
                    export.bytes.len(),
                    export.mime.as_deref().unwrap_or("MIME unavailable")
                ))?;
                let uri = export.uri();
                out.blob(export.bytes, "application/octet-stream", uri)
            }
            "body_view" => {
                let args: View = decode_arguments(arguments)?;
                super::validate_selector(&args.id).map_err(invalid_arguments)?;
                let options = ViewOptions {
                    accept: args.accept.unwrap_or_else(|| ViewOptions::default().accept),
                    max_bytes: args.max_bytes,
                    max_dimension: args.max_dimension,
                };
                options.validate().map_err(invalid_arguments)?;
                out.emit(self.operations.view(&args.id, &options)?)
            }
            "body_intent_get" => {
                let _: Empty = decode_arguments(arguments)?;
                presentation::intent(self.operations.intent()?.as_ref(), out)
            }
            "body_intent_set" => {
                let args: Intent = decode_arguments(arguments)?;
                presentation::intent_set(&self.operations.set_intent(&args.text)?, out)
            }
            _ => bail!("unknown Body tool {name:?}"),
        }
    }
}
