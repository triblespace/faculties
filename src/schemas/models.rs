//! The shared model collection: every model a faculty loads, as a root in it.
//!
//! One collection holds every model a faculty loads (Breeze for speech,
//! Voxtral for hearing), each as an ordinary Mary model root selected by its
//! `source` and `quantization` labels. It is named `models` and shared across
//! hosts by its descriptor's own READ and WRITE policy, so a
//! host reaches it only through the exact handle its launcher configures
//! (`TRIBLESPACE_COLLECTION_MODELS`), never by a signer-private default.

use triblespace::macros::id_hex;
use triblespace::prelude::*;

/// Stable scope id of the shared model collection, the key
/// `collection_names::table` reads it by. Minted with `trible genid` on
/// 2026-10-11: `12554446C65A23B4A3047266FD29DE94`.
pub const COLLECTION_SCOPE_ID: Id = id_hex!("12554446C65A23B4A3047266FD29DE94");

/// The collection's name.
pub const COLLECTION_NAME: &str = "models";
