//! Structured Secrets are native private relations, not a persisted Rust catalog.
//!
//! Authoring inputs below are transient. Readers query `OpenedItem::facts()` at
//! the point of use and open only selected ciphertext children. Repeated roles,
//! unknown facts and opaque entity ids are ordinary open-world data.

use super::*;
use crate::encrypted::{open_blob, Encrypted, ItemKey, ItemSealer};
use crate::schema::*;

pub type ItemBody = Inline<inlineencodings::Handle<Encrypted<SimpleArchive>>>;
pub type PrivateText = Inline<inlineencodings::Handle<Encrypted<blobencodings::UTF8String>>>;

#[derive(Clone, Copy)]
pub enum FieldValue<'a> {
    Text(&'a str),
    Integer(i128),
    Flag(bool),
}

pub struct FieldInput<'a> {
    pub role: Id,
    pub label: Option<&'a str>,
    pub value: FieldValue<'a>,
}

pub struct AttachmentInput<'a> {
    pub filename: &'a str,
    pub bytes: &'a [u8],
}

#[derive(Default)]
pub struct ItemInput<'a> {
    pub fields: &'a [FieldInput<'a>],
    pub attachments: &'a [AttachmentInput<'a>],
}

/// Metadata projection only; never opens ciphertext or includes private values.
#[derive(Clone, Copy, Debug)]
pub struct ItemRow {
    pub id: Id,
    pub name: TextHandle,
    pub created_at: IntervalValue,
    pub body: ItemBody,
}

pub fn rows<P: TriblePattern>(facts: &P) -> Vec<ItemRow> {
    find!((id: Id, name: TextHandle, created_at: IntervalValue, body: ItemBody),
        pattern!(facts, [{ ?id @ metadata::tag: KIND_ITEM_VERSION,
            metadata::name: ?name, metadata::created_at: ?created_at, structured_body: ?body }]))
    .filter_map(|(id, name, created_at, body)| {
        point_value("item creation time", created_at)
            .is_ok()
            .then_some(ItemRow {
                id,
                name,
                created_at,
                body,
            })
    })
    .collect()
}

/// Prepare all ciphertext, immutable authority and the initial holder envelope.
/// The sole public label is explicit; private field labels/filenames are sealed.
pub fn prepare(
    signer: &SigningKey,
    public_label: &str,
    input: ItemInput<'_>,
    created_at: IntervalValue,
) -> Result<SealedVersion> {
    validate_name("public item label", public_label)?;
    point_value("item creation time", created_at)?;
    let key = ItemKey::generate(genid().id);
    let mut sealer = ItemSealer::new(&key);
    let mut fields = Fragment::empty();
    for field in input.fields {
        let label = field
            .label
            .map(|label| {
                sealer.seal_blob::<blobencodings::UTF8String>(label.to_owned().to_blob(), &[])
            })
            .transpose()?;
        // The value participates in the field identity: construct one complete
        // entity rather than treating role/label as a uniqueness constraint.
        let facts = match field.value {
            FieldValue::Text(text) => {
                let value = sealer
                    .seal_blob::<blobencodings::UTF8String>(text.to_owned().to_blob(), &[])?;
                entity! { field_role: field.role, private_name?: label, field_text: value }
            }
            FieldValue::Integer(value) => {
                entity! { field_role: field.role, private_name?: label, field_integer: value }
            }
            FieldValue::Flag(value) => {
                entity! { field_role: field.role, private_name?: label, field_flag: value }
            }
        };
        fields += facts;
    }
    let mut attachments = Fragment::empty();
    for attachment in input.attachments {
        let name = sealer.seal_blob::<blobencodings::UTF8String>(
            attachment.filename.to_owned().to_blob(),
            &[],
        )?;
        let bytes = sealer
            .seal_blob::<blobencodings::RawBytes>(attachment.bytes.to_vec().to_blob(), &[])?;
        attachments += entity! { private_name: name, attachment_bytes: bytes };
    }
    let private = entity! { metadata::tag: KIND_ITEM,
    item_field*: fields, item_attachment*: attachments };
    let bundle = sealer.finish_facts(private.into_facts())?;
    super::resource::seal_item_bundle(signer, public_label, key, bundle, created_at)
}

/// A decrypted relation plus its held key and fixed reader, not an eager field
/// catalog. The backing decrypted blob zeroizes; decoded relations and caller
/// copies cannot be promised erased. Do not print this object as metadata.
pub struct OpenedItem<R> {
    reader: R,
    key: ItemKey,
    facts: TribleSet,
    body: ItemBody,
}

impl<R: BlobStoreGet> OpenedItem<R> {
    pub fn facts(&self) -> &TribleSet {
        &self.facts
    }
    pub fn body(&self) -> ItemBody {
        self.body
    }
    pub fn key_id(&self) -> Id {
        self.key.id()
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

    /// Generate a current code from one explicitly selected field-text child.
    /// Labels, attachments and unrelated same-key blobs are not field values.
    pub fn totp(&self, child: PrivateText) -> Result<crate::totp::Code> {
        self.totp_with_time(child, None)
    }

    /// Controlled-time SDK counterpart; native CLI/MCP expose current time only.
    pub fn totp_at(&self, child: PrivateText, unix_seconds: u64) -> Result<crate::totp::Code> {
        self.totp_with_time(child, Some(unix_seconds))
    }

    fn totp_with_time(&self, child: PrivateText, unix: Option<u64>) -> Result<crate::totp::Code> {
        if !exists!(pattern!(&self.facts, [
            { metadata::tag: KIND_ITEM, item_field: _?field },
            { _?field @ field_text: child },
        ])) {
            bail!("selected TOTP child is not an item text field");
        }
        let plaintext = self.open_child(child)?;
        let text = std::str::from_utf8(&plaintext.bytes)
            .map_err(|_| anyhow!("selected TOTP field is not UTF-8"))?;
        crate::totp::generate(text, unix)
    }
}

/// Every usable body alternative for this opaque version id, keyed by exact H.
/// An admitted competing body is another relation, never an iteration-order win.
pub fn open<R: BlobStoreGet + Clone, P: TriblePattern>(
    reader: R,
    facts: &P,
    id: Id,
    holder: &SigningKey,
) -> Result<Vec<OpenedItem<R>>> {
    let mut seen = BTreeSet::new();
    let mut items = Vec::new();
    for binding in super::envelope::recover_item(&reader, facts, id, holder)? {
        let body: ItemBody = binding.body.transmute();
        if !exists!(pattern!(facts, [{ id @ metadata::tag: KIND_ITEM_VERSION,
            structured_body: body }]))
            || !seen.insert(body)
        {
            continue;
        }
        let key = ItemKey::from_dek(id, binding.dek);
        let ciphertext = reader
            .get::<Blob<Encrypted<SimpleArchive>>, _>(body)
            .map_err(|_| anyhow!("encrypted item root is unavailable"))?;
        let facts: TribleSet = open_blob(&key, ciphertext)?
            .try_from_blob()
            .map_err(|_| anyhow!("encrypted item root is not an archive"))?;
        if !exists!(pattern!(&facts, [{ metadata::tag: KIND_ITEM }])) {
            continue;
        }
        items.push(OpenedItem {
            reader: reader.clone(),
            key,
            facts,
            body,
        });
    }
    if items.is_empty() {
        bail!("no resident decryptable structured item for this holder");
    }
    items.sort_by_key(|item| item.body);
    Ok(items)
}

/// Select one exact advertised ciphertext body, not a derived-id lookup.
pub fn open_exact<R: BlobStoreGet + Clone, P: TriblePattern>(
    reader: R,
    facts: &P,
    id: Id,
    body: ItemBody,
    holder: &SigningKey,
) -> Result<OpenedItem<R>> {
    open(reader, facts, id, holder)?
        .into_iter()
        .find(|item| item.body == body)
        .ok_or_else(|| anyhow!("selected encrypted item body is unavailable"))
}

impl<R: BlobStoreGet + Clone> SecretsSnapshot<R> {
    /// Consume the fixed snapshot so the opened item keeps its exact reader.
    pub fn open_item(self, id: Id, holder: &SigningKey) -> Result<Vec<OpenedItem<R>>> {
        let facts = self
            .facts
            .ok_or_else(|| anyhow!("structured item is not resident"))?;
        open(self.store_snapshot, &facts, id, holder)
    }

    pub fn open_item_body(
        self,
        id: Id,
        body: ItemBody,
        holder: &SigningKey,
    ) -> Result<OpenedItem<R>> {
        let facts = self
            .facts
            .ok_or_else(|| anyhow!("structured item is not resident"))?;
        open_exact(self.store_snapshot, &facts, id, body, holder)
    }
}

#[cfg(test)]
#[path = "item_tests.rs"]
mod tests;
