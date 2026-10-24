//! Typed authenticated ciphertext, with public conservative child references.
//!
//! Key identity, lengths and graph topology are public. One key opens the whole
//! item, not selected fields. This is not an implicit decrypting blob store.

use std::marker::PhantomData;

use anyhow::{anyhow, bail, Result};
use chacha20poly1305::{
    aead::{Aead, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use dryoc::{
    dryocsecretbox::{Key, Nonce},
    types::{Bytes as _, NewByteArray},
};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::{Blob, BlobEncoding, Bundle, Bytes, MemoryBlobStore};
use triblespace::core::metadata::{self, MetaDescribe};
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::*;
use zeroize::Zeroizing;

/// XChaCha20-Poly1305 with this module's aligned, authenticated reference header.
/// Minted with installed `trible genid` on 2026-10-17.
pub const FORMAT: Id = id_hex!("8C711D518C0D28347A76E1B641ED1509");

/// A format marker, parameterized by the expected plaintext blob encoding.
pub struct Encrypted<T: BlobEncoding>(PhantomData<T>);

impl<T: BlobEncoding> BlobEncoding for Encrypted<T> {}

impl<T: BlobEncoding> MetaDescribe for Encrypted<T> {
    fn describe() -> Fragment {
        let mut description = entity! {
            metadata::tag*: [metadata::KIND_BLOB_ENCODING, FORMAT],
            metadata::blob_encoding*: T::describe(),
        };
        let id = description.root().expect("rooted encoding description");
        description += entity! { ExclusiveId::force_ref(&id) @
            metadata::name: "encrypted",
            metadata::description: "Typed XChaCha20-Poly1305 ciphertext with authenticated public key identity and aligned ciphertext child handles. Values are private; lengths and ciphertext graph topology are public.",
        };
        description
    }
}

/// One opaque public identity and a zeroizing secret key. No Debug/Clone/export.
pub struct ItemKey {
    id: Id,
    dek: Key,
}

impl ItemKey {
    pub fn generate(id: Id) -> Self {
        Self {
            id,
            dek: Key::gen(),
        }
    }

    pub fn id(&self) -> Id {
        self.id
    }

    pub(crate) fn from_dek(id: Id, dek: Key) -> Self {
        Self { id, dek }
    }
    pub(crate) fn into_dek(self) -> Key {
        self.dek
    }
}

/// An explicitly supplied ciphertext dependency, erased only at the type level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CiphertextRef([u8; 32]);

impl<T: BlobEncoding> From<Inline<inlineencodings::Handle<Encrypted<T>>>> for CiphertextRef {
    fn from(handle: Inline<inlineencodings::Handle<Encrypted<T>>>) -> Self {
        Self(handle.raw)
    }
}

fn header_len(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < 64 + 16 || bytes[..16] != FORMAT.raw() {
        bail!("invalid encrypted blob framing");
    }
    let count = u64::from_le_bytes(bytes[56..64].try_into().unwrap());
    let len = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(32))
        .and_then(|len| len.checked_add(64))
        .filter(|len| *len <= bytes.len() - 16)
        .ok_or_else(|| anyhow!("invalid encrypted blob reference bounds"))?;
    Ok(len)
}

/// An UNAUTHENTICATED routing hint only. Framing is bounded, but only
/// `open_blob` authenticates identity, type, references and ciphertext.
pub fn peek_key_id(bytes: &[u8]) -> Result<Id> {
    header_len(bytes)?;
    Id::new(bytes[16..32].try_into().unwrap())
        .ok_or_else(|| anyhow!("invalid encrypted blob key identity"))
}

fn associated_data<T: BlobEncoding>(header: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(16 + header.len());
    aad.extend_from_slice(&T::id().raw());
    aad.extend_from_slice(header);
    aad
}

/// Seal exactly these plaintext bytes and explicit ciphertext dependencies.
/// Does not discover references, rewrite handles, or fetch any dependencies.
pub fn seal_blob<T: BlobEncoding>(
    key: &ItemKey,
    plaintext: Blob<T>,
    children: &[CiphertextRef],
) -> Result<Blob<Encrypted<T>>> {
    let count =
        u64::try_from(children.len()).map_err(|_| anyhow!("too many ciphertext children"))?;
    let capacity = children
        .len()
        .checked_mul(32)
        .and_then(|n| n.checked_add(64))
        .ok_or_else(|| anyhow!("too many ciphertext children"))?;
    let nonce = Nonce::gen();
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(&FORMAT.raw());
    bytes.extend_from_slice(&key.id.raw());
    bytes.extend_from_slice(nonce.as_slice());
    bytes.extend_from_slice(&count.to_le_bytes());
    for child in children {
        bytes.extend_from_slice(&child.0);
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key.dek.as_slice())
        .map_err(|_| anyhow!("invalid item key"))?;
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(nonce.as_slice()),
            Payload {
                msg: plaintext.bytes.as_ref(),
                aad: &associated_data::<T>(&bytes),
            },
        )
        .map_err(|_| anyhow!("cannot seal encrypted blob"))?;
    bytes.extend_from_slice(&ciphertext);
    Ok(Blob::new(bytes.into()))
}

// The immutable Vec allocation stays at the same address when its owner moves.
// AnyBytes retains this owner until the last cloned/sliced byte view is dropped.
struct ClearBytes(Zeroizing<Vec<u8>>);
unsafe impl anybytes::ByteSource for ClearBytes {
    type Owner = Self;
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    fn get_owner(self) -> Self {
        self
    }
}

/// Authenticate the expected encoding and complete public header, then open.
/// The result has its own plaintext hash and a zeroizing backing owner. This
/// cannot erase caller copies, plaintext inputs or decoded TribleSet allocations.
pub fn open_blob<T: BlobEncoding>(key: &ItemKey, blob: Blob<Encrypted<T>>) -> Result<Blob<T>> {
    let bytes = blob.bytes.as_ref();
    let header = header_len(bytes)?;
    if bytes[16..32] != key.id.raw() {
        bail!("encrypted blob key identity mismatch");
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key.dek.as_slice())
        .map_err(|_| anyhow!("invalid item key"))?;
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&bytes[32..56]),
            Payload {
                msg: &bytes[header..],
                aad: &associated_data::<T>(&bytes[..header]),
            },
        )
        .map_err(|_| anyhow!("encrypted blob authentication failed"))?;
    Ok(Blob::new(Bytes::from_source(ClearBytes(Zeroizing::new(
        plaintext,
    )))))
}

/// Tiny authoring inventory: only ciphertext created here can be carried out.
/// No arbitrary Fragment attachments or mutable store are accepted.
pub struct ItemSealer<'a> {
    key: &'a ItemKey,
    blobs: MemoryBlobStore,
}

impl<'a> ItemSealer<'a> {
    pub fn new(key: &'a ItemKey) -> Self {
        Self {
            key,
            blobs: MemoryBlobStore::new(),
        }
    }

    /// Children must already have been sealed by this inventory (children first).
    pub fn seal_blob<T: BlobEncoding>(
        &mut self,
        plaintext: Blob<T>,
        children: &[CiphertextRef],
    ) -> Result<Inline<inlineencodings::Handle<Encrypted<T>>>> {
        let snapshot = self.blobs.snapshot().expect("infallible memory snapshot");
        for child in children {
            let handle =
                Inline::<inlineencodings::Handle<blobencodings::UnknownBlob>>::new(child.0);
            if snapshot.get::<Bytes, _>(handle).is_err() {
                bail!("ciphertext child is not in this item inventory");
            }
        }
        Ok(self.blobs.insert(seal_blob(self.key, plaintext, children)?))
    }

    /// Archive content facts only, with every explicitly produced ciphertext as
    /// a conservative root dependency. Pass `private.into_facts()` deliberately:
    /// exports, metafacts and the Fragment's shared (possibly plaintext) store
    /// are not archived or carried. Required out-of-line values must be sealed
    /// explicitly; this method never rewrites plaintext handles.
    pub fn finish_facts(mut self, facts: TribleSet) -> Result<Bundle<Encrypted<SimpleArchive>>> {
        let children = self
            .blobs
            .snapshot()
            .expect("infallible memory snapshot")
            .iter()
            .map(|(handle, _)| CiphertextRef(handle.raw))
            .collect::<Vec<_>>();
        let root = seal_blob(self.key, facts.to_blob(), &children)?;
        Ok(Bundle::new(root, self.blobs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opened_plaintext_views_keep_the_zeroizing_owner_alive() {
        let key = ItemKey::generate(genid().id);
        let cipher = seal_blob::<blobencodings::RawBytes>(
            &key,
            b"prefix retained suffix".to_vec().to_blob(),
            &[],
        )
        .unwrap();
        let opened = open_blob(&key, cipher).unwrap();
        let clone = opened.clone();
        let retained = clone.bytes.slice(7..15);
        drop(opened);
        drop(clone);
        assert_eq!(retained.as_ref(), b"retained");
        let last_view = retained.clone();
        drop(retained);
        assert_eq!(last_view.as_ref(), b"retained");
        drop(last_view);
    }

    #[test]
    fn typed_roundtrip_uses_distinct_hashes_and_fresh_nonces() {
        let key = ItemKey::generate(genid().id);
        let plain: Blob<blobencodings::UTF8String> = "synthetic-private-value".to_owned().to_blob();
        let original = plain.get_handle().raw;
        let first = seal_blob(&key, plain.clone(), &[]).unwrap();
        let second = seal_blob(&key, plain, &[]).unwrap();
        assert_ne!(first.get_handle().raw, original);
        assert_ne!(first.get_handle(), second.get_handle());
        assert_ne!(&first.bytes[32..56], &second.bytes[32..56]);
        assert_eq!(peek_key_id(first.bytes.as_ref()).unwrap(), key.id());
        let opened = open_blob(&key, first.clone()).unwrap();
        assert_eq!(opened.get_handle().raw, original);
        assert_eq!(opened.bytes.as_ref(), b"synthetic-private-value");
        assert!(open_blob(
            &key,
            first.transmute::<Encrypted<blobencodings::RawBytes>>()
        )
        .is_err());
    }

    #[test]
    fn every_framing_dimension_is_authenticated_and_bounded() {
        let key = ItemKey::generate(genid().id);
        let child = seal_blob::<blobencodings::RawBytes>(&key, vec![1, 2].to_blob(), &[]).unwrap();
        let cipher = seal_blob::<blobencodings::RawBytes>(
            &key,
            vec![3, 4, 5].to_blob(),
            &[child.get_handle().into()],
        )
        .unwrap();
        let bytes = cipher.bytes.as_ref();
        assert_eq!(&bytes[64..96], &child.get_handle().raw);
        for offset in [0, 16, 32, 56, 64, 96, bytes.len() - 1] {
            let mut altered = bytes.to_vec();
            altered[offset] ^= 1;
            let blob = Blob::<Encrypted<blobencodings::RawBytes>>::new(altered.into());
            assert!(
                open_blob(&key, blob).is_err(),
                "accepted tamper at {offset}"
            );
        }
        for len in 0..bytes.len() {
            assert!(open_blob(
                &key,
                Blob::<Encrypted<blobencodings::RawBytes>>::new(bytes[..len].to_vec().into())
            )
            .is_err());
        }
        let mut trailing = bytes.to_vec();
        trailing.push(0);
        assert!(open_blob(
            &key,
            Blob::<Encrypted<blobencodings::RawBytes>>::new(trailing.into())
        )
        .is_err());
        let mut overflow = bytes.to_vec();
        overflow[56..64].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(peek_key_id(&overflow).is_err());
        assert!(open_blob(&ItemKey::generate(key.id()), cipher.clone()).is_err());
        let wrong_id = ItemKey::from_dek(genid().id, key.dek.clone());
        assert!(open_blob(&wrong_id, cipher).is_err());
    }

    #[test]
    fn inventory_carries_ciphertext_only_and_does_not_rewrite_plaintext_handles() {
        let key = ItemKey::generate(genid().id);
        let mut sealer = ItemSealer::new(&key);
        let text = sealer
            .seal_blob::<blobencodings::UTF8String>(
                "synthetic-private-child".to_owned().to_blob(),
                &[],
            )
            .unwrap();
        let private = entity! { metadata::name: "synthetic-discarded-schema-and-content" };
        assert!(!private.blobs().is_empty());
        let expected = private.facts().clone();
        let bundle = sealer.finish_facts(private.into_facts()).unwrap();
        assert_eq!(bundle.blobs().len(), 2);
        let mut carried = bundle.blobs().clone();
        let snapshot = carried.snapshot().unwrap();
        let cipher = snapshot
            .get::<Blob<Encrypted<SimpleArchive>>, _>(bundle.root())
            .unwrap();
        assert_eq!(&cipher.bytes[64..96], &text.raw);
        let opened: TribleSet = open_blob(&key, cipher).unwrap().try_from_blob().unwrap();
        assert_eq!(opened, expected);
        for (_, blob) in snapshot.iter() {
            assert!(!blob
                .bytes
                .windows(b"synthetic-".len())
                .any(|w| w == b"synthetic-"));
        }
        let unrelated = seal_blob::<blobencodings::RawBytes>(&key, vec![8].to_blob(), &[]).unwrap();
        assert!(ItemSealer::new(&key)
            .seal_blob::<blobencodings::RawBytes>(
                vec![9].to_blob(),
                &[unrelated.get_handle().into()]
            )
            .is_err());
    }
}
