//! Home schema: which Hue bridge Home talks to, and what it asked it to do.
//!
//! Light state itself is never stored. The bridge is the source of truth for
//! what is on, and a copy of it here would be a second model that is stale the
//! moment someone presses a wall switch. Two record kinds live in the `home`
//! collection:
//!
//! - A bridge binding ([`KIND_BRIDGE`]) names the bridge (its own `bridgeid`),
//!   where to reach it, and the exact Secrets version holding the bridge
//!   username, and the SHA-256 fingerprint of the certificate the bridge
//!   presented when it was bound. Every later connection is pinned to that
//!   fingerprint. Rebinding supersedes the earlier binding through
//!   `metadata::supersedes`, as Discord's authentication revisions do.
//! - An action ([`KIND_ACTION`]) is one state change Home sent: the bridge
//!   resource it addressed, the target's Hue name at the time, the JSON body,
//!   the bridge's JSON answer, and `metadata::created_at`. Reads record
//!   nothing.
//!
//! Every id below was minted with `trible genid` on 2026-11-09. Attributes use
//! the explicit-anchor form, so each value encoding participates in identity.

use triblespace::core::id::ExclusiveId;
use triblespace::core::inline::encodings::hash::HashProtocol;
use triblespace::core::inline::RawInline;
use triblespace::core::metadata::{self, MetaDescribe};
use triblespace::macros::id_hex;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{GenId, Handle, Hash};
use triblespace::prelude::*;

/// SHA-256, as the hash protocol of a foreign 256-bit fingerprint: the
/// digest a TLS certificate is pinned by, which no triblespace store
/// computes. `Hash<Sha256>` keeps it distinct from Blake3 digests by type.
pub struct Sha256;

impl HashProtocol for Sha256 {
    const NAME: &'static str = "sha256";

    fn digest(bytes: &[u8]) -> RawInline {
        use sha2::Digest;
        sha2::Sha256::digest(bytes).into()
    }
}

impl MetaDescribe for Sha256 {
    fn describe() -> Fragment {
        // Minted with `trible genid` on 2026-11-09.
        let id = id_hex!("498BD453DFF7831CE1334F033CF05394");
        entity! { ExclusiveId::force_ref(&id) @
            metadata::name: "sha256",
            metadata::description: "SHA-256 256-bit digest of raw bytes, as computed by systems outside triblespace (TLS certificate fingerprints). The value stores the digest bytes.",
            metadata::tag: metadata::KIND_INLINE_ENCODING,
        }
    }
}

/// Stable extrinsic scope of the Home collection, named `home`.
///
/// Minted with `trible genid` on 2026-11-09:
/// `7FD800EA34E5C18C3E2B420EFE66F1E5`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("7FD800EA34E5C18C3E2B420EFE66F1E5");

/// Kind marker of a bridge binding.
///
/// Minted with `trible genid` on 2026-11-09:
/// `C0042D70FBA5D96F746D239121B53DD8`.
pub const KIND_BRIDGE: Id = id_hex!("C0042D70FBA5D96F746D239121B53DD8");

/// Kind marker of one state change sent to a bridge.
///
/// Minted with `trible genid` on 2026-11-09:
/// `F3C0472EB11C41C9DAD65D4F4C8224A7`.
pub const KIND_ACTION: Id = id_hex!("F3C0472EB11C41C9DAD65D4F4C8224A7");

pub mod home {
    use super::*;

    attributes! {
        /// The bridge's own `bridgeid`, lowercase hex (`001788fffe000001`).
        "0BDD674F2E955CCF6FDCD0D508962081" as bridge_id: Handle<UTF8String>;
        /// Host name or IP address the bridge answers HTTPS on.
        "EEAD0B51D6A16A234411E55F70B30366" as address: Handle<UTF8String>;
        /// Exact Secrets version holding the bridge username (API key).
        "F8A234DC62CAA271EF2C2247D9CD40B3" as username_secret: GenId;
        /// SHA-256 of the DER leaf certificate the bridge presented when it
        /// was bound; every connection is refused unless it presents the
        /// same one. Minted with `trible genid` on 2026-11-09.
        "FD25F831F60221FCDFB5783B5BF9497F" as certificate_sha256: Hash<Sha256>;
        /// The bridge resource an action addressed, e.g. `groups/3/action`.
        "B09BF469E0A8B236426999E86B9AB735" as target: Handle<UTF8String>;
        /// The target's Hue name when the action was sent.
        "64B2BB5AF5A8A3A2054B895AC38BA5C7" as target_name: Handle<UTF8String>;
        /// The JSON body sent.
        "54DFF80C3CEA0ECCCEEC6A92D27184FF" as request: Handle<UTF8String>;
        /// The bridge's JSON answer.
        "66C0BF7249665E5D015601B35AD0DC55" as response: Handle<UTF8String>;
    }
}
