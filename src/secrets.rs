//! The standalone encryption core plus explicit native faculty frontends.
//! Existing core paths remain re-exported; the small crate stays independent
//! of the CLI, MCP, Mary, and widget dependency graph.
pub use faculties_secrets::*;
pub mod cli;
pub mod mcp;
pub mod onepassword;
mod operations;
pub use operations::{DeliveryReport, ItemMetadata, SecretMetadata, Secrets};
