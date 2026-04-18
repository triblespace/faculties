//! GORBIE-embeddable viewers for faculty data.
//!
//! Only available behind the `widgets` feature flag.

pub mod compass;
pub mod messages;
pub mod timeline;
pub mod wiki;

pub use compass::CompassBoard;
pub use messages::MessagesPanel;
pub use timeline::BranchTimeline;
pub use wiki::WikiViewer;
