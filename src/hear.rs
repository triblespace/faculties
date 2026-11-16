//! Hearing: shared utterance segmentation, filtering and native embeddings,
//! [`prosody`], which reads the tone of a recorded voice with no model, and
//! [`timbre`], which compares whose voice clips sound like.
//!
//! Recorded clips use resident bytes and explicit local model configuration.
//! The CLI alone owns Soma listening, software pause-file holds and historical
//! JSONL/raw-file handover. Capture stays open during holds; completed live
//! utterances are resampled once, never one capture frame at a time.

pub mod cli;
pub mod mcp;
mod operations;
pub mod prosody;
pub(crate) mod segmenter;
pub mod timbre;
pub use operations::{
    process_pcm16k, Backend, ClipSummary, Hear, Heard, ModelConfig, Observation, Options, Outcome,
    DEFAULT_MODEL, DEFAULT_PROMPT, HEAR_RATE,
};
