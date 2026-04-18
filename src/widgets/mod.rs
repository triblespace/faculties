//! GORBIE-embeddable viewers for faculty data.
//!
//! Only available behind the `widgets` feature flag.

pub mod timeline;
pub mod wiki;

pub use timeline::BranchTimeline;
pub use wiki::WikiViewer;
