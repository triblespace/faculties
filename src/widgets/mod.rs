//! GORBIE-embeddable viewers for faculty data.
//!
//! Only available behind the `widgets` feature flag.

pub mod args;
pub mod colony;
pub mod decisions;
pub mod diagnostics;
pub mod discord;
pub mod events;
pub mod files;
pub mod goals;
pub mod knowledge;
pub mod knowledge_health;
pub mod mail;
pub mod memory;
pub mod messages;
pub mod priority_game;
pub mod profiles;
pub mod quota;
pub mod relations;
pub mod rules;
pub mod schema_catalog;
pub mod storage;
pub mod teams;
pub mod timeline;

pub use args::resolve_pile_path;
pub use colony::ColonyViewer;
pub use decisions::DecisionsPanel;
pub use diagnostics::DiagnosticsViewer;
pub use discord::DiscordViewer;
pub use events::EventsViewer;
pub use files::FilesViewer;
pub use goals::GoalsBoard;
pub use knowledge::KnowledgeViewer;
pub use knowledge_health::KnowledgeHealthViewer;
pub use mail::MailViewer;
pub use memory::MemoryViewer;
pub use messages::MessagesPanel;
pub use priority_game::{GameWrite, PriorityGame};
pub use profiles::ProfilesViewer;
pub use quota::QuotaViewer;
pub use relations::RelationsViewer;
pub use rules::RulesViewer;
pub use schema_catalog::SchemasViewer;
pub use storage::{DatasetRevision, DatasetView, SourceKey, StorageState, WidgetContext};
pub use teams::TeamsViewer;
pub use timeline::{BranchTimeline, SourceKind, TimelineSource};
