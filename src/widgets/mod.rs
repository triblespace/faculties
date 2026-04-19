//! GORBIE-embeddable viewers for faculty data.
//!
//! Only available behind the `widgets` feature flag.

pub mod compass;
pub mod inspector;
pub mod live;
pub mod messages;
pub mod timeline;
pub mod wiki;

pub use compass::CompassBoard;
pub use inspector::PileInspector;
pub use live::SharedPile;
pub use messages::MessagesPanel;
pub use timeline::BranchTimeline;
pub use wiki::WikiViewer;
