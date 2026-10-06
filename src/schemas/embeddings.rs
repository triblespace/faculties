//! Nomic's 768-dimensional text+vision space, as the posture policy still
//! stores its exemplars: one dimension-typed [`Embedding768`] blob encoding
//! and one [`attr::embedding`] attribute. The dimension is part of the type,
//! so a vector of any other width fails to decode.
//!
//! Files, Memory and Wiki search with one WeMM space instead (`crate::wemm`).
//! The 768-d observations they wrote here remain in the pile, unread.

use anybytes::View;
use triblespace::core::blob::{Blob, BlobEncoding, TryFromBlob};
use triblespace::core::id::ExclusiveId;
use triblespace::core::inline::{Encodes, InlineEncoding};
use triblespace::core::metadata::{self, MetaDescribe};
use triblespace::core::trible::Fragment;
use triblespace::macros::id_hex;
use triblespace::prelude::*;

/// Dimension of the shared space (nomic-embed-{text,vision}-v1.5).
pub const DIM: usize = 768;

/// Stable extrinsic scope for signed observations in the shared nomic
/// text+vision embedding space.
///
/// Minted with `trible genid` on 2026-08-09:
/// `F6BE4C16A56001FEA03A5927C6ED3814`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("F6BE4C16A56001FEA03A5927C6ED3814");

// ── dimension-typed embedding encoding ────────────────────────────────────

/// Error decoding a dimension-typed embedding blob.
#[derive(Debug)]
pub enum EmbeddingDimError {
    /// The blob held a different number of floats than the type's dimension.
    WrongLen { expected: usize, got: usize },
    /// The bytes couldn't be viewed as `[f32]` (misalignment / bad length).
    View(anybytes::view::ViewError),
}

impl std::fmt::Display for EmbeddingDimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongLen { expected, got } => {
                write!(f, "embedding has {got} floats, expected {expected}")
            }
            Self::View(e) => write!(f, "embedding view: {e}"),
        }
    }
}
impl std::error::Error for EmbeddingDimError {}

/// A 768-d L2-normalized embedding in the shared nomic space, length-validated
/// on read so a foreign-dimension vector can never enter the index. Same wire
/// format as `triblespace_search::Embedding` (raw f32 LE), but reads check the
/// width — so a 512-d CLIP or 1152-d SigLIP vector simply fails to decode here,
/// at compile time (distinct `Handle<_>`) and at read time (the check below).
pub struct Embedding768;

impl BlobEncoding for Embedding768 {}

impl MetaDescribe for Embedding768 {
    fn describe() -> Fragment {
        let id = id_hex!("D135AA8404D09D112E5BD206494190C4");
        entity! { ExclusiveId::force_ref(&id) @
            metadata::name: "Embedding768",
            metadata::description: "768-d [f32] LE embedding blob in the shared nomic text+vision space (nomic-embed-{text,vision}-v1.5). L2-normalized; length-validated on read so it can never be mixed with another embedding dimension in one HNSW index.",
            metadata::tag: metadata::KIND_BLOB_ENCODING,
        }
    }
}

impl TryFromBlob<Embedding768> for View<[f32]> {
    type Error = EmbeddingDimError;
    fn try_from_blob(b: Blob<Embedding768>) -> Result<Self, Self::Error> {
        let floats = b.bytes.len() / 4;
        if floats != DIM {
            return Err(EmbeddingDimError::WrongLen {
                expected: DIM,
                got: floats,
            });
        }
        b.bytes.view().map_err(EmbeddingDimError::View)
    }
}

impl Encodes<Vec<f32>> for Embedding768
where
    inlineencodings::Handle<Embedding768>: InlineEncoding,
{
    type Output = Blob<Embedding768>;
    fn encode(source: Vec<f32>) -> Blob<Embedding768> {
        let mut bytes = Vec::with_capacity(source.len() * 4);
        for v in &source {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        Blob::new(bytes.into())
    }
}

// ── the canonical embedding attribute ──────────────────────────────────────
// One attribute, reused across files, photos, and memory chunks — like
// `metadata::name`, it's a cross-cutting property, not owned by any one
// faculty. "This entity has a position in the shared multimodal space."

pub mod attr {
    use super::*;
    attributes! {
        "BCDCA79081A84E7428A2D06A7F222313" unsafe as embedding: inlineencodings::Handle<super::Embedding768>;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding768_roundtrips_and_rejects_wrong_dim() {
        let v: Vec<f32> = (0..DIM).map(|i| i as f32 * 0.001).collect();
        let blob = <Embedding768 as Encodes<Vec<f32>>>::encode(v.clone());
        let back: View<[f32]> =
            <View<[f32]> as TryFromBlob<Embedding768>>::try_from_blob(blob).unwrap();
        assert_eq!(back.as_ref(), v.as_slice(), "768-d round-trips byte-exact");

        // A foreign-dimension vector (e.g. a 512-d CLIP leftover) must NOT
        // decode — the width is validated on read, so it can never slip into
        // the shared index.
        let wrong: Vec<f32> = vec![0.0; 512];
        let blob = <Embedding768 as Encodes<Vec<f32>>>::encode(wrong);
        let err = <View<[f32]> as TryFromBlob<Embedding768>>::try_from_blob(blob);
        assert!(
            matches!(
                err,
                Err(EmbeddingDimError::WrongLen {
                    expected: 768,
                    got: 512
                })
            ),
            "wrong dimension is rejected on read"
        );
    }
}
