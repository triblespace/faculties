//! Finite recorded-audio processing only; discovery never opens a model,
//! device, local side file, output directory or capture stream.
use super::{ModelConfig, Options, DEFAULT_PROMPT};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use crate::turntaking::SpeechFilter;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;

pub struct Hear {
    config: Option<ModelConfig>,
    files: Option<crate::files::operations::Files>,
}
impl Hear {
    /// Without storage, a clip can come only from a host path.
    pub fn new(config: Option<ModelConfig>) -> Self {
        Self {
            config,
            files: None,
        }
    }
    /// Resolve `file_id` clips against this storage's Files view.
    pub fn with_storage(storage: crate::storage::Storage, config: Option<ModelConfig>) -> Self {
        Self {
            config,
            files: Some(crate::files::operations::Files::with_storage(storage)),
        }
    }
}
impl Hear {
    /// The clip's bytes, exactly as `hear_once` hands them to Hear's input
    /// stage: one of a Files `file_id` (only with storage) or a host `path`.
    fn clip(&self, file_id: Option<&str>, path: Option<&str>) -> Result<Bytes> {
        match crate::mcp::input::exactly_one([("file_id", file_id), ("path", path)])? {
            ("file_id", id) => {
                let files = self.files.as_ref().ok_or_else(|| {
                    invalid_arguments("file_id needs a launcher-configured pile; use path")
                })?;
                crate::mcp::input::InputBudget::per_call().file_id(files, id)
            }
            (_, path) => crate::mcp::input::InputBudget::per_call().host_file(path),
        }
    }
}
const TOOLS:&[Tool]=&[Tool{
    name:"hear_once",
    description:"Run one encoded audio clip through the shared VAD, speech filters and launcher-configured Gemma hearing stack. The clip is a Files file_id or an absolute path on the host the MCP server runs on, read with the server process's permissions, which are a capability, not authority: pass only paths the user or task authorizes, and workspace selection authorizes none. Returns ordered metadata and exact raw f32le row-major embedding resources, not audio playback; optional text is diagnostic. Requires the hear build feature and explicit local model/config/tokenizer configuration. No microphone, Soma listener, download, log path or model selection can be requested.",
    input_schema:r#"{"type":"object","properties":{"file_id":{"type":"string","pattern":"^[0-9a-fA-F]{32}$","description":"Exact 32-hex Files entity ID or single-file import ID of the clip."},"path":{"type":"string","description":"Absolute path of the clip on the MCP server's host."},"mime_type":{"type":"string","default":"audio/wav"},"source":{"type":"string","default":"resident"},"transcribe":{"type":"boolean","default":false},"prompt":{"type":"string","default":"Transcribe exactly what is being said."},"tokens":{"type":"integer","minimum":1,"default":64},"min_chars":{"type":"integer","minimum":0,"default":2},"min_dur_s":{"type":"number","minimum":0,"default":0.6}},"oneOf":[{"required":["file_id"]},{"required":["path"]}],"additionalProperties":false}"#
}];
fn mime() -> String {
    "audio/wav".to_owned()
}
fn source() -> String {
    "resident".to_owned()
}
fn prompt() -> String {
    DEFAULT_PROMPT.to_owned()
}
fn tokens() -> usize {
    64
}
fn min_chars() -> usize {
    2
}
fn min_dur() -> f64 {
    0.6
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Once {
    file_id: Option<String>,
    path: Option<String>,
    #[serde(default = "mime")]
    mime_type: String,
    #[serde(default = "source")]
    source: String,
    #[serde(default)]
    transcribe: bool,
    #[serde(default = "prompt")]
    prompt: String,
    #[serde(default = "tokens")]
    tokens: usize,
    #[serde(default = "min_chars")]
    min_chars: usize,
    #[serde(default = "min_dur")]
    min_dur_s: f64,
}
impl Faculty for Hear {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        if name != "hear_once" {
            bail!("unknown Hear MCP tool {name:?}");
        }
        let a: Once = decode_arguments(arguments)?;
        super::operations::mime_hint(&a.mime_type).map_err(invalid_arguments)?;
        if a.tokens == 0 {
            return Err(invalid_arguments(
                "transcription token limit must be positive",
            ));
        }
        let options = Options {
            transcribe: a.transcribe,
            prompt: a.prompt,
            tokens: a.tokens,
            filter: SpeechFilter {
                min_chars: a.min_chars,
                min_dur_s: a.min_dur_s,
                ..Default::default()
            },
        };
        options.validate().map_err(invalid_arguments)?;
        let bytes = self.clip(a.file_id.as_deref(), a.path.as_deref())?;
        if bytes.is_empty() {
            return Err(invalid_arguments("recorded audio contains no bytes"));
        }
        let config=self.config.as_ref().ok_or_else(||anyhow::anyhow!("Hear has no launcher-configured local model pile/config/tokenizer; no model or audio device was opened"))?;
        let summary =
            super::Hear::new(config.clone()).once(bytes, &a.mime_type, &a.source, &options, out)?;
        out.line(format!(
            "{} utterance(s); {} embedded, {} dropped",
            summary.segments, summary.embedded, summary.dropped
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_prompt_and_source_are_literal() {
        let args: Once = decode_arguments(Bytes::from(
            r#"{"path":"/not-opened.wav","prompt":"@/not-a-file","source":"@-"}"#,
        ))
        .unwrap();
        assert_eq!(args.prompt, "@/not-a-file");
        assert_eq!(args.source, "@-");
        assert_eq!(args.mime_type, "audio/wav");
        assert!(!args.transcribe);
    }

    /// file_id and path hand Hear's input stage the same bytes; no model runs.
    #[test]
    fn file_id_and_path_hand_the_input_stage_the_same_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("hear.pile");
        let key = directory.path().join("hear.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let clip: Vec<u8> = (0..=255u8).cycle().take(4099).collect();
        let host = directory.path().join("clip.wav");
        std::fs::write(&host, &clip).unwrap();
        let folder = directory.path().join("clips");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("clip.wav"), &clip).unwrap();
        let files = crate::files::mcp::Files::new(pile.clone(), Some(key.clone()));
        let import = |path: &std::path::Path| {
            let mut report = String::new();
            files
                .call(
                    "files_add",
                    Bytes::from(serde_json::to_vec(&serde_json::json!({ "path": path })).unwrap()),
                    &mut Out::new(&mut |part| {
                        if let crate::out::Part::Text { text } = part {
                            report.push_str(&text);
                        }
                        Ok(())
                    }),
                )
                .unwrap();
            report
                .lines()
                .find_map(|line| line.strip_prefix("Import: "))
                .unwrap()
                .trim()
                .to_owned()
        };
        let file_id = import(&host);
        let folder_id = import(&folder);
        let hear = Hear::with_storage(crate::storage::Storage::new(pile, Some(key)), None);
        let path = host.to_str().unwrap();
        assert_eq!(
            hear.clip(Some(&file_id), None).unwrap().as_ref(),
            clip.as_slice()
        );
        assert_eq!(
            hear.clip(None, Some(path)).unwrap().as_ref(),
            clip.as_slice()
        );
        let error = hear.clip(Some(&folder_id), None).unwrap_err();
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<crate::mcp::input::NotAFile>()
                .is_some()),
            "{error:#}"
        );
        assert!(hear.clip(Some(&file_id), Some(path)).is_err());
        // Without storage only a path can name the clip.
        let bare = Hear::new(None);
        assert_eq!(
            bare.clip(None, Some(path)).unwrap().as_ref(),
            clip.as_slice()
        );
        let error = bare.clip(Some(&file_id), None).unwrap_err();
        assert!(
            format!("{error:#}").contains("launcher-configured pile"),
            "{error:#}"
        );
    }
}
