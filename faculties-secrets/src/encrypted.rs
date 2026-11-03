//! Typed authenticated ciphertext, with public conservative child references.
//!
//! Key identity, lengths and graph topology are public. One key opens the whole
//! item, not selected fields. This is not an implicit decrypting blob store.

use std::marker::PhantomData;

use anyhow::{anyhow, bail, Result};
use chacha20poly1305::{
    aead::{Aead, AeadInPlace, Payload},
    KeyInit, Tag, XChaCha20Poly1305, XNonce,
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
use zeroize::{DefaultIsZeroes, Zeroizing};

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

/// One 16-byte, 16-aligned plaintext block. Archive readers view the bytes in
/// place as 16-aligned tribles and refuse anything else, so the alignment is a
/// property of this element type, never of whatever the allocator returned.
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct ClearBlock([u8; 16]);
impl DefaultIsZeroes for ClearBlock {}

// Plaintext lives only in this zeroizing, block-aligned allocation. The Vec is
// never grown after construction, so its address is stable when the owner
// moves, and AnyBytes retains this owner until the last cloned/sliced byte view
// is dropped. Dropping it zeroizes the whole capacity, tail padding included.
struct ClearBytes {
    blocks: Zeroizing<Vec<ClearBlock>>,
    len: usize,
}

impl ClearBytes {
    fn zeroed(len: usize) -> Self {
        Self {
            blocks: Zeroizing::new(vec![ClearBlock::default(); len.div_ceil(16)]),
            len,
        }
    }

    fn as_mut_bytes(&mut self) -> &mut [u8] {
        // SAFETY: ClearBlock is repr(C) over [u8; 16] (size 16, no padding),
        // the Vec holds len.div_ceil(16) initialized blocks, so the first
        // `len` bytes are in bounds, initialized, and uniquely borrowed here.
        unsafe { std::slice::from_raw_parts_mut(self.blocks.as_mut_ptr().cast(), self.len) }
    }
}

unsafe impl anybytes::ByteSource for ClearBytes {
    type Owner = Self;
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: as in `as_mut_bytes`, shared instead of unique.
        unsafe { std::slice::from_raw_parts(self.blocks.as_ptr().cast(), self.len) }
    }
    fn get_owner(self) -> Self {
        self
    }
}

/// Copy already-decrypted plaintext once into a 16-aligned zeroizing owner,
/// so it can be decoded as an archive. The caller still owns and must zeroize
/// `plaintext` (e.g. hold it in `Zeroizing`); no other copy is made.
pub fn clear_bytes(plaintext: &[u8]) -> Bytes {
    let mut clear = ClearBytes::zeroed(plaintext.len());
    clear.as_mut_bytes().copy_from_slice(plaintext);
    Bytes::from_source(clear)
}

/// Authenticate the expected encoding and complete public header, then open.
/// The result has its own plaintext hash and a 16-aligned zeroizing backing
/// owner; decryption happens in place there, so no other plaintext buffer
/// exists. This cannot erase caller copies, plaintext inputs or decoded
/// TribleSet allocations.
pub fn open_blob<T: BlobEncoding>(key: &ItemKey, blob: Blob<Encrypted<T>>) -> Result<Blob<T>> {
    let bytes = blob.bytes.as_ref();
    let header = header_len(bytes)?;
    if bytes[16..32] != key.id.raw() {
        bail!("encrypted blob key identity mismatch");
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key.dek.as_slice())
        .map_err(|_| anyhow!("invalid item key"))?;
    // header_len guarantees the 16-byte Poly1305 tag follows the header.
    let (ciphertext, tag) = bytes[header..].split_at(bytes.len() - header - 16);
    let mut clear = ClearBytes::zeroed(ciphertext.len());
    clear.as_mut_bytes().copy_from_slice(ciphertext);
    // The tag is verified before the keystream is applied; on failure `clear`
    // still holds only ciphertext and is zeroized on drop anyway.
    cipher
        .decrypt_in_place_detached(
            XNonce::from_slice(&bytes[32..56]),
            &associated_data::<T>(&bytes[..header]),
            clear.as_mut_bytes(),
            Tag::from_slice(tag),
        )
        .map_err(|_| anyhow!("encrypted blob authentication failed"))?;
    Ok(Blob::new(Bytes::from_source(clear)))
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
    fn opened_and_copied_plaintext_is_aligned_by_type_for_every_length() {
        let key = ItemKey::generate(genid().id);
        for len in (0..=67).chain([255, 256, 257, 4095, 4096, 4097]) {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let cipher =
                seal_blob::<blobencodings::RawBytes>(&key, plain.clone().to_blob(), &[]).unwrap();
            let opened = open_blob(&key, cipher).unwrap();
            assert_eq!(opened.bytes.as_ref(), plain.as_slice(), "len {len}");
            assert_eq!(
                opened.bytes.as_ptr() as usize % 16,
                0,
                "open_blob len {len}"
            );
            let copied = clear_bytes(&plain);
            assert_eq!(copied.as_ref(), plain.as_slice(), "len {len}");
            assert_eq!(copied.as_ptr() as usize % 16, 0, "clear_bytes len {len}");
        }
        // Even from a deliberately misaligned source view.
        let source = vec![0u8; 65];
        let copied = clear_bytes(&source[1..]);
        assert_eq!(copied.as_ptr() as usize % 16, 0);
    }

    #[test]
    fn archive_decodes_from_a_copy_of_misaligned_plaintext() {
        let facts = entity! { metadata::name: "synthetic-aligned-archive" }.into_facts();
        let archive: Blob<SimpleArchive> = facts.clone().to_blob();
        let mut shifted = vec![0u8; archive.bytes.len() + 1];
        shifted[1..].copy_from_slice(archive.bytes.as_ref());
        // The reader refuses the misaligned view itself, so the copy matters.
        let misaligned = Bytes::from_source(shifted.clone()).slice(1..);
        assert_ne!(misaligned.as_ptr() as usize % 16, 0);
        assert!(Blob::<SimpleArchive>::new(misaligned)
            .try_from_blob::<TribleSet>()
            .is_err());
        let decoded: TribleSet = Blob::<SimpleArchive>::new(clear_bytes(&shifted[1..]))
            .try_from_blob()
            .unwrap();
        assert_eq!(decoded, facts);
    }

    #[test]
    fn clear_owner_zeroizes_its_whole_aligned_capacity() {
        fn zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        // Drop of the owner is Zeroizing's drop; this is what it runs.
        zeroize_on_drop::<Zeroizing<Vec<ClearBlock>>>();
        let mut clear = ClearBytes::zeroed(21);
        clear.as_mut_bytes().fill(0xa5);
        clear.blocks.last_mut().unwrap().0[15] = 0x5a; // tail padding too
        let capacity = clear.blocks.capacity() * 16;
        assert_eq!(capacity, 32);
        zeroize::Zeroize::zeroize(&mut *clear.blocks);
        // SAFETY: the allocation is still owned by `clear` (zeroize clears the
        // length, not the capacity) and every byte was just written by it.
        let all =
            unsafe { std::slice::from_raw_parts(clear.blocks.as_ptr().cast::<u8>(), capacity) };
        assert!(all.iter().all(|byte| *byte == 0));
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
