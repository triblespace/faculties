//! Bounded 1PUX input translated directly to native encrypted item relations.
//! Vendor JSON is transient; no per-item or login JSON is persisted. The exact
//! original archive preserves all unsupported/unknown material. A second bound
//! raw resource encrypts a native SimpleArchive receipt, pinning exact item bodies.
//! Public labels are explicit item titles; CLI reports never contain them.
use super::{item, resource, schema::*, storage::SecretsCollection, SecretsSnapshot};
use crate::schemas::secrets_import::*;
use anyhow::{anyhow, bail, Result};
use ed25519_dalek::SigningKey;
use item::{AttachmentInput, FieldInput, FieldValue, ItemBody, ItemInput};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::capability::policy::resource_handle;
use triblespace::core::collection::{
    Collection, CollectionHandle, CollectionRecord, CollectionRecordSelector, CollectionStoreExt,
};
use triblespace::core::metadata;
use triblespace::core::repo::{BlobStoreGet, Store, StoreRead};
use triblespace::prelude::*;
use zeroize::{Zeroize, Zeroizing};

mod protected_import;
pub use protected_import::{PreparedProtectedImport, ProtectedImportPublicationError};
mod rejection;
pub use rejection::{ProtectedImportCode, ProtectedImportRejection, ProtectedImportStage};
use rejection::parse_error;

/// Limits apply before publication. Larger exports require an explicit caller
/// choice; ZIP64 directories and multi-disk archives are not supported.
#[derive(Clone, Copy, Debug)]
pub struct ImportLimits {
    pub archive_bytes: usize,
    pub member_bytes: u64,
    pub expanded_bytes: u64,
    pub json_bytes: u64,
    /// Preflight authored-payload budget: original bytes, repeated projected
    /// plaintext and conservative node/root/receipt overhead. Not a peak-RSS cap.
    pub projected_bytes: u64,
    pub entries: usize,
    pub items: usize,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            archive_bytes: 512 * 1024 * 1024,
            member_bytes: 512 * 1024 * 1024,
            expanded_bytes: 2 * 1024 * 1024 * 1024,
            json_bytes: 64 * 1024 * 1024,
            projected_bytes: 2 * 1024 * 1024 * 1024,
            entries: 65_534,
            items: 100_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportCounts {
    pub accounts: usize,
    pub vaults: usize,
    pub items: usize,
    pub fields: usize,
    pub attachments: usize,
    pub files: usize,
    pub archive_only_files: usize,
    /// Scalar leaves in export.data not natively mapped (including metadata).
    /// export.attributes, other members and non-scalar structure survive only
    /// in the original archive and are not included in this number.
    pub archive_only_values: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportReport {
    pub archive: Id,
    pub receipt: Id,
    pub item_versions: Vec<Id>,
    /// Exact native body alternatives pinned by this encrypted receipt.
    pub item_bodies: Vec<(Id, ItemBody)>,
    /// Original archive and exact native item resources, never all resources
    /// currently advertised under one item id. The retry receipt stays private.
    pub delivery_resources: Vec<CollectionHandle>,
    pub counts: ImportCounts,
    pub reused: bool,
    pub view_maintenance_pending: bool,
}
impl ImportReport {
    pub fn artifact_ids(&self) -> impl Iterator<Item = Id> + '_ {
        std::iter::once(self.archive).chain(self.item_versions.iter().copied())
    }
}

/// Only vendor input is retained here, never a catalog of resident pile state.
/// Parsed strings and retained bytes are scrubbed on drop on a best-effort basis.
pub struct OnePasswordArchive<'a> {
    original: &'a [u8],
    data: SensitiveJson,
    files: BTreeMap<String, Zeroizing<Vec<u8>>>,
    counts: ImportCounts,
}
struct Projection<'a> {
    fields: Vec<FieldInput<'a>>,
    attachments: Vec<AttachmentInput<'a>>,
    mapped: BTreeSet<String>,
    files: BTreeSet<String>,
}
struct SensitiveJson(Value);
impl Drop for SensitiveJson {
    fn drop(&mut self) {
        fn scrub(value: &mut Value) {
            match value {
                Value::String(text) => text.zeroize(),
                Value::Array(values) => values.iter_mut().for_each(scrub),
                Value::Object(values) => {
                    for (mut key, mut value) in std::mem::take(values) {
                        key.zeroize();
                        scrub(&mut value);
                    }
                }
                _ => {}
            }
        }
        scrub(&mut self.0);
    }
}

impl<'a> OnePasswordArchive<'a> {
    pub fn parse(original: &'a [u8], limits: ImportLimits) -> Result<Self> {
        if original.len() > limits.archive_bytes {
            return Err(parse_error(ProtectedImportCode::ArchiveByteLimit, "1Password import: archive byte limit exceeded"));
        }
        let declared_entries = standard_zip_entry_count(original)?;
        if declared_entries > limits.entries {
            return Err(parse_error(ProtectedImportCode::ArchiveEntryLimit, "1Password import: archive entry limit exceeded"));
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(original))
            .map_err(|_| parse_error(ProtectedImportCode::ZipArchive, "1Password import: invalid ZIP archive"))?;
        // zip 2 collapses duplicate central-directory names into an IndexMap.
        // Compare with the original EOCD count, not the already-collapsed names.
        if archive.len() != declared_entries || archive.offset() != 0 {
            return Err(parse_error(ProtectedImportCode::ZipLayout, "1Password import: duplicate entries or unsupported ZIP layout"));
        }
        let mut data = None;
        let mut attributes = None;
        let mut expanded = 0u64;
        let mut files = BTreeMap::new();
        let mut buffer = Zeroizing::new(vec![0u8; 64 * 1024]);
        for index in 0..archive.len() {
            let mut member = archive
                .by_index(index)
                .map_err(|_| parse_error(ProtectedImportCode::ZipMember, "1Password import: unreadable ZIP member"))?;
            let name = std::str::from_utf8(member.name_raw())
                .map_err(|_| parse_error(ProtectedImportCode::InvalidMemberPath, "1Password import: invalid member path"))?;
            if !safe_member_path(name) || member.is_symlink() || member.encrypted() {
                return Err(parse_error(ProtectedImportCode::UnsafeMember, "1Password import: unsafe or encrypted ZIP member"));
            }
            let is_data = name == "export.data";
            let is_attributes = name == "export.attributes";
            let retain = is_data || is_attributes || name.starts_with("files/");
            let directory = member.is_dir();
            let name = name.to_owned();
            let limit = if is_data || is_attributes {
                limits.member_bytes.min(limits.json_bytes)
            } else {
                limits.member_bytes
            };
            if member.size() > limit {
                return Err(parse_error(ProtectedImportCode::MemberByteLimit, "1Password import: member byte limit exceeded"));
            }
            let declared_size = member.size();
            let mut content = Zeroizing::new(Vec::new());
            let mut size = 0u64;
            loop {
                let read = member
                    .read(&mut buffer)
                    .map_err(|_| parse_error(ProtectedImportCode::ZipIntegrity, "1Password import: ZIP integrity check failed"))?;
                if read == 0 {
                    break;
                }
                size = size.checked_add(read as u64).ok_or_else(|| {
                    parse_error(ProtectedImportCode::MemberByteLimit, "1Password import: member byte limit exceeded")
                })?;
                expanded = expanded.checked_add(read as u64).ok_or_else(|| {
                    parse_error(ProtectedImportCode::ExpandedByteLimit, "1Password import: expanded byte limit exceeded")
                })?;
                if size > limit || expanded > limits.expanded_bytes {
                    return Err(parse_error(ProtectedImportCode::ExpandedByteLimit, "1Password import: expanded byte limit exceeded"));
                }
                if retain {
                    content.extend_from_slice(&buffer[..read]);
                }
            }
            if size != declared_size || (directory && size != 0) {
                return Err(parse_error(ProtectedImportCode::MemberSizeMismatch, "1Password import: ZIP member size mismatch"));
            }
            if is_data {
                data = Some(content);
            } else if is_attributes {
                attributes = Some(content);
            } else if name.starts_with("files/") && !directory {
                files.insert(name, content);
            }
        }
        let attributes = attributes
            .ok_or_else(|| parse_error(ProtectedImportCode::MissingAttributes, "1Password import: missing export attributes"))?;
        let attributes = parse_json(&attributes)?;
        if attributes.0.get("version").and_then(Value::as_u64) != Some(3) {
            return Err(parse_error(ProtectedImportCode::UnsupportedVersion, "1Password import: unsupported export format version"));
        }
        let data = parse_json(
            &data.ok_or_else(|| parse_error(ProtectedImportCode::MissingData, "1Password import: missing export data"))?,
        )?;
        let mut parsed = Self {
            original,
            data,
            files,
            counts: ImportCounts::default(),
        };
        let mut counts = parsed.counts;
        let accounts = required_array(&parsed.data.0, "accounts")?;
        counts.accounts = accounts.len();
        for account in accounts {
            let vaults = required_array(account, "vaults")?;
            counts.vaults += vaults.len();
            for vault in vaults {
                for item in required_array(vault, "items")? {
                    if !item.is_object() {
                        return Err(parse_error(ProtectedImportCode::InvalidStructure, "1Password import: invalid item structure"));
                    }
                    counts.items += 1;
                    if counts.items > limits.items {
                        return Err(parse_error(ProtectedImportCode::ItemCountLimit, "1Password import: item count limit exceeded"));
                    }
                }
            }
        }
        let mut mapped = BTreeSet::new();
        let mut used_files = BTreeSet::new();
        let mut projected_bytes = 0u64;
        charge_projected(&mut projected_bytes, original.len(), limits.projected_bytes)?;
        // Original archive and owner-only native receipt resource overhead.
        charge_projected(&mut projected_bytes, 32 * 1024, limits.projected_bytes)?;
        for (account_path, account, vault_path, vault, item_path, item) in parsed.items() {
            let projection =
                parsed.project(&account_path, account, &vault_path, vault, &item_path, item);
            // Charge every appearance, including duplicated account/vault
            // context and repeated references to one attachment. A small input
            // must not amplify into excessive authoring before the one commit.
            charge_projected(&mut projected_bytes, 8192, limits.projected_bytes)?;
            charge_projected(
                &mut projected_bytes,
                public_title(item).len(),
                limits.projected_bytes,
            )?;
            for field in &projection.fields {
                charge_projected(&mut projected_bytes, 512, limits.projected_bytes)?;
                charge_projected(
                    &mut projected_bytes,
                    field.label.map_or(0, str::len),
                    limits.projected_bytes,
                )?;
                let bytes = match field.value {
                    FieldValue::Text(value) => value.len(),
                    FieldValue::Integer(_) => 16,
                    FieldValue::Flag(_) => 1,
                };
                charge_projected(&mut projected_bytes, bytes, limits.projected_bytes)?;
            }
            for attachment in &projection.attachments {
                charge_projected(&mut projected_bytes, 512, limits.projected_bytes)?;
                charge_projected(
                    &mut projected_bytes,
                    attachment.filename.len(),
                    limits.projected_bytes,
                )?;
                charge_projected(
                    &mut projected_bytes,
                    attachment.bytes.len(),
                    limits.projected_bytes,
                )?;
            }
            counts.fields += projection.fields.len();
            counts.attachments += projection.attachments.len();
            mapped.extend(projection.mapped);
            used_files.extend(projection.files);
        }
        counts.files = parsed.files.len();
        counts.archive_only_files = parsed.files.len() - used_files.len();
        counts.archive_only_values = scalar_count(&parsed.data.0).saturating_sub(mapped.len());
        parsed.counts = counts;
        Ok(parsed)
    }

    pub fn counts(&self) -> ImportCounts {
        self.counts
    }

    /// Build every ciphertext and dependency before one source publication.
    /// Retry locators are only hints: decrypted native receipt relations pin
    /// exact bodies, and the complete original archive is compared byte-for-byte.
    pub fn import<S, R>(
        &self,
        store: &mut S,
        signer: &SigningKey,
        collection: SecretsCollection,
        observed: &SecretsSnapshot<R>,
        created_at: super::IntervalValue,
    ) -> Result<ImportReport>
    where
        S: Store + CollectionStoreExt,
        R: StoreRead,
    {
        if observed.collection() != collection.handle() {
            bail!("1Password import: mismatched collection observation");
        }
        if let Some(report) = self.verified_receipt(observed, signer)? {
            return Ok(report);
        }
        let fingerprint = self.fingerprint();
        let anchor = resource::seal_version(
            signer,
            "1Password original export",
            self.original,
            created_at,
        )?;
        let archive = anchor.secret;
        let archive_resource = prepared_resource(&anchor)?;
        let mut fragment = anchor.fragment;
        let mut entries = Fragment::empty();
        for (ordinal, (account_path, account, vault_path, vault, item_path, source)) in
            self.items().enumerate()
        {
            let projection = self.project(
                &account_path,
                account,
                &vault_path,
                vault,
                &item_path,
                source,
            );
            let title = public_title(source);
            let sealed = item::prepare(
                signer,
                title,
                ItemInput {
                    fields: &projection.fields,
                    attachments: &projection.attachments,
                },
                created_at,
            )?;
            // Query the newly authored fragment; opaque identifiers are never
            // reconstructed from content or validated against a derived id.
            let body = find!(body: ItemBody, pattern!(sealed.fragment.facts(), [
                { sealed.secret @ structured_body: ?body }
            ]))
            .next()
            .ok_or_else(|| anyhow!("1Password import: prepared item has no body"))?;
            let resource = prepared_resource(&sealed)?;
            entries += entity! { item_ordinal: ordinal as u64, import_item: sealed.secret,
            imported_body: body, resource_handle: resource };
            fragment += sealed.fragment;
        }
        let receipt_facts = entity! { metadata::tag: KIND_1PASSWORD_IMPORT,
        source_fingerprint: fingerprint, source_archive: archive,
        resource_handle: archive_resource, import_entry*: entries };
        let plaintext: Blob<SimpleArchive> = receipt_facts.into_facts().to_blob();
        let receipt = resource::seal_version(
            signer,
            "1Password import receipt",
            plaintext.bytes.as_ref(),
            created_at,
        )?;
        let receipt_id = receipt.secret;
        fragment += receipt.fragment;
        fragment += entity! { ExclusiveId::force_ref(&receipt_id) @
        metadata::tag: KIND_1PASSWORD_IMPORT, source_fingerprint: fingerprint };
        store.commit(collection.source(), signer, fragment)
            .map_err(|_| anyhow!("1Password import: publication failed; retry the same export to inspect receipt"))?;
        let after = super::storage::snapshot(
            store.snapshot().map_err(|_| {
                anyhow!("1Password import committed; verification pending; retry the same export")
            })?,
            collection,
        )
        .map_err(|_| {
            anyhow!("1Password import committed; verification pending; retry the same export")
        })?;
        let mut report = self.verified_receipt(&after, signer)?.ok_or_else(|| {
            anyhow!("1Password import committed; verification pending; retry the same export")
        })?;
        report.reused = false;
        Ok(report)
    }

    fn fingerprint(&self) -> Inline<inlineencodings::Hash<inlineencodings::Blake3>> {
        Inline::new(*blake3::hash(self.original).as_bytes())
    }

    fn items(&self) -> impl Iterator<Item = (String, &Value, String, &Value, String, &Value)> {
        self.data.0["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .flat_map(|(ai, account)| {
                account["vaults"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .flat_map(move |(vi, vault)| {
                        vault["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .enumerate()
                            .map(move |(ii, item)| {
                                (
                                    format!("/accounts/{ai}"),
                                    account,
                                    format!("/accounts/{ai}/vaults/{vi}"),
                                    vault,
                                    format!("/accounts/{ai}/vaults/{vi}/items/{ii}"),
                                    item,
                                )
                            })
                    })
            })
    }

    fn verified_receipt<R: StoreRead>(
        &self,
        observed: &SecretsSnapshot<R>,
        signer: &SigningKey,
    ) -> Result<Option<ImportReport>> {
        let Some(facts) = observed.facts() else {
            return Ok(None);
        };
        let reader = observed.store_snapshot();
        let source = Collection::<SimpleArchive>::open(reader, observed.collection())
            .map_err(|_| anyhow!("1Password import: source descriptor unavailable"))?;
        if !source
            .writer_is_admitted_acquiring(reader, signer.verifying_key())
            .map_err(|_| anyhow!("1Password import: source author admission unavailable"))?
        {
            return Ok(None);
        }
        let fingerprint = self.fingerprint();
        // Encryption to a public key proves possession, not authorship. Only a
        // current-importer-signed admitted source publication may associate the
        // locator with a receipt's exact raw S/H/R. Extra union facts cannot.
        for member in observed.support().members() {
            let selectors = BTreeSet::from([CollectionRecordSelector::CommitMember(
                observed.collection(),
                member.transmute(),
            )]);
            let signed = reader
                .select_records(&selectors)
                .map_err(|_| anyhow!("1Password import: source provenance unavailable"))?
                .into_iter()
                .any(|record| {
                    matches!(record, CollectionRecord::Commit(commit)
                    if commit.public_key().raw == signer.verifying_key().to_bytes()
                        && commit.verify_strict().is_ok())
                });
            if !signed {
                continue;
            }
            let Ok(publication) = reader.get::<TribleSet, _>(member) else {
                continue;
            };
            for (receipt, receipt_body, receipt_resource) in find!((
            receipt: Id, receipt_body: super::BytesHandle, receipt_resource: CollectionHandle),
            pattern!(&publication, [{
                ?receipt @ metadata::tag: KIND_1PASSWORD_IMPORT, metadata::tag: KIND_SECRET,
                source_fingerprint: fingerprint, secret_body: ?receipt_body,
                resource_handle: ?receipt_resource
            }])) {
                if !raw_descriptor_matches(observed, receipt, receipt_body, receipt_resource) {
                    continue;
                }
                let Ok(bytes) = resource::open_bound_raw(
                    reader,
                    facts,
                    receipt,
                    receipt_body,
                    receipt_resource,
                    signer,
                ) else {
                    continue;
                };
                let blob: Blob<SimpleArchive> = Blob::new(bytes.to_vec().into());
                let Ok(private) = blob.try_from_blob::<TribleSet>() else {
                    continue;
                };
                for (root, archive, archive_resource) in find!((root: Id, archive: Id, archive_resource: CollectionHandle), pattern!(&private, [
                    { ?root @ metadata::tag: KIND_1PASSWORD_IMPORT, source_fingerprint: fingerprint,
                        source_archive: ?archive, resource_handle: ?archive_resource }
                ])) {
                    // Compare the bytes opened by this exact receipt-pinned R,
                    // never a different body advertised under the same S.
                    let archive_ok =
                        raw_resource_bodies(observed, archive, archive_resource).any(|body| {
                            resource::open_bound_raw(
                                reader,
                                facts,
                                archive,
                                body,
                                archive_resource,
                                signer,
                            )
                            .is_ok_and(|bytes| bytes.as_slice() == self.original)
                        });
                    if !archive_ok {
                        continue;
                    }
                    let mut selected = BTreeSet::new();
                    let mut resources = BTreeSet::from([archive_resource]);
                    let mut complete = true;
                    for ordinal in 0..self.counts.items {
                        let mut found = false;
                        for (id, body, resource) in find!((id: Id, body: ItemBody, resource: CollectionHandle), pattern!(&private, [
                            { root @ import_entry: _?entry },
                            { _?entry @ item_ordinal: ordinal as u64, import_item: ?id,
                                imported_body: ?body, resource_handle: ?resource }
                        ])) {
                            if structured_resource_matches(observed, id, body, resource, signer)
                                && item::open_exact(reader.clone(), facts, id, body, signer).is_ok()
                            {
                                selected.insert((id, body));
                                resources.insert(resource);
                                found = true;
                            }
                        }
                        if !found {
                            complete = false;
                            break;
                        }
                    }
                    if complete {
                        let item_bodies = selected.into_iter().collect::<Vec<_>>();
                        let item_versions = item_bodies
                            .iter()
                            .map(|(id, _)| *id)
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        return Ok(Some(ImportReport {
                            archive,
                            receipt,
                            item_versions,
                            item_bodies,
                            delivery_resources: resources.into_iter().collect(),
                            counts: self.counts,
                            reused: true,
                            view_maintenance_pending: false,
                        }));
                    }
                }
            }
        }
        Ok(None)
    }

    fn project<'b>(
        &'b self,
        account_path: &str,
        account: &'b Value,
        vault_path: &str,
        vault: &'b Value,
        item_path: &str,
        source: &'b Value,
    ) -> Projection<'b> {
        let mut p = Projection {
            fields: Vec::new(),
            attachments: Vec::new(),
            mapped: BTreeSet::new(),
            files: BTreeSet::new(),
        };
        // Vendor uuid is scoped by account AND vault, never treated globally.
        for (parent, prefix, pointer, label) in [
            (account, account_path, "/attrs/uuid", "1Password account ID"),
            (
                account,
                account_path,
                "/attrs/email",
                "1Password account email",
            ),
            (vault, vault_path, "/attrs/uuid", "1Password vault ID"),
            (vault, vault_path, "/attrs/name", "1Password vault name"),
            (source, item_path, "/uuid", "1Password item ID"),
            (source, item_path, "/categoryUuid", "1Password category"),
            (source, item_path, "/trashed", "1Password trashed"),
            (source, item_path, "/state", "1Password state"),
            (
                source,
                item_path,
                "/createdAt",
                "1Password source creation time",
            ),
            (
                source,
                item_path,
                "/updatedAt",
                "1Password source update time",
            ),
            (source, item_path, "/favIndex", "1Password favorite index"),
        ] {
            if let Some(value) = parent.pointer(pointer) {
                push_scalar(
                    &mut p,
                    value,
                    ROLE_CUSTOM,
                    Some(label),
                    format!("{prefix}{pointer}"),
                );
            }
        }
        if let Some(title) = source.pointer("/overview/title").and_then(Value::as_str) {
            let path = format!("{item_path}/overview/title");
            if title == public_title(source) {
                p.mapped.insert(path);
            } else {
                // The core's public-label API disallows surrounding whitespace
                // and NUL. Preserve exact original title bytes privately rather
                // than pretending the display-label normalization was lossless.
                push_scalar(
                    &mut p,
                    &source["overview"]["title"],
                    ROLE_CUSTOM,
                    Some("1Password original title"),
                    path,
                );
            }
        }
        for (pointer, role) in [
            ("/overview/url", ROLE_URL),
            ("/details/notesPlain", ROLE_NOTES),
        ] {
            if let Some(value) = source.pointer(pointer) {
                push_scalar(&mut p, value, role, None, format!("{item_path}{pointer}"));
            }
        }
        for (index, url) in array_at(source, "/overview/urls").enumerate() {
            if let Some(value) = url.get("url") {
                let label = url.get("label").and_then(Value::as_str);
                if push_scalar(
                    &mut p,
                    value,
                    ROLE_URL,
                    label,
                    format!("{item_path}/overview/urls/{index}/url"),
                ) && label.is_some()
                {
                    p.mapped
                        .insert(format!("{item_path}/overview/urls/{index}/label"));
                }
            }
        }
        for (index, field) in array_at(source, "/details/loginFields").enumerate() {
            let role = match field.get("designation").and_then(Value::as_str) {
                Some("username") => ROLE_USERNAME,
                Some("password") => ROLE_PASSWORD,
                _ => ROLE_CUSTOM,
            };
            let (label_key, label) = field_label(field);
            if let Some(value) = field.get("value") {
                if push_scalar(
                    &mut p,
                    value,
                    role,
                    label,
                    format!("{item_path}/details/loginFields/{index}/value"),
                ) {
                    if label.is_some() {
                        p.mapped.insert(format!(
                            "{item_path}/details/loginFields/{index}/{label_key}"
                        ));
                    }
                    if role != ROLE_CUSTOM {
                        p.mapped.insert(format!(
                            "{item_path}/details/loginFields/{index}/designation"
                        ));
                    }
                }
            }
        }
        for (si, section) in array_at(source, "/details/sections").enumerate() {
            for (fi, field) in array_at(section, "/fields").enumerate() {
                let (label_key, label) = field_label(field);
                let path = format!("{item_path}/details/sections/{si}/fields/{fi}");
                if let Some(value) = field.get("value") {
                    let before = p.fields.len();
                    if value.is_object() {
                        // Named scalar variants are native values. Complex,
                        // future or unrecognized variants remain in the archive.
                        for variant in [
                            "string",
                            "concealed",
                            "email",
                            "url",
                            "phone",
                            "totp",
                            "int",
                            "number",
                            "date",
                            "monthYear",
                            "reference",
                            "sshKey",
                            "address",
                            "creditCardType",
                            "gender",
                        ] {
                            if let Some(value) = value.get(variant) {
                                let role = if variant == "url" {
                                    ROLE_URL
                                } else {
                                    ROLE_CUSTOM
                                };
                                push_scalar(
                                    &mut p,
                                    value,
                                    role,
                                    label,
                                    format!("{path}/value/{variant}"),
                                );
                            }
                        }
                        if let Some(attributes) = value.get("file") {
                            self.attach(&mut p, attributes, &format!("{path}/value/file"));
                        }
                    } else {
                        push_scalar(&mut p, value, ROLE_CUSTOM, label, format!("{path}/value"));
                    }
                    if p.fields.len() > before && label.is_some() {
                        p.mapped.insert(format!("{path}/{label_key}"));
                    }
                }
            }
        }
        if let Some(attributes) = source.pointer("/details/documentAttributes") {
            self.attach(
                &mut p,
                attributes,
                &format!("{item_path}/details/documentAttributes"),
            );
        }
        p
    }

    fn attach<'b>(&'b self, p: &mut Projection<'b>, attributes: &'b Value, path: &str) {
        let Some(id) = attributes.get("documentId").and_then(Value::as_str) else {
            return;
        };
        let Some(filename) = attributes.get("fileName").and_then(Value::as_str) else {
            return;
        };
        // Exact vendor reference, not basename heuristics or prefix matching.
        let member = format!("files/{id}___{filename}");
        if let Some(bytes) = self.files.get(&member) {
            p.attachments.push(AttachmentInput { filename, bytes });
            p.files.insert(member);
            p.mapped.insert(format!("{path}/documentId"));
            p.mapped.insert(format!("{path}/fileName"));
        }
    }
}

fn public_title(source: &Value) -> &str {
    source
        .pointer("/overview/title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty() && !title.contains('\0'))
        .unwrap_or("Untitled 1Password item")
}
fn prepared_resource(sealed: &super::SealedVersion) -> Result<CollectionHandle> {
    find!(resource: CollectionHandle, pattern!(sealed.fragment.facts(), [
        { sealed.secret @ resource_handle: ?resource }
    ]))
    .next()
    .ok_or_else(|| anyhow!("1Password import: prepared resource is missing"))
}
fn raw_resource_bodies<R: BlobStoreGet>(
    observed: &SecretsSnapshot<R>,
    id: Id,
    resource: CollectionHandle,
) -> std::vec::IntoIter<super::BytesHandle> {
    let Ok(descriptor) = observed.store_snapshot().get::<TribleSet, _>(resource) else {
        return Vec::new().into_iter();
    };
    let Some(facts) = observed.facts() else {
        return Vec::new().into_iter();
    };
    find!(body: super::BytesHandle, pattern!(&descriptor, [
        { metadata::tag: KIND_SECRET_RESOURCE, wrap_secret: id,
            secret_body: ?body }
    ]))
    .filter(|body| {
        exists!(pattern!(facts, [{ id @ metadata::tag: KIND_SECRET, secret_body: *body }]))
    })
    .collect::<Vec<_>>()
    .into_iter()
}
fn raw_descriptor_matches<R: BlobStoreGet>(
    observed: &SecretsSnapshot<R>,
    id: Id,
    body: super::BytesHandle,
    resource: CollectionHandle,
) -> bool {
    raw_resource_bodies(observed, id, resource).any(|candidate| candidate == body)
}

#[cfg(test)]
fn raw_resource_matches<R: BlobStoreGet>(
    observed: &SecretsSnapshot<R>,
    id: Id,
    resource: CollectionHandle,
    signer: &SigningKey,
) -> bool {
    let Some(facts) = observed.facts() else {
        return false;
    };
    raw_resource_bodies(observed, id, resource).any(|body| {
        resource::open_bound_raw(observed.store_snapshot(), facts, id, body, resource, signer)
            .is_ok()
    })
}
fn structured_resource_matches<R: BlobStoreGet>(
    observed: &SecretsSnapshot<R>,
    id: Id,
    body: ItemBody,
    resource: CollectionHandle,
    signer: &SigningKey,
) -> bool {
    let Ok(descriptor) = observed.store_snapshot().get::<TribleSet, _>(resource) else {
        return false;
    };
    let Some(facts) = observed.facts() else {
        return false;
    };
    exists!(
        pattern!(&descriptor, [{ metadata::tag: KIND_SECRET_RESOURCE,
        wrap_secret: id, structured_body: body }])
    ) && resource::holder_has_binding(
        observed.store_snapshot(),
        facts,
        id,
        body.transmute(),
        resource,
        signer,
    )
    .unwrap_or(false)
}
fn array_at<'a>(value: &'a Value, path: &str) -> impl Iterator<Item = &'a Value> {
    value
        .pointer(path)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}
fn field_label(field: &Value) -> (&str, Option<&str>) {
    for key in ["title", "name", "id"] {
        if let Some(label) = field.get(key).and_then(Value::as_str) {
            return (key, Some(label));
        }
    }
    ("", None)
}
fn push_scalar<'a>(
    p: &mut Projection<'a>,
    value: &'a Value,
    role: Id,
    label: Option<&'a str>,
    path: String,
) -> bool {
    let value = if let Some(value) = value.as_str() {
        FieldValue::Text(value)
    } else if let Some(value) = value.as_i64() {
        FieldValue::Integer(value as i128)
    } else if let Some(value) = value.as_u64() {
        FieldValue::Integer(value as i128)
    } else if let Some(value) = value.as_bool() {
        FieldValue::Flag(value)
    } else {
        return false;
    };
    p.fields.push(FieldInput { role, label, value });
    p.mapped.insert(path);
    true
}
fn scalar_count(value: &Value) -> usize {
    match value {
        Value::Object(values) => values.values().map(scalar_count).sum(),
        Value::Array(values) => values.iter().map(scalar_count).sum(),
        _ => 1,
    }
}

fn parse_json(bytes: &[u8]) -> Result<SensitiveJson> {
    serde_json::from_slice(bytes)
        .map(SensitiveJson)
        .map_err(|_| parse_error(ProtectedImportCode::MalformedJson, "1Password import: malformed export JSON"))
}

fn charge_projected(total: &mut u64, amount: usize, limit: u64) -> Result<()> {
    let error = || parse_error(ProtectedImportCode::ProjectedByteLimit, "1Password import: projected byte budget exceeded");
    let amount = u64::try_from(amount).map_err(|_| error())?;
    *total = total
        .checked_add(amount)
        .filter(|sum| *sum <= limit)
        .ok_or_else(error)?;
    Ok(())
}

fn required_array<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| parse_error(ProtectedImportCode::InvalidStructure, "1Password import: invalid account/vault/item structure"))
}

fn safe_member_path(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') || name.contains(['\\', ':', '\0']) {
        return false;
    }
    let path = name.strip_suffix('/').unwrap_or(name);
    !path.is_empty() && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

// A narrow EOCD check, not a second ZIP parser. zip 2 owns entry parsing,
// decompression, sizes and CRC validation. ZIP64/multidisk are refused explicitly.
fn standard_zip_entry_count(bytes: &[u8]) -> Result<usize> {
    let invalid = || parse_error(ProtectedImportCode::ZipDirectory, "1Password import: unsupported or malformed ZIP directory");
    let lower = bytes.len().saturating_sub(22 + u16::MAX as usize);
    let end = bytes.len().checked_sub(22).ok_or_else(invalid)?;
    let position = (lower..=end)
        .rev()
        .find(|&position| {
            bytes[position..].starts_with(b"PK\x05\x06")
                && position
                    + 22
                    + u16::from_le_bytes([bytes[position + 20], bytes[position + 21]]) as usize
                    == bytes.len()
        })
        .ok_or_else(invalid)?;
    let read16 =
        |offset| u16::from_le_bytes([bytes[position + offset], bytes[position + offset + 1]]);
    let read32 = |offset| {
        u32::from_le_bytes(
            bytes[position + offset..position + offset + 4]
                .try_into()
                .unwrap(),
        )
    };
    let count = read16(10);
    let size = read32(12);
    let offset = read32(16);
    if read16(4) != 0
        || read16(6) != 0
        || read16(8) != count
        || count == u16::MAX
        || size == u32::MAX
        || offset == u32::MAX
        || (offset as u64 + size as u64) != position as u64
        || position
            .checked_sub(20)
            .is_some_and(|start| bytes[start..].starts_with(b"PK\x06\x07"))
    {
        return Err(invalid());
    }
    Ok(count as usize)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::resource::{DeliveryLimits, SecretTarget};
    use super::*;
    use std::io::Write;
    use triblespace::core::capability::CapabilityResource;
    use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
    use triblespace::core::repo::memoryrepo::MemoryRepo;
    use triblespace::core::repo::{BlobStoreList, CapabilityProofRead, SnapshotSource};
    pub(crate) fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            zip.start_file(
                *name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    pub(crate) fn fixture() -> Vec<u8> {
        zip(&[
            ("export.attributes", br#"{"version":3,"futureAttribute":true}"#),
            ("export.data", br#"{"futureRoot":"SYNTHETIC-PRIVATE-root","accounts":[{
                "attrs":{"uuid":"SYNTHETIC-PRIVATE-account","email":"synthetic@example.invalid"},
                "vaults":[{"attrs":{"uuid":"SYNTHETIC-PRIVATE-vault-a","name":"SYNTHETIC-PRIVATE-vault"},
                    "items":[{"uuid":"same-item-id","categoryUuid":"001","trashed":false,
                        "overview":{"title":"Public synthetic title","url":"https://synthetic.invalid",
                            "urls":[{"label":"SYNTHETIC-PRIVATE-url-label","url":"https://other.invalid"}]},
                        "details":{"loginFields":[
                            {"designation":"username","value":"SYNTHETIC-PRIVATE-user"},
                            {"designation":"password","value":"SYNTHETIC-PRIVATE-password"},
                            {"designation":"futureRole","name":"SYNTHETIC-PRIVATE-custom-label","value":"SYNTHETIC-PRIVATE-custom"}],
                            "notesPlain":"SYNTHETIC-PRIVATE-notes",
                            "passwordHistory":[{"value":"SYNTHETIC-PRIVATE-old","time":123}],
                            "sections":[{"fields":[
                                {"title":"SYNTHETIC-PRIVATE-otp-label","value":{"totp":"SYNTHETIC-PRIVATE-otp"}},
                                {"title":"SYNTHETIC-PRIVATE-number-label","value":{"int":42}},
                                {"title":"SYNTHETIC-PRIVATE-flag-label","value":true},
                                {"title":"SYNTHETIC-PRIVATE-unknown","value":{"futureVariant":{"deep":"SYNTHETIC-PRIVATE-unmapped"}}}]}],
                            "documentAttributes":{"documentId":"doc","fileName":"SYNTHETIC-PRIVATE-file.bin"}},
                        "futureItem":{"huge":1234567890123456789012345678901234567890}
                    }]},
                    {"attrs":{"uuid":"SYNTHETIC-PRIVATE-vault-b","name":"Second vault"},
                    "items":[{"uuid":"same-item-id","overview":{"title":"Second public title"},
                        "details":{"notesPlain":"SYNTHETIC-PRIVATE-second"}}]}
                ]} ]}"#),
            ("files/doc___SYNTHETIC-PRIVATE-file.bin", b"SYNTHETIC-PRIVATE-binary\0\xff"),
            ("files/SYNTHETIC-PRIVATE-icon.png", b"SYNTHETIC-PRIVATE-icon"),
            ("future-member", b"SYNTHETIC-PRIVATE-future"),
        ])
    }
    fn at() -> super::super::IntervalValue {
        let epoch = hifitime::Epoch::from_unix_seconds(1.0);
        (epoch, epoch).try_to_inline().unwrap()
    }
    fn collection(store: &mut MemoryRepo, signer: &SigningKey) -> SecretsCollection {
        SecretsCollection::register(
            store,
            "native importer synthetic fixture",
            CollectionPolicy::new(
                AdmissionPolicy::Open,
                AdmissionPolicy::direct(signer.verifying_key()),
            ),
        )
        .unwrap()
    }
    fn text_values<R: BlobStoreGet>(opened: &item::OpenedItem<R>, role: Id) -> BTreeSet<Vec<u8>> {
        find!(text: item::PrivateText, pattern!(opened.facts(), [
            { metadata::tag: KIND_ITEM, item_field: _?field },
            { _?field @ field_role: role, field_text: ?text }
        ]))
        .map(|text| opened.open_child(text).unwrap().bytes.as_ref().to_vec())
        .collect()
    }
    fn assert_no_private<R: BlobStoreGet + BlobStoreList>(reader: &R) {
        for info in reader.blobs() {
            let bytes: Blob<blobencodings::UnknownBlob> = reader.get(info.unwrap().handle).unwrap();
            assert!(
                !bytes
                    .bytes
                    .windows(b"SYNTHETIC-PRIVATE-".len())
                    .any(|part| part == b"SYNTHETIC-PRIVATE-"),
                "plaintext leaked into published blobs"
            );
        }
    }

    #[test]
    fn native_fields_context_attachments_and_exact_source_survive_one_commit_and_retry() {
        let bytes = Zeroizing::new(fixture());
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let c = archive.counts();
        assert_eq!(
            (
                c.accounts,
                c.vaults,
                c.items,
                c.attachments,
                c.files,
                c.archive_only_files
            ),
            (1, 2, 2, 1, 2, 1)
        );
        assert!(c.archive_only_values >= 6);
        let signer = SigningKey::from_bytes(&[71; 32]);
        let other = SigningKey::from_bytes(&[72; 32]);
        let mut store = MemoryRepo::for_host(signer.verifying_key());
        let collection = collection(&mut store, &signer);
        let before =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let report = archive
            .import(&mut store, &signer, collection, &before, at())
            .unwrap();
        assert!(!report.reused);
        assert!(before.facts().is_none());
        let after = super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert_eq!(after.support().len(), 1);
        assert_eq!(after.open(report.archive, &signer).unwrap(), *bytes);
        assert_eq!(
            super::super::secret_rows(after.facts().unwrap()).len(),
            2,
            "only original archive and native receipt are raw resources"
        );
        assert_eq!(item::rows(after.facts().unwrap()).len(), 2);
        assert_eq!(report.item_bodies.len(), 2);
        let opened = report
            .item_bodies
            .iter()
            .map(|(id, body)| {
                item::open_exact(
                    after.store_snapshot().clone(),
                    after.facts().unwrap(),
                    *id,
                    *body,
                    &signer,
                )
                .unwrap()
            })
            .find(|opened| text_values(opened, ROLE_PASSWORD).len() == 1)
            .unwrap();
        assert_eq!(
            text_values(&opened, ROLE_USERNAME),
            BTreeSet::from([b"SYNTHETIC-PRIVATE-user".to_vec()])
        );
        assert!(text_values(&opened, ROLE_NOTES).contains(b"SYNTHETIC-PRIVATE-notes".as_slice()));
        let custom = text_values(&opened, ROLE_CUSTOM);
        for value in [
            b"SYNTHETIC-PRIVATE-vault-a".as_slice(),
            b"SYNTHETIC-PRIVATE-account",
            b"SYNTHETIC-PRIVATE-custom",
            b"SYNTHETIC-PRIVATE-otp",
        ] {
            assert!(custom.contains(value));
        }
        assert!(exists!(
            pattern!(opened.facts(), [{ field_integer: 42i128 }])
        ));
        assert!(exists!(pattern!(opened.facts(), [{ field_flag: true }])));
        let attachment = find!(bytes: Inline<inlineencodings::Handle<super::super::encrypted::Encrypted<blobencodings::RawBytes>>>,
            pattern!(opened.facts(), [{ item_attachment: _?a }, { _?a @ attachment_bytes: ?bytes }])).next().unwrap();
        assert_eq!(
            opened.open_child(attachment).unwrap().bytes.as_ref(),
            b"SYNTHETIC-PRIVATE-binary\0\xff"
        );
        let receipt: Blob<SimpleArchive> =
            Blob::new(after.open(report.receipt, &signer).unwrap().into());
        let receipt: TribleSet = receipt.try_from_blob().unwrap();
        assert_eq!(
            find!(id: Id, pattern!(&receipt, [{ import_entry: ?id }])).count(),
            2
        );
        for id in report.artifact_ids().chain(std::iter::once(report.receipt)) {
            assert_eq!(
                super::super::wrap_recipients(after.facts().unwrap(), id),
                BTreeSet::from([signer.verifying_key().to_bytes()])
            );
        }
        assert!(after.open(report.archive, &other).is_err());
        assert!(
            after.store_snapshot().proofs().unwrap().next().is_none(),
            "import never grants"
        );
        assert_no_private(after.store_snapshot());
        let retry = archive
            .import(&mut store, &signer, collection, &after, at())
            .unwrap();
        assert!(retry.reused);
        assert_eq!(retry.item_bodies, report.item_bodies);
        assert_eq!(retry.delivery_resources, report.delivery_resources);
        assert_eq!(
            super::super::storage::snapshot(store.snapshot().unwrap(), collection)
                .unwrap()
                .support(),
            after.support()
        );
    }

    #[test]
    fn repeated_designations_and_normalized_titles_keep_exact_native_values() {
        let bytes = zip(&[("export.attributes", br#"{"version":3}"#), ("export.data", br#"{"accounts":[{"vaults":[{"items":[{
            "overview":{"title":"  padded synthetic title  "},"details":{"loginFields":[
                {"designation":"password","value":"synthetic-first"},
                {"designation":"password","value":"synthetic-second"},
                {"designation":"arbitrary-future-role","name":"custom","value":"synthetic-custom"}]}}]}]}]}"#)]);
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let (ap, account, vp, vault, ip, source) = archive.items().next().unwrap();
        let p = archive.project(&ap, account, &vp, vault, &ip, source);
        assert_eq!(
            p.fields
                .iter()
                .filter(|field| field.role == ROLE_PASSWORD)
                .count(),
            2
        );
        assert!(p
            .fields
            .iter()
            .any(|field| matches!(field.value, FieldValue::Text("  padded synthetic title  "))));
        assert!(p.fields.iter().any(|field| field.role == ROLE_CUSTOM
            && matches!(field.value, FieldValue::Text("synthetic-custom"))));
        assert!(public_title(source) == "padded synthetic title");
    }

    #[test]
    fn projected_budget_counts_repeated_context_and_attachment_references_before_sealing() {
        for context in [true, false] {
            let mut source_item =
                serde_json::json!({"overview":{"title":"Synthetic public label"}});
            if !context {
                source_item["details"] = serde_json::json!({"documentAttributes":{
                    "documentId":"document", "fileName":"shared.bin"}});
            }
            let attrs = if context {
                serde_json::json!({"email":"x".repeat(4096)})
            } else {
                serde_json::json!({})
            };
            let data = serde_json::to_vec(&serde_json::json!({"accounts":[{
                "attrs":attrs, "vaults":[{"items":vec![source_item;8]}]}]}))
            .unwrap();
            let file = vec![0x55; 4096];
            let mut entries = vec![
                ("export.attributes", br#"{"version":3}"#.as_slice()),
                ("export.data", data.as_slice()),
            ];
            if !context {
                entries.push(("files/document___shared.bin", file.as_slice()));
            }
            let bytes = zip(&entries);
            assert!(bytes.len() < 128 * 1024);
            assert_eq!(
                OnePasswordArchive::parse(&bytes, ImportLimits::default())
                    .unwrap()
                    .counts
                    .items,
                8
            );
            let limits = ImportLimits {
                projected_bytes: 128 * 1024,
                ..ImportLimits::default()
            };
            let error = OnePasswordArchive::parse(&bytes, limits)
                .err()
                .expect("repeated output must exhaust budget");
            assert!(error.to_string() == "1Password import: projected byte budget exceeded");
        }
        let mut total = u64::MAX - 1;
        assert!(charge_projected(&mut total, 2, u64::MAX).is_err());
        assert_eq!(total, u64::MAX - 1);
    }

    #[test]
    fn changed_source_and_misleading_public_receipts_cannot_reuse_or_expand_delivery() {
        let bytes = fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let signer = SigningKey::from_bytes(&[73; 32]);
        let mut store = MemoryRepo::for_host(signer.verifying_key());
        let collection = collection(&mut store, &signer);
        let before =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        // A well-shaped candidate locator with a wrong encrypted receipt body
        // is not proof of a prior import.
        let wrong = resource::seal_version(
            &signer,
            "wrong receipt",
            b"not a native archive",
            at(),
        )
        .unwrap();
        let mut fragment = wrong.fragment;
        fragment += entity! { ExclusiveId::force_ref(&wrong.secret) @
        metadata::tag: KIND_1PASSWORD_IMPORT, source_fingerprint: archive.fingerprint() };
        store
            .commit(collection.source(), &signer, fragment)
            .unwrap();
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let first = archive
            .import(&mut store, &signer, collection, &observed, at())
            .unwrap();
        assert!(!first.reused);
        let unrelated = item::prepare(
            &signer,
            "unrelated public item",
            ItemInput::default(),
            at(),
        )
        .unwrap();
        let unrelated_id = unrelated.secret;
        let mut additions = unrelated.fragment;
        additions += entity! { ExclusiveId::force_ref(&first.receipt) @
        import_item: unrelated_id, source_archive: unrelated_id,
        resource_handle: first.delivery_resources[0],
        metadata::description: "unknown annotation" };
        // A foreign native body appended under an imported opaque id remains
        // another typed relation, but cannot change the encrypted receipt.
        let foreign_body = find!(body: ItemBody, pattern!(additions.facts(), [{ unrelated_id @ structured_body: ?body }])).next().unwrap();
        additions += entity! { ExclusiveId::force_ref(&first.item_versions[0]) @ structured_body: foreign_body };
        store
            .commit(collection.source(), &signer, additions)
            .unwrap();
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let retry = archive
            .import(&mut store, &signer, collection, &observed, at())
            .unwrap();
        assert!(retry.reused);
        assert_eq!(retry.item_bodies, first.item_bodies);
        assert_eq!(retry.delivery_resources, first.delivery_resources);
        assert!(!retry.artifact_ids().any(|id| id == unrelated_id));
        let changed = zip(&[
            ("export.attributes", br#"{"version":3}"#),
            ("export.data", br#"{"accounts":[]}"#),
        ]);
        let changed = OnePasswordArchive::parse(&changed, ImportLimits::default()).unwrap();
        let second = changed
            .import(&mut store, &signer, collection, &observed, at())
            .unwrap();
        assert!(!second.reused);
        assert_ne!(second.archive, first.archive);
        assert!(before.facts().is_none());
    }

    #[test]
    fn receipt_locator_and_binding_must_share_one_author_signed_publication() {
        let bytes = zip(&[
            ("export.attributes", br#"{"version":3}"#),
            ("export.data", br#"{"accounts":[]}"#),
        ]);
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let signer = SigningKey::from_bytes(&[85; 32]);
        let mut store = MemoryRepo::for_host(signer.verifying_key());
        let collection = collection(&mut store, &signer);
        let anchor = resource::seal_version(&signer, "original", &bytes, at()).unwrap();
        let archive_id = anchor.secret;
        let archive_resource = prepared_resource(&anchor).unwrap();
        let private = entity! {
            metadata::tag: KIND_1PASSWORD_IMPORT,
            source_fingerprint: archive.fingerprint(),
            source_archive: archive_id,
            resource_handle: archive_resource,
        };
        let private: Blob<SimpleArchive> = private.into_facts().to_blob();
        let sealed =
            resource::seal_version(&signer, "receipt", private.bytes.as_ref(), at()).unwrap();
        let receipt = sealed.secret;
        let mut binding = anchor.fragment;
        binding += sealed.fragment;
        let locator = entity! { ExclusiveId::force_ref(&receipt) @
            metadata::tag: KIND_1PASSWORD_IMPORT,
            source_fingerprint: archive.fingerprint(),
        };
        // Both halves are genuinely signed by the importing author and the
        // complete union is decryptable. Neither publication binds the locator
        // to S/H/R, so their union is not an authenticated import receipt.
        store
            .commit(collection.source(), &signer, binding.clone())
            .unwrap();
        store
            .commit(collection.source(), &signer, locator.clone())
            .unwrap();
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert!(observed.open(receipt, &signer).is_ok());
        assert!(archive.verified_receipt(&observed, &signer).unwrap().is_none());

        binding += locator;
        store
            .commit(collection.source(), &signer, binding.clone())
            .unwrap();
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert_eq!(
            archive.verified_receipt(&observed, &signer).unwrap().unwrap().receipt,
            receipt
        );

        // A separate routed source does not inherit receipt declarations from
        // the store's other collections. Once explicitly carried there in one
        // author-signed publication, the same immutable R remains valid there.
        let elsewhere = SecretsCollection::register(
            &mut store,
            "explicitly carried synthetic receipt",
            CollectionPolicy::new(
                AdmissionPolicy::Open,
                AdmissionPolicy::direct(signer.verifying_key()),
            ),
        )
        .unwrap();
        let empty =
            super::super::storage::snapshot(store.snapshot().unwrap(), elsewhere).unwrap();
        assert!(archive.verified_receipt(&empty, &signer).unwrap().is_none());
        store.commit(elsewhere.source(), &signer, binding).unwrap();
        let carried =
            super::super::storage::snapshot(store.snapshot().unwrap(), elsewhere).unwrap();
        assert_eq!(
            archive.verified_receipt(&carried, &signer).unwrap().unwrap().receipt,
            receipt
        );
    }

    #[test]
    fn attacker_authored_decryptable_receipt_cannot_select_unrelated_owner_items() {
        use triblespace::core::collection::grant_collection_write;
        let bytes = fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let owner = SigningKey::from_bytes(&[83; 32]);
        let attacker = SigningKey::from_bytes(&[84; 32]);
        let mut store = MemoryRepo::for_host(owner.verifying_key());
        let collection = collection(&mut store, &owner);
        grant_collection_write(
            &mut store,
            collection.handle(),
            &owner,
            attacker.verifying_key(),
        )
        .unwrap();
        let anchor =
            resource::seal_version(&owner, "original", &bytes, at()).unwrap();
        let archive_id = anchor.secret;
        let archive_resource = prepared_resource(&anchor).unwrap();
        let unrelated = item::prepare(
            &owner,
            "unrelated owner item",
            ItemInput::default(),
            at(),
        )
        .unwrap();
        let id = unrelated.secret;
        let target = prepared_resource(&unrelated).unwrap();
        let body = find!(body: ItemBody, pattern!(unrelated.fragment.facts(), [{ id @ structured_body: ?body }])).next().unwrap();
        let mut fragment = anchor.fragment;
        fragment += unrelated.fragment;
        store.commit(collection.source(), &owner, fragment).unwrap();
        let mut entries = Fragment::empty();
        for ordinal in 0..archive.counts.items {
            entries += entity! { item_ordinal: ordinal as u64, import_item: id,
            imported_body: body, resource_handle: target };
        }
        let forged = entity! { metadata::tag: KIND_1PASSWORD_IMPORT,
        source_fingerprint: archive.fingerprint(), source_archive: archive_id,
        resource_handle: archive_resource, import_entry*: entries };
        let forged: Blob<SimpleArchive> = forged.into_facts().to_blob();
        let sealed = resource::seal_version(
            &attacker,
            "forged receipt",
            forged.bytes.as_ref(),
            at(),
        )
        .unwrap();
        let forged_id = sealed.secret;
        let mut fragment = sealed.fragment;
        fragment += entity! { ExclusiveId::force_ref(&forged_id) @ metadata::tag: KIND_1PASSWORD_IMPORT,
        source_fingerprint: archive.fingerprint() };
        store
            .commit(collection.source(), &attacker, fragment)
            .unwrap();
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let wraps = super::super::add_recipient_envelopes_from_facts(
            observed.store_snapshot(),
            observed.facts().unwrap(),
            forged_id,
            &attacker,
            [owner.verifying_key()],
        )
        .unwrap();
        store
            .commit(collection.source(), &attacker, wraps.fragment)
            .unwrap();
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert!(
            observed.open(forged_id, &owner).is_ok(),
            "attacker can seal to the owner's public key"
        );
        assert!(archive
            .verified_receipt(&observed, &owner)
            .unwrap()
            .is_none());
        let report = archive
            .import(&mut store, &owner, collection, &observed, at())
            .unwrap();
        assert!(!report.reused);
        assert!(!report.item_versions.contains(&id));
        assert!(!report.delivery_resources.contains(&target));
    }

    #[test]
    fn opaque_native_receipt_ids_repeated_roles_and_unmodeled_facts_remain_visible() {
        let bytes = fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let signer = SigningKey::from_bytes(&[74; 32]);
        let mut store = MemoryRepo::for_host(signer.verifying_key());
        let collection = collection(&mut store, &signer);
        let before =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let first = archive
            .import(&mut store, &signer, collection, &before, at())
            .unwrap();
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let original: Blob<SimpleArchive> =
            Blob::new(observed.open(first.receipt, &signer).unwrap().into());
        let original: TribleSet = original.try_from_blob().unwrap();
        let root = genid();
        let mut private = entity! { &root @ metadata::tag: KIND_1PASSWORD_IMPORT,
        source_fingerprint: archive.fingerprint(), source_archive: first.archive,
        resource_handle: *first.delivery_resources.iter().find(|r| raw_resource_matches(&observed, first.archive, **r, &signer)).unwrap(),
        metadata::description: "unknown native receipt annotation" };
        for (ordinal, id, body, resource) in find!((ordinal: u64, id: Id, body: ItemBody, resource: CollectionHandle),
            pattern!(&original, [{ item_ordinal: ?ordinal, import_item: ?id, imported_body: ?body, resource_handle: ?resource }]))
        {
            let entry = genid();
            private += entity! { &root @ import_entry: &entry };
            private += entity! { &entry @ item_ordinal: ordinal, import_item: id, imported_body: body,
            resource_handle: resource, metadata::tag: genid().id };
        }
        let blob: Blob<SimpleArchive> = private.into_facts().to_blob();
        let sealed = resource::seal_version(
            &signer,
            "opaque receipt",
            blob.bytes.as_ref(),
            at(),
        )
        .unwrap();
        let opaque = sealed.secret;
        let mut fragment = sealed.fragment;
        fragment += entity! { ExclusiveId::force_ref(&opaque) @ metadata::tag: KIND_1PASSWORD_IMPORT,
        source_fingerprint: archive.fingerprint() };
        store
            .commit(collection.source(), &signer, fragment)
            .unwrap();
        // Directly inspect the opaque candidate without depending on query
        // iteration ordering among concurrently valid receipt alternatives.
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let content: Blob<SimpleArchive> =
            Blob::new(observed.open(opaque, &signer).unwrap().into());
        let native: TribleSet = content.try_from_blob().unwrap();
        assert!(exists!(
            pattern!(&native, [{ root.id @ metadata::tag: KIND_1PASSWORD_IMPORT }])
        ));
        assert!(archive
            .verified_receipt(&observed, &signer)
            .unwrap()
            .is_some());
    }

    #[test]
    fn explicit_exact_resource_delivery_is_non_delegating_and_receipt_remains_owner_only() {
        let bytes = fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let signer = SigningKey::from_bytes(&[75; 32]);
        let bob = SigningKey::from_bytes(&[76; 32]);
        let carol = SigningKey::from_bytes(&[77; 32]);
        let mut store = MemoryRepo::for_host(signer.verifying_key());
        let collection = collection(&mut store, &signer);
        let before =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let report = archive
            .import(&mut store, &signer, collection, &before, at())
            .unwrap();
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        for target in &report.delivery_resources {
            resource::grant(
                &mut store,
                &signer,
                &observed,
                SecretTarget::Resource(*target),
                bob.verifying_key(),
                DeliveryLimits::default(),
                false,
            )
            .unwrap();
        }
        let observed =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let selected = report
            .delivery_resources
            .iter()
            .copied()
            .map(SecretTarget::Resource)
            .collect::<Vec<_>>();
        let count = super::super::storage::maintain_selected_recipient_envelopes(
            &mut store,
            &signer,
            &observed,
            collection,
            &signer,
            &selected,
            hifitime::Epoch::from_unix_seconds(1.0),
        )
        .unwrap();
        assert_eq!(count, 3);
        let delivered =
            super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert_eq!(delivered.open(report.archive, &bob).unwrap(), bytes);
        assert!(delivered.open(report.receipt, &bob).is_err());
        for (id, body) in &report.item_bodies {
            assert!(item::open_exact(
                delivered.store_snapshot().clone(),
                delivered.facts().unwrap(),
                *id,
                *body,
                &bob
            )
            .is_ok());
        }
        assert!(resource::grant(
            &mut store,
            &bob,
            &delivered,
            selected[0],
            carol.verifying_key(),
            DeliveryLimits::default(),
            false
        )
        .is_err());
        assert_eq!(
            super::super::storage::maintain_selected_recipient_envelopes(
                &mut store,
                &signer,
                &delivered,
                collection,
                &signer,
                &selected,
                hifitime::Epoch::from_unix_seconds(1.0)
            )
            .unwrap(),
            0
        );
        for proof in delivered.store_snapshot().proofs().unwrap() {
            let proof = proof.unwrap();
            assert!(report
                .delivery_resources
                .iter()
                .any(|r| proof.resource() == CapabilityResource::from(*r)));
        }
    }
    fn error(bytes: &[u8], limits: ImportLimits) -> String {
        let result = OnePasswordArchive::parse(bytes, limits);
        let error = result
            .err()
            .expect("malicious fixture must be rejected")
            .to_string();
        assert!(!error.contains("SYNTHETIC-PRIVATE-"), "error leaked input");
        error
    }

    #[test]
    fn malformed_json_paths_crc_and_all_resource_limits_fail_safely() {
        let valid = fixture();
        for limits in [
            ImportLimits {
                archive_bytes: 1,
                ..ImportLimits::default()
            },
            ImportLimits {
                member_bytes: 1,
                ..ImportLimits::default()
            },
            ImportLimits {
                expanded_bytes: 1,
                ..ImportLimits::default()
            },
            ImportLimits {
                json_bytes: 1,
                ..ImportLimits::default()
            },
            ImportLimits {
                projected_bytes: 1,
                ..ImportLimits::default()
            },
            ImportLimits {
                entries: 1,
                ..ImportLimits::default()
            },
            ImportLimits {
                items: 1,
                ..ImportLimits::default()
            },
        ] {
            error(&valid, limits);
        }
        for name in [
            "../private",
            "/private",
            "files/../private",
            "files\\private",
            "C:private",
            "files/./private",
        ] {
            error(&zip(&[(name, b"private")]), ImportLimits::default());
        }
        for entries in [
            vec![
                ("export.attributes", br#"{"version":99}"#.as_slice()),
                ("export.data", b"{}".as_slice()),
            ],
            vec![
                ("export.attributes", br#"{"version":3}"#.as_slice()),
                ("export.data", b"private-malformed".as_slice()),
            ],
            vec![
                ("export.attributes", br#"{"version":3}"#.as_slice()),
                (
                    "export.data",
                    br#"{"accounts":[{"vaults":"private"}]}"#.as_slice(),
                ),
            ],
        ] {
            error(&zip(&entries), ImportLimits::default());
        }
        let mut corrupted = valid;
        let start = corrupted
            .windows(b"SYNTHETIC-PRIVATE-binary".len())
            .position(|bytes| bytes == b"SYNTHETIC-PRIVATE-binary")
            .unwrap();
        corrupted[start] ^= 1;
        assert!(error(&corrupted, ImportLimits::default()).contains("integrity"));
    }

    #[test]
    fn zip2_duplicate_name_collapse_and_zip64_multidisk_are_detected() {
        let mut duplicate = zip(&[("files/a", b"a"), ("files/b", b"b")]);
        let positions = duplicate
            .windows(7)
            .enumerate()
            .filter_map(|(index, bytes)| (bytes == b"files/b").then_some(index))
            .collect::<Vec<_>>();
        for position in positions {
            duplicate[position + 6] = b'a';
        }
        assert_eq!(
            zip::ZipArchive::new(Cursor::new(&duplicate)).unwrap().len(),
            1
        );
        assert!(error(&duplicate, ImportLimits::default()).contains("duplicate"));
        for (offset, value) in [(4, 1), (10, 0xff), (16, 0xff)] {
            let mut bytes = fixture();
            let eocd = bytes.len() - 22;
            bytes[eocd + offset] = value;
            error(&bytes, ImportLimits::default());
        }
    }

    #[test]
    fn zip_layout_refusal_characterizes_name_collapse_and_offset_reinterpretation() {
        fn rejection(bytes: &[u8]) -> ProtectedImportCode {
            ProtectedImportRejection::parse(
                OnePasswordArchive::parse(bytes, ImportLimits::default()).err().unwrap(),
            ).code
        }
        // Even harmless-looking repeated empty directory entries are collapsed
        // by zip2. This does NOT establish the contents of any real export.
        for directory in [false, true] {
            let first = if directory { "files/a/" } else { "files/a" };
            let second = if directory { "files/b/" } else { "files/b" };
            let mut bytes = zip(&[
                ("export.attributes", br#"{"version":3}"#),
                ("export.data", br#"{"accounts":[]}"#),
                (first, b""), (second, b""),
            ]);
            let positions: Vec<_> = bytes.windows(second.len()).enumerate()
                .filter_map(|(index, name)| (name == second.as_bytes()).then_some(index)).collect();
            assert_eq!(positions.len(), 2); // local and central headers
            for position in positions { bytes[position + 6] = b'a'; }
            let reader = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
            assert_eq!(standard_zip_entry_count(&bytes).unwrap(), 4);
            assert_eq!(reader.len(), 3);
            assert_eq!(reader.offset(), 0);
            assert_eq!(rejection(&bytes), ProtectedImportCode::ZipLayout);
        }

        let plain = zip(&[
            ("export.attributes", br#"{"version":3}"#),
            ("export.data", br#"{"accounts":[]}"#),
        ]);
        let prefix = b"synthetic-prefix";
        let mut prefixed = prefix.to_vec();
        prefixed.extend_from_slice(&plain);
        let reader = zip::ZipArchive::new(Cursor::new(&prefixed)).unwrap();
        assert_eq!(reader.len(), 2);
        assert_eq!(reader.offset(), prefix.len() as u64);
        // Ordinary relative-offset prepending is already rejected by the EOCD
        // bounds predicate, before the later zip_layout check is reached.
        assert_eq!(rejection(&prefixed), ProtectedImportCode::ZipDirectory);

        // An overstated directory size can satisfy that arithmetic while the
        // zip reader relocates the actual headers. This reaches the OTHER arm
        // of zip_layout with no duplicate names; it is not a reason to relax it.
        let eocd = prefixed.len() - 22;
        let size = u32::from_le_bytes(prefixed[eocd + 12..eocd + 16].try_into().unwrap());
        prefixed[eocd + 12..eocd + 16].copy_from_slice(&(size + prefix.len() as u32).to_le_bytes());
        let reader = zip::ZipArchive::new(Cursor::new(&prefixed)).unwrap();
        assert_eq!(standard_zip_entry_count(&prefixed).unwrap(), 2);
        assert_eq!(reader.len(), 2);
        assert_eq!(reader.offset(), prefix.len() as u64);
        assert_eq!(rejection(&prefixed), ProtectedImportCode::ZipLayout);
    }
}
