//! Explicit MCP UX: a speech clip is an audio attachment, never an instruction
//! to play the server's speakers. Routing metadata is a separate operation.
use super::synthesis::{ModelSources, Synthesizer};
use super::{Channel, Voice as Operations};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

const TOOLS: &[Tool] = &[
    Tool { name: "voice_synthesize", description: "Synthesize literal text with the configured resident voice and return a WAV attachment, without host playback. Optional Breeze direction is delivery instruction, never spoken text; omit direction and cfg_scale to inherit startup defaults. clear_direction removes direction for this request and defaults CFG to 1; otherwise non-unit CFG requires an effective direction. Blank direction is invalid. Other backends reject delivery options. Requires configured model-asset routing and voice features; discovery opens none. No utterance is recorded as heard.", input_schema: r#"{"type":"object","properties":{"text":{"type":"string","minLength":1},"direction":{"type":"string","minLength":1,"description":"Breeze delivery instruction; omit to inherit startup direction. Conflicts with clear_direction."},"clear_direction":{"type":"boolean","default":false},"cfg_scale":{"type":"number","exclusiveMinimum":0,"description":"Finite Breeze guidance strength; omit to inherit, or 1 when clearing direction."}},"required":["text"],"additionalProperties":false}"# },
    Tool { name: "voice_route", description: "Read the latest stored private say and public shout routing policies (or built-in defaults). Metadata only: no device enumeration, Reachy probe, synthesis, or playback. Private say routing still rejects public devices even if its policy lists them.", input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"# },
    Tool { name: "voice_route_set", description: "Publish an ordered device-name preference list for local say or shout routing. Names are literal case-insensitive match patterns, not files. A new timestamped generation replaces older policy generations; exact timestamp ties are unioned. This changes future host routing only and does not play audio. The say privacy check cannot be configured away.", input_schema: r#"{"type":"object","properties":{"channel":{"type":"string","enum":["say","shout"]},"devices":{"type":"array","minItems":1,"items":{"type":"string","minLength":1}}},"required":["channel","devices"],"additionalProperties":false}"# },
];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Text {
    text: String,
    direction: Option<String>,
    #[serde(default)]
    clear_direction: bool,
    cfg_scale: Option<f32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteSet {
    channel: Channel,
    devices: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesis_transport_is_literal_and_advertises_delivery() {
        let schema: serde_json::Value = serde_json::from_str(TOOLS[0].input_schema).unwrap();
        for field in ["direction", "clear_direction", "cfg_scale"] {
            assert!(schema["properties"][field].is_object());
        }
        let request: Text = decode_arguments(
            r#"{"text":"(laugh) @literal","direction":"@also-literal","cfg_scale":2.5}"#
                .to_owned()
                .into(),
        )
        .unwrap();
        assert_eq!(request.text, "(laugh) @literal");
        assert_eq!(request.direction.as_deref(), Some("@also-literal"));
        assert_eq!(request.cfg_scale, Some(2.5));
        assert!(!request.clear_direction);
    }
}
pub struct Voice {
    operations: Operations,
    synthesizer: Option<Synthesizer>,
}
impl Voice {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        if storage.collection_routes().is_some() {
            return Self {
                operations: Operations::with_storage(storage),
                synthesizer: None,
            };
        }
        let sources = ModelSources::from_environment(Some(storage.path()));
        Self::with_storage_and_sources(storage, sources)
    }
    pub fn with_sources(pile: PathBuf, key: Option<PathBuf>, sources: ModelSources) -> Self {
        Self::with_storage_and_sources(crate::storage::Storage::new(pile, key), sources)
    }
    pub fn with_storage_and_sources(
        storage: crate::storage::Storage,
        sources: ModelSources,
    ) -> Self {
        let synthesizer = storage
            .collection_routes()
            .is_none()
            .then(|| Synthesizer::new(sources));
        Self {
            operations: Operations::with_storage(storage),
            synthesizer,
        }
    }
}
impl Faculty for Voice {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "voice_synthesize" => {
                let args: Text = decode_arguments(arguments)?;
                super::operations::validate_text(&args.text).map_err(invalid_arguments)?;
                let options = super::options::SpeechOptions {
                    direction: args.direction,
                    clear_direction: args.clear_direction,
                    cfg_scale: args.cfg_scale,
                };
                options.validate().map_err(invalid_arguments)?;
                let synthesizer = self.synthesizer.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "voice synthesize: workspace model-asset routing is unconfigured"
                    )
                })?;
                synthesizer
                    .validate_options(&options)
                    .map_err(invalid_arguments)?;
                let clip = synthesizer.synthesize_with_options(&args.text, &options)?;
                out.audio(clip.wav.clone(), super::AUDIO_WAV_MIME)?;
                out.line(format!(
                    "{} Hz mono, {} samples ({:.2}s); no host playback",
                    clip.sample_rate,
                    clip.sample_count,
                    clip.duration_seconds()
                ))
            }
            "voice_route" => {
                let _: Empty = decode_arguments(arguments)?;
                for policy in self.operations.routes()? {
                    out.line(format!(
                        "{} policy (priority order): {}",
                        policy.channel.name(),
                        policy.devices.join(" → ")
                    ))?;
                }
                Ok(())
            }
            "voice_route_set" => {
                let args: RouteSet = decode_arguments(arguments)?;
                super::operations::validate_devices(&args.devices).map_err(invalid_arguments)?;
                self.operations
                    .set_route(args.channel, &args.devices)?
                    .emit(out)
            }
            _ => bail!("Voice MCP has no tool {name:?}"),
        }
    }
}
