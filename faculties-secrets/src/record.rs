//! General structured records: one private relation plus the blobs it
//! references, sealed under one item key and delivered by one bound envelope.
//!
//! This is the path password-manager items use (`item` is one consumer that
//! puts `KIND_ITEM` in its private relation). Any other private TribleSet can
//! be sealed the same way: the builder seals each referenced blob with the
//! supplied [`ItemSealer`] and returns facts that point at those ciphertext
//! handles. The public record is the structured version (`KIND_ITEM_VERSION`,
//! its public label, creation time and ciphertext root) plus the envelope.
//!
//! Delivery follows the version's resource descriptor R: its own descriptor
//! ([`Delivery::Own`]) or a shared group descriptor ([`Delivery::Group`]),
//! which one grant over that R covers.

use super::*;
use crate::encrypted::{open_blob, Encrypted, ItemKey, ItemSealer};
use crate::schema::*;

/// Public ciphertext root of a structured version.
pub type RecordBody = Inline<inlineencodings::Handle<Encrypted<SimpleArchive>>>;

/// Which resource descriptor a new version's key is delivered under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
    /// A fresh descriptor for this version alone, rooted at the signer.
    Own,
    /// An existing group descriptor (see [`crate::resource::create_group`]).
    /// A grant over it covers every version sealed into it.
    Group(CollectionHandle),
}

/// Seal one private relation and the blobs it references as one immutable
/// version. `build` seals each referenced blob with the sealer (children
/// before parents) and returns the private facts; nothing else is carried.
pub fn seal(
    signer: &SigningKey,
    public_label: &str,
    created_at: IntervalValue,
    delivery: Delivery,
    build: impl FnOnce(&mut ItemSealer<'_>) -> Result<TribleSet>,
) -> Result<SealedVersion> {
    validate_name("public record label", public_label)?;
    point_value("record creation time", created_at)?;
    let key = ItemKey::generate(genid().id);
    let mut sealer = ItemSealer::new(&key);
    let facts = build(&mut sealer)?;
    let bundle = sealer.finish_facts(facts)?;
    crate::resource::seal_structured(signer, public_label, key, bundle, created_at, delivery)
}

/// A decrypted relation plus its held key, fixed reader and the resource it
/// was delivered under. Not an eager catalog; query `facts()` at the point of
/// use and open only the children you need.
pub struct OpenedRecord<R> {
    pub(crate) reader: R,
    pub(crate) key: ItemKey,
    pub(crate) facts: TribleSet,
    pub(crate) body: RecordBody,
    /// None for a version opened through a user-protected envelope.
    pub(crate) resource: Option<CollectionHandle>,
}

impl<R: BlobStoreGet> OpenedRecord<R> {
    pub fn facts(&self) -> &TribleSet {
        &self.facts
    }
    pub fn body(&self) -> RecordBody {
        self.body
    }
    pub fn key_id(&self) -> Id {
        self.key.id()
    }
    /// The resource descriptor the opened envelope bound (own or group).
    pub fn resource(&self) -> Option<CollectionHandle> {
        self.resource
    }

    /// Missing or invalid selected children fail here, not when opening other
    /// fields or the root. No discovery or implicit cleartext cache is involved.
    pub fn open_child<T: BlobEncoding>(
        &self,
        handle: Inline<inlineencodings::Handle<Encrypted<T>>>,
    ) -> Result<Blob<T>> {
        let blob = self
            .reader
            .get::<Blob<Encrypted<T>>, _>(handle)
            .map_err(|_| anyhow!("selected ciphertext child is unavailable"))?;
        open_blob(&self.key, blob)
    }
}

/// Every opened body alternative for one version id this holder can open,
/// possibly none. Shared by [`open`] and `item::open`.
pub(crate) fn open_all<R: BlobStoreGet + Clone, P: TriblePattern>(
    reader: R,
    facts: &P,
    id: Id,
    holder: &SigningKey,
) -> Result<Vec<OpenedRecord<R>>> {
    let mut seen = BTreeSet::new();
    let mut records = Vec::new();
    for binding in super::envelope::recover_item(&reader, facts, id, holder)? {
        let body: RecordBody = binding.body.transmute();
        if !exists!(pattern!(facts, [{ id @ metadata::tag: KIND_ITEM_VERSION,
            structured_body: body }]))
            || !seen.insert(body)
        {
            continue;
        }
        let key = ItemKey::from_dek(id, binding.dek);
        let ciphertext = reader
            .get::<Blob<Encrypted<SimpleArchive>>, _>(body)
            .map_err(|_| anyhow!("encrypted record root is unavailable"))?;
        let facts: TribleSet = open_blob(&key, ciphertext)?
            .try_from_blob()
            .map_err(|_| anyhow!("encrypted record root is not an archive"))?;
        records.push(OpenedRecord {
            reader: reader.clone(),
            key,
            facts,
            body,
            resource: Some(binding.resource),
        });
    }
    records.sort_by_key(|record| record.body);
    Ok(records)
}

/// Every usable body alternative for this opaque version id, keyed by exact H.
pub fn open<R: BlobStoreGet + Clone, P: TriblePattern>(
    reader: R,
    facts: &P,
    id: Id,
    holder: &SigningKey,
) -> Result<Vec<OpenedRecord<R>>> {
    let records = open_all(reader, facts, id, holder)?;
    if records.is_empty() {
        bail!("no resident decryptable structured record for this holder");
    }
    Ok(records)
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
