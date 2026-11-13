//! Image generation with provenance. Providers sit behind one call, each with
//! its own typed options: OpenAI's Image API (remote, always built) and the
//! local FLUX.2 pipeline (the `imagine` cargo feature plus weights). Every
//! successful generation lands in Files as the provider's exact bytes plus a
//! generation record, in one commit. Credentials are exact Secrets selectors
//! bound in the `imagine` collection. Generation never shells out to another
//! faculty.

pub mod auth;
pub mod cli;
#[cfg(any(feature = "imagine", test))]
mod configuration;
pub mod mcp;
pub mod openai;
mod operations;
mod provenance;

pub use operations::{
    Flux, FluxOptions, Generated, GeneratedImage, Imagine, ModelSources, Receipt, Request, Variant,
};

#[cfg(test)]
mod tests;
