//! One self-contained fragment per generation: the canonical Files record of
//! the exact bytes, and the generation entity that says how they were made.
//! Published as one commit to the Files collection, so the file never exists
//! without its provenance and the provenance never without its file.

use anyhow::{bail, Result};
use triblespace::core::metadata;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{Boolean, Handle, NsTAIInterval, F64, U256BE};
use triblespace::prelude::*;

use super::openai;
use super::operations::{FluxOptions, Generated, Request, Variant};
use crate::schemas::imagine::{
    imagine, KIND_GENERATION, PROVIDER_FLUX, PROVIDER_OPENAI, VARIANT_DEV, VARIANT_KLEIN,
};

type TextHandle = Inline<Handle<UTF8String>>;

/// The fragment to publish, and the two ids it introduces.
pub struct Staged {
    pub fragment: Fragment,
    pub file: Id,
    pub generation: Id,
}

fn count(value: u64) -> Inline<U256BE> {
    u128::from(value).to_inline()
}

/// Stage `generated` as a Files record named `name` together with its
/// generation entity, requested at `requested_at`.
pub fn stage(
    request: &Request,
    generated: &Generated,
    requested_at: Inline<NsTAIInterval>,
    name: &str,
) -> Result<Staged> {
    let mut fragment =
        crate::files::stage(generated.bytes().clone(), name, generated.media_type())?;
    let file = fragment.root().expect("a staged file has one root");
    let prompt: TextHandle = fragment.put(request.prompt().to_owned());
    let generation = match (request, generated) {
        (Request::OpenAi(options), Generated::OpenAi(answer)) => {
            openai_entity(&mut fragment, file, prompt, options, answer, requested_at)?
        }
        (Request::Flux(options), Generated::Flux(_)) => {
            flux_entity(&mut fragment, file, prompt, options, requested_at)
        }
        _ => bail!("the generation result does not belong to its request's provider"),
    };
    let generation_id = generation.root().expect("a generation has one root");
    fragment += generation;
    Ok(Staged {
        fragment,
        file,
        generation: generation_id,
    })
}

fn openai_entity(
    fragment: &mut Fragment,
    file: Id,
    prompt: TextHandle,
    options: &openai::Options,
    answer: &openai::Generated,
    requested_at: Inline<NsTAIInterval>,
) -> Result<Fragment> {
    let model: TextHandle = fragment.put(options.model.clone());
    let revised: Option<TextHandle> = answer
        .revised_prompt
        .as_ref()
        .map(|text| fragment.put(text.clone()));
    let request_id: Option<TextHandle> = answer
        .request_id
        .as_ref()
        .map(|text| fragment.put(text.clone()));
    let (size_auto, width, height): (Option<Inline<Boolean>>, _, _) = match options.size {
        openai::Size::Auto => (Some(true.to_inline()), None, None),
        openai::Size::Exact { width, height } => {
            (None, Some(count(width.into())), Some(count(height.into())))
        }
    };
    let created: Option<Inline<NsTAIInterval>> = answer
        .created
        .map(|seconds| crate::clock::point(hifitime::Epoch::from_unix_seconds(seconds as f64)))
        .transpose()?;
    let usage = answer.usage;
    let compression = options.output_compression.map(|value| count(value.into()));
    let response_width = answer.size.map(|size| count(size.0.into()));
    let response_height = answer.size.map(|size| count(size.1.into()));
    let response_quality = answer.quality.map(openai::Quality::id);
    let response_background = answer.background.map(openai::Background::id);
    let response_format = answer.output_format.map(openai::OutputFormat::id);
    let input_tokens = usage.input_tokens.map(count);
    let output_tokens = usage.output_tokens.map(count);
    let total_tokens = usage.total_tokens.map(count);
    let input_text_tokens = usage.input_text_tokens.map(count);
    let input_image_tokens = usage.input_image_tokens.map(count);
    let output_text_tokens = usage.output_text_tokens.map(count);
    let output_image_tokens = usage.output_image_tokens.map(count);
    let quality = options.quality.id();
    let output_format = options.output_format.id();
    let background = options.background.id();
    let moderation = options.moderation.id();
    Ok(entity! { _ @
        metadata::tag: KIND_GENERATION,
        metadata::created_at: requested_at,
        imagine::output: file,
        imagine::provider: PROVIDER_OPENAI,
        imagine::model: model,
        imagine::prompt: prompt,
        imagine::revised_prompt?: revised,
        imagine::size_auto?: size_auto,
        imagine::width?: width,
        imagine::height?: height,
        imagine::quality: quality,
        imagine::output_format: output_format,
        imagine::output_compression?: compression,
        imagine::background: background,
        imagine::moderation: moderation,
        imagine::response_created_at?: created,
        imagine::response_width?: response_width,
        imagine::response_height?: response_height,
        imagine::response_quality?: response_quality,
        imagine::response_background?: response_background,
        imagine::response_format?: response_format,
        imagine::request_id?: request_id,
        imagine::input_tokens?: input_tokens,
        imagine::output_tokens?: output_tokens,
        imagine::total_tokens?: total_tokens,
        imagine::input_text_tokens?: input_text_tokens,
        imagine::input_image_tokens?: input_image_tokens,
        imagine::output_text_tokens?: output_text_tokens,
        imagine::output_image_tokens?: output_image_tokens,
    })
}

fn flux_entity(
    fragment: &mut Fragment,
    file: Id,
    prompt: TextHandle,
    options: &FluxOptions,
    requested_at: Inline<NsTAIInterval>,
) -> Fragment {
    let (variant, checkpoint) = match options.variant {
        Variant::Klein => (VARIANT_KLEIN, "black-forest-labs/FLUX.2-klein-4B"),
        Variant::Dev => (VARIANT_DEV, "black-forest-labs/FLUX.2-dev"),
    };
    let model: TextHandle = fragment.put(checkpoint.to_owned());
    let steps = options.steps.unwrap_or(options.variant.default_steps());
    let guidance: Inline<F64> = f64::from(options.guidance).to_inline();
    let steps = count(steps as u64);
    let seed = count(options.seed);
    let width = count(options.width as u64);
    let height = count(options.height as u64);
    entity! { _ @
        metadata::tag: KIND_GENERATION,
        metadata::created_at: requested_at,
        imagine::output: file,
        imagine::provider: PROVIDER_FLUX,
        imagine::model: model,
        imagine::prompt: prompt,
        imagine::flux_variant: variant,
        imagine::steps: steps,
        imagine::seed: seed,
        imagine::width: width,
        imagine::height: height,
        imagine::guidance: guidance,
    }
}
