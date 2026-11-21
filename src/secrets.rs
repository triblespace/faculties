//! The standalone encryption core plus explicit native faculty frontends.
//! Existing core paths remain re-exported; the small crate stays independent
//! of the CLI, MCP, Mary, and widget dependency graph.
pub use faculties_secrets::*;
pub mod cli;
pub mod mcp;
pub mod onepassword;
mod operations;
pub(crate) use operations::{acquiring_snapshot, add_raw_to_colony};
pub use operations::{DeliveryReport, ItemMetadata, SecretMetadata, Secrets};
