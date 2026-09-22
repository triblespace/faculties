//! Config schema: the collection a pile keeps its own configuration in.
//!
//! A host needs to know which exact collection descriptor each faculty name
//! refers to. That mapping used to live in the process environment, and an
//! environment is a copy taken when the process started: it cannot be
//! invalidated, so a long-lived shell can carry a whole retired generation
//! while every host's own record of the mapping reads correctly. Measured on
//! 2026-09-22 -- one session held a retired message collection for hours while
//! the file beside it and its own watcher both held the current one, and the
//! only visible symptom was that a peer's messages never arrived.
//!
//! So the mapping lives in the pile, in one collection admitting only that
//! pile's own signing key for READ and WRITE. Two properties make that work
//! without a bootstrap fact:
//!
//! * Collection identity is the content address of its descriptor, and the
//!   descriptor embeds the admission policy. A policy naming this pile's key
//!   therefore yields a handle nobody else derives, so the collection is
//!   genuinely per-pile even though every host uses the same name.
//! * That handle is a pure function of the name and the key, both of which a
//!   process already holds before it opens anything. Nothing external has to
//!   remember it, which is the whole point: an external note of a handle is
//!   one more copy that can go stale.
//!
//! The pile path and the signing key stay outside, and they are the only two
//! that must: you cannot read a pile to discover where the pile is.

use triblespace::macros::id_hex;
use triblespace::prelude::*;

/// The stable name of every pile's own configuration collection.
///
/// The same name on every host, deliberately. The policy differs because the
/// key differs, so the handles differ; the name is what makes them recognisable
/// as the same thing playing the same role.
pub const COLLECTION_NAME: &str = "config";

/// Stable scope for the configuration collection.
///
/// It sits in the collection-name table like any other faculty root so that
/// opening it validates the descriptor's name the same way, but its handle is
/// never resolved *through* the configuration: it is always derived. The root
/// of a resolution cannot be resolvable by the thing it roots, and a stale
/// override there would make every other name stale with it.
///
/// Minted with `trible genid` on 2026-09-22:
/// `EDB5C6AC5E1219CEEB4C2E283BF7E4C8`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("EDB5C6AC5E1219CEEB4C2E283BF7E4C8");

pub mod config {
    use super::*;
    attributes! {
        /// Which faculty's configuration this state is a version of.
        ///
        /// The register's anchor. A faculty brings its own stable scope id and
        /// asks for the current state of *that* register, so configuration is
        /// keyed by identity rather than by a name -- a name would have to be
        /// hashed, could collide, and says nothing about which thing it is the
        /// configuration OF.
        ///
        /// This is the identity half the register module argues for: an
        /// attribute meaning "the configuration of faculty F", after which no
        /// scoping knob is wanted, because a state that is not in the register
        /// simply does not carry this fact.
        ///
        /// Minted with `trible genid` on 2026-09-22.
        "74027DA8641D5E3EECDC59089BF5D989" as anchor: inlineencodings::GenId;

        /// The exact collection descriptor this state selects.
        ///
        /// A descriptor handle rather than a label, for the same reason
        /// `collection_source` is one in the core: it names one exact
        /// collection and cannot claim to be something it is not.
        ///
        /// Minted with `trible genid` on 2026-09-22.
        "6408236D3CE210AD714FA2DA7EB9025C" as selects: inlineencodings::Handle<blobencodings::SimpleArchive>;
    }
}
