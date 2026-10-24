//! Canonical Archive library operations and explicit frontend adapters.
//! Source-format scanners remain reusable in the existing archive_* modules.

pub mod cli;
pub mod mcp;
mod local;
mod operations;
pub mod sessions;
pub use operations::{
    parse_tai_timestamp, Archive, ImportReceipt, ImportSource, ImportSummary, Publication,
};
