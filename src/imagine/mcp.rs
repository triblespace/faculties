//! Images are perceptions, not server filesystem paths. Generation, durable
//! storage in Files and optional memory publication are distinct steps;
//! delivery errors never restart any of them.
//!
//! The durable receipt comes first: the Files commit happens before anything
//! is emitted, and its id is the first line. The image block after it is
//! optional perception: when it would push the response over the MCP ceiling
//! it is omitted with a line saying so, and the call still succeeds. No
//! field of the tool schema carries base64 or any other encoded bytes.
use super::openai::{self, Background, Moderation, OutputFormat, Quality, Size};
use super::{
    operations::remember_range, FluxOptions, Imagine as Operations, ModelSources, Request, Variant,
};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

const TOOLS: &[Tool] = &[Tool {
    name: "imagine_generate",
    description: "Generate one image and store it in Files with its provenance (provider, model, prompt, every option, the provider's answer metadata) in one commit; the first output line is the Files id, then the image as an attachment when it fits the response. provider openai (default) uses OpenAI's Image API with the bound credential (imagine auth set openai); options: model, size (auto or WIDTHxHEIGHT, multiples of 16, edge <= 3840, aspect 1:3..3:1, 655360..8294400 pixels), quality, output_format, output_compression (jpeg/webp), background (transparent needs png/webp), moderation. One request, never retried; calls can take about two minutes and cost money. provider flux uses the launcher's local FLUX.2 model (needs the imagine build feature and weights; not available in workspaces); options: variant, steps 1-200, seed, width/height multiples of 16 up to 4096, guidance. Options of the other provider are refused. Optional remember also stores the original bytes as a Memory: a TAI timestamp (a 3-second moment ending then) or from..to range. No file paths, downloads, or playback are requested.",
    input_schema: r#"{"type":"object","properties":{"prompt":{"type":"string","minLength":1},"provider":{"type":"string","enum":["openai","flux"],"default":"openai"},"model":{"type":"string","default":"gpt-image-2.5-sunburst"},"size":{"type":"string","default":"auto"},"quality":{"type":"string","enum":["low","medium","high","xhigh","max","auto"],"default":"auto"},"output_format":{"type":"string","enum":["png","jpeg","webp"],"default":"png"},"output_compression":{"type":"integer","minimum":0,"maximum":100},"background":{"type":"string","enum":["transparent","opaque","auto"],"default":"auto"},"moderation":{"type":"string","enum":["auto","low"],"default":"auto"},"variant":{"type":"string","enum":["klein","dev"]},"steps":{"type":"integer","minimum":1,"maximum":200},"seed":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":16,"maximum":4096,"multipleOf":16},"height":{"type":"integer","minimum":16,"maximum":4096,"multipleOf":16},"guidance":{"type":"number","minimum":0},"remember":{"type":"string"}},"required":["prompt"],"additionalProperties":false}"#,
}];

/// What the response may hold besides the image: the receipt, diagnostic and
/// remember lines and the JSON-RPC envelope, with room to spare.
const RESPONSE_MARGIN: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ProviderChoice {
    #[default]
    OpenAi,
    Flux,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Generate {
    prompt: String,
    #[serde(default)]
    provider: ProviderChoice,
    // OpenAI
    model: Option<String>,
    size: Option<Size>,
    quality: Option<Quality>,
    output_format: Option<OutputFormat>,
    output_compression: Option<u8>,
    background: Option<Background>,
    moderation: Option<Moderation>,
    // FLUX
    variant: Option<Variant>,
    steps: Option<usize>,
    seed: Option<u64>,
    width: Option<usize>,
    height: Option<usize>,
    guidance: Option<f32>,
    remember: Option<String>,
}

impl Generate {
    fn request(self) -> Result<(Request, Option<String>)> {
        let request = match self.provider {
            ProviderChoice::OpenAi => {
                let flux = [
                    ("variant", self.variant.is_some()),
                    ("steps", self.steps.is_some()),
                    ("seed", self.seed.is_some()),
                    ("width", self.width.is_some()),
                    ("height", self.height.is_some()),
                    ("guidance", self.guidance.is_some()),
                ];
                if let Some((name, _)) = flux.iter().find(|(_, given)| *given) {
                    bail!("{name} is a flux option; the openai provider takes size, quality, output_format, output_compression, background, moderation and model");
                }
                let mut options = openai::Options::new(self.prompt);
                if let Some(model) = self.model {
                    options.model = model;
                }
                options.size = self.size.unwrap_or_default();
                options.quality = self.quality.unwrap_or_default();
                options.output_format = self.output_format.unwrap_or_default();
                options.output_compression = self.output_compression;
                options.background = self.background.unwrap_or_default();
                options.moderation = self.moderation.unwrap_or_default();
                Request::OpenAi(options)
            }
            ProviderChoice::Flux => {
                let openai = [
                    ("model", self.model.is_some()),
                    ("size", self.size.is_some()),
                    ("quality", self.quality.is_some()),
                    ("output_format", self.output_format.is_some()),
                    ("output_compression", self.output_compression.is_some()),
                    ("background", self.background.is_some()),
                    ("moderation", self.moderation.is_some()),
                ];
                if let Some((name, _)) = openai.iter().find(|(_, given)| *given) {
                    bail!("{name} is an openai option; the flux provider takes variant, steps, seed, width, height and guidance");
                }
                Request::Flux(FluxOptions {
                    prompt: self.prompt,
                    variant: self.variant.unwrap_or_default(),
                    steps: self.steps,
                    seed: self.seed.unwrap_or(0),
                    width: self.width.unwrap_or(1024),
                    height: self.height.unwrap_or(1024),
                    guidance: self.guidance.unwrap_or(4.0),
                })
            }
        };
        Ok((request, self.remember))
    }
}

/// Whether an image of `bytes` length fits one MCP response once
/// base64-encoded, next to everything else the call emits.
fn fits_response(bytes: usize) -> bool {
    let base64 = bytes.div_ceil(3) * 4;
    base64 + RESPONSE_MARGIN <= crate::mcp::Limits::default().response_bytes
}

pub struct Imagine {
    operations: Operations,
    memory: crate::memory::Memory,
}
impl Imagine {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    /// Keeps the supplied storage, workspace routes included: the OpenAI path
    /// stores through its Files route and opens its Secrets route. Only the
    /// local FLUX path is unavailable under routing.
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self::from_operations(Operations::with_storage(storage))
    }
    pub fn with_sources(pile: PathBuf, key: Option<PathBuf>, sources: ModelSources) -> Self {
        Self::with_storage_and_sources(crate::storage::Storage::new(pile, key), sources)
    }
    pub fn with_storage_and_sources(
        storage: crate::storage::Storage,
        sources: ModelSources,
    ) -> Self {
        Self::from_operations(Operations::with_storage(storage).with_flux_sources(sources))
    }
    pub fn from_operations(operations: Operations) -> Self {
        let memory = crate::memory::Memory::with_storage(operations.storage().clone());
        Self { operations, memory }
    }
}
impl Faculty for Imagine {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        if name != "imagine_generate" {
            bail!("Imagine MCP has no tool {name:?}");
        }
        let args: Generate = decode_arguments(arguments)?;
        let (request, remember) = args.request().map_err(invalid_arguments)?;
        request.validate().map_err(invalid_arguments)?;
        let range = remember
            .as_deref()
            .map(remember_range)
            .transpose()
            .map_err(invalid_arguments)?;
        let receipt = self.operations.generate(&request)?;
        let generated = &receipt.generated;
        let bytes = generated.bytes();
        out.line(format!(
            "files:{:x} ({}, {} bytes; generation {:x})",
            receipt.file,
            generated.media_type(),
            bytes.len(),
            receipt.generation
        ))?;
        if let Some(warning) = &receipt.warning {
            out.line(format!("warning: {warning}"))?;
        }
        if fits_response(bytes.len()) {
            out.image(bytes.clone(), generated.media_type())?;
        } else {
            out.line(format!(
                "image not attached: {} bytes would exceed this response's size limit; it is stored as files:{:x}",
                bytes.len(),
                receipt.file
            ))?;
        }
        out.line(generated.diagnostic())?;
        if let Some(range) = range {
            let remembered = self.memory.image(bytes.as_ref(), range).context(
                "image generated and stored, but remembering it failed; generation is not retried",
            )?;
            remembered.emit(out)?;
        }
        Ok(())
    }
}
