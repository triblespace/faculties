//! Canonical Archive library operations and explicit frontend adapters.
//! Source-format scanners remain reusable in the existing archive_* modules.

pub mod cli;
mod local;
pub mod mcp;
mod operations;
pub mod sessions;
#[cfg(feature = "widgets")]
pub(crate) use operations::classifier_stopped;
pub use operations::{
    parse_tai_timestamp, Archive, ImportReceipt, ImportSource, ImportSummary, Publication, Reading,
};
