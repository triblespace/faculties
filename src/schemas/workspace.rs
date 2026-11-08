//! Workspace bindings in the node's existing private `config` collection.
//!
//! These identities were published by Erlkonig, not minted by this extraction.
//! The anchors and encodings below are unchanged from its `node_schema` at
//! `81f06609e40f7c451d76771e374ede15e34b7fc2`. A workspace route uses the
//! existing `config::anchor` for its role and `config::selects` for its root.

use triblespace::macros::id_hex;
use triblespace::prelude::*;

/// An explicit query workspace revision, separate from service enrollment.
/// Minted with `trible genid` on 2026-10-08.
pub const WORKSPACE: Id = id_hex!("F4D15E6C2AA3263C1B6DBDD0D5880539");

attributes! {
    /// Measurement subject -> enrolled managed-pile identity, when applicable.
    /// The workspace reader recognizes this legacy binding only to refuse
    /// implicit retargeting. Minted on 2026-10-06.
    "C39028980C8DCF8219B07396CCC3B1C3" as managed_pile: inlineencodings::GenId;
    /// Explicit enrollment or withdrawal of a managed entry. False retains
    /// history but withdraws the node's request to manage it.
    /// Minted on 2026-10-06.
    "6029BC6E30BF94DB16A5D4C96FF6B4E8" as enrolled: inlineencodings::Boolean;
    /// Workspace revision -> role selection (config::anchor + config::selects).
    /// Minted with `trible genid` on 2026-10-08.
    "57DCA7F297BF848DA19CFD2730D6293E" as workspace_route: inlineencodings::GenId;
}
