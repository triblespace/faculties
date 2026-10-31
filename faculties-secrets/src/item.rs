//! Structured Secrets are native private relations, not a persisted Rust catalog.
//!
//! Authoring inputs below are transient. Readers query `OpenedItem::facts()` at
//! the point of use and open only selected ciphertext children. Repeated roles,
//! unknown facts and opaque entity ids are ordinary open-world data.

use super::*;
use crate::encrypted::{Encrypted, ItemKey, ItemSealer};
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
    prepare_using(public_label, input, created_at, |key, bundle| {
        super::resource::seal_item_bundle(signer, public_label, key, bundle, created_at)
    })
}

pub(crate) fn prepare_using(
    public_label: &str,
    input: ItemInput<'_>,
    created_at: IntervalValue,
    finish: impl FnOnce(ItemKey, Bundle<Encrypted<SimpleArchive>>) -> Result<SealedVersion>,
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
    finish(key, bundle)
}

/// A password-manager item is one structured record whose private relation
/// carries `KIND_ITEM`; opening it is the general record path.
pub type OpenedItem<R> = crate::record::OpenedRecord<R>;

impl<R: BlobStoreGet> crate::record::OpenedRecord<R> {
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
    let items: Vec<_> = crate::record::open_all(reader, facts, id, holder)?
        .into_iter()
        .filter(|record| exists!(pattern!(&record.facts, [{ metadata::tag: KIND_ITEM }])))
        .collect();
    if items.is_empty() {
        bail!("no resident decryptable structured item for this holder");
    }
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
