//! Configured native operations over the standalone encrypted Secrets core.
use crate::clock;
use crate::secrets::{self, storage as secret_storage};
#[cfg(test)]
use crate::storage::open_secrets_collection_read;
use crate::storage::{AcquiringReader, FacultySnapshot};
use anyhow::{Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use faculties_secrets::resource::{DeliveryLimits, SecretTarget};
use std::path::PathBuf;
use triblespace::core::capability::CapabilityProofId;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::*;
use zeroize::Zeroizing;

type LeasedItem = secrets::item::OpenedItem<AcquiringReader<FacultySnapshot>>;

#[derive(Clone, Debug)]
pub struct Secrets {
    storage: crate::storage::Storage,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretMetadata {
    pub id: Id,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemMetadata {
    pub id: Id,
    pub body: secrets::item::ItemBody,
    /// None explicitly means the public label bytes are unavailable/undecodable.
    pub name: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeliveryReport {
    pub grants: usize,
    pub added_envelopes: usize,
    pub view_maintenance_pending: bool,
}
impl Secrets {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self { storage }
    }
    fn storage(&self) -> SecretsStorage<'_> {
        SecretsStorage {
            storage: &self.storage,
        }
    }
    /// Encrypt resident bytes once, returning the exact newly authored version.
    pub fn add(&self, name: &str, plaintext: &[u8]) -> Result<Id> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                true,
            )?;
            let secret = secret_storage::add_secret(
                pile,
                signer,
                collection,
                name,
                plaintext,
                clock::point_now()?,
            )?;
            drop(
                runtime
                    .block_on(crate::storage::ensure_downstream(
                        pile,
                        collection.source(),
                        signer,
                    ))
                    .context(
                        "Encrypted secret was committed, but ensuring its derived views failed",
                    )?,
            );
            Ok(secret)
        })
    }
    /// One fresh whole-item key and one immutable structured version.
    pub fn add_item(&self, public_label: &str, input: secrets::item::ItemInput<'_>) -> Result<Id> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                true,
            )?;
            let id = secret_storage::add_item(
                pile,
                signer,
                collection,
                public_label,
                input,
                clock::point_now()?,
            )?;
            drop(
                runtime
                    .block_on(crate::storage::ensure_downstream(
                        pile,
                        collection.source(),
                        signer,
                    ))
                    .context(
                        "Encrypted item was committed, but ensuring its derived views failed",
                    )?,
            );
            Ok(id)
        })
    }
    /// Import one byte-exact 1PUX export. Validation precedes storage access;
    /// all encrypted versions and receipt relations publish in one commit.
    /// Initial DEK delivery is signer-only, independent of collection READ.
    pub fn import_1password(
        &self,
        bytes: &[u8],
        limits: super::onepassword::ImportLimits,
    ) -> Result<super::onepassword::ImportReport> {
        let archive = super::onepassword::OnePasswordArchive::parse(bytes, limits)?;
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                true,
            )?;
            let observed = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let mut report =
                archive.import(pile, signer, collection, &observed, clock::point_now()?)?;
            // A failed attachment does not undo publication. Returning the
            // receipt makes committed success explicit; ordinary later reads
            // can realize the source fragments without this optional upkeep.
            report.view_maintenance_pending = runtime
                .block_on(crate::storage::ensure_downstream(
                    pile,
                    collection.source(),
                    signer,
                ))
                .is_err();
            Ok(report)
        })
    }
    /// Issue native, non-delegating delivery grants for an explicit finite
    /// selection, then deliver missing envelopes in one selected maintenance
    /// pass and one store open. This never grants collection READ/WRITE. A
    /// failure can leave additive grants/envelopes published; exact retries are
    /// safe. An empty target list is refused, never interpreted as all secrets.
    pub fn grant_selected(
        &self,
        targets: &[SecretTarget],
        recipients: &[VerifyingKey],
        limits: DeliveryLimits,
    ) -> Result<DeliveryReport> {
        limits.validate()?;
        if targets.is_empty() {
            anyhow::bail!("selected delivery requires at least one explicit target");
        }
        for recipient in recipients {
            if recipient.is_weak()
                || recipient.to_edwards().compress().to_bytes() != recipient.to_bytes()
            {
                anyhow::bail!("selected delivery requires canonical, non-weak recipient keys");
            }
        }
        let targets = targets
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let recipients = recipients
            .iter()
            .map(VerifyingKey::to_bytes)
            .collect::<std::collections::BTreeSet<_>>();
        if recipients.is_empty() {
            return Ok(DeliveryReport::default());
        }
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                true,
            )?;
            let observed = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let mut proofs = std::collections::BTreeSet::new();
            for target in &targets {
                for recipient in &recipients {
                    proofs.extend(secrets::resource::grant(
                        pile,
                        signer,
                        &observed,
                        *target,
                        VerifyingKey::from_bytes(recipient).expect("validated public key"),
                        limits,
                        false,
                    )?);
                }
            }
            let observed = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let added_envelopes = secret_storage::maintain_selected_recipient_envelopes(
                pile,
                signer,
                &observed,
                collection,
                signer,
                &targets,
                clock::now()?,
            )?;
            let view_maintenance_pending = runtime
                .block_on(crate::storage::ensure_downstream(
                    pile,
                    collection.source(),
                    signer,
                ))
                .is_err();
            Ok(DeliveryReport {
                grants: proofs.len(),
                added_envelopes,
                view_maintenance_pending,
            })
        })
    }
    /// Public labels only; never opens any item ciphertext.
    pub fn list_items(&self) -> Result<Vec<ItemMetadata>> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let Some(facts) = snapshot.facts() else {
                return Ok(Vec::new());
            };
            Ok(secrets::item::rows(facts)
                .into_iter()
                .map(|row| ItemMetadata {
                    id: row.id,
                    body: row.body,
                    name: secrets::read_text(snapshot.store_snapshot(), row.name).ok(),
                })
                .collect())
        })
    }
    /// Disclose every decryptable exact body alternative, without choosing one.
    pub fn open_item(&self, secret: Id) -> Result<Vec<secrets::item::ItemBody>> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            Ok(
                acquiring_snapshot(pile, collection, signer, runtime, false)?
                    .open_item(secret, signer)?
                    .into_iter()
                    .map(|item| item.body())
                    .collect(),
            )
        })
    }
    fn with_item_body<T>(
        &self,
        secret: Id,
        body: secrets::item::ItemBody,
        operation: impl FnOnce(&LeasedItem) -> Result<T>,
    ) -> Result<T> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let item = acquiring_snapshot(pile, collection, signer, runtime, false)?
                .open_item_body(secret, body, signer)?;
            operation(&item)
        })
    }
    /// Export native facts, not an acquiring reader whose owner has closed.
    pub fn open_item_body(&self, secret: Id, body: secrets::item::ItemBody) -> Result<TribleSet> {
        self.with_item_body(secret, body, |item| Ok(item.facts().clone()))
    }
    /// Selected cold acquisition and decryption happen while the store lease is
    /// alive. Only independently owned plaintext bytes leave the operation.
    pub fn open_item_child<T: BlobEncoding>(
        &self,
        secret: Id,
        body: secrets::item::ItemBody,
        child: Inline<inlineencodings::Handle<secrets::encrypted::Encrypted<T>>>,
    ) -> Result<Blob<T>> {
        self.with_item_body(secret, body, |item| item.open_child(child))
    }
    /// Compute a current TOTP inside the same exact admitted item/store lease.
    /// Only the short-lived code, bounds, parameters and identity leave it.
    pub fn item_totp(
        &self,
        secret: Id,
        body: secrets::item::ItemBody,
        child: secrets::item::PrivateText,
    ) -> Result<secrets::totp::Code> {
        self.with_item_body(secret, body, |item| item.totp(child))
    }
    /// Read-only native relation inspection. Values/filenames stay encrypted;
    /// repeated relations and unknown role IDs are emitted without a catalog.
    pub fn inspect_item(
        &self,
        secret: Id,
        body: secrets::item::ItemBody,
        out: &mut crate::out::Out<'_>,
    ) -> Result<()> {
        use secrets::item::PrivateText;
        use secrets::schema::*;
        use triblespace::core::metadata;
        self.with_item_body(secret, body, |item| {
            for (field, role) in find!((field: Id, role: Id), pattern!(item.facts(), [
                {metadata::tag: KIND_ITEM, item_field: ?field}, {?field @ field_role: ?role}])) {
                let known = [(ROLE_USERNAME, "username"), (ROLE_PASSWORD, "password"), (ROLE_URL, "url"),
                    (ROLE_NOTES, "notes"), (ROLE_CUSTOM, "custom")].into_iter().find(|(id, _)| *id == role);
                let label = known.map(|(_, name)| format!(" {name}")).unwrap_or_default();
                out.line(format!("field {field:x} role {role:x}{label}"))?;
            }
            for (field, text) in find!((field: Id, text: PrivateText), pattern!(item.facts(), [
                {metadata::tag: KIND_ITEM, item_field: ?field}, {?field @ field_text: ?text}])) {
                out.line(format!("field {field:x} text blake3:{}", hex::encode(text.raw)))?;
            }
            for (field, name) in find!((field: Id, name: PrivateText), pattern!(item.facts(), [
                {metadata::tag: KIND_ITEM, item_field: ?field}, {?field @ private_name: ?name}])) {
                out.line(format!("field {field:x} private-name text blake3:{}", hex::encode(name.raw)))?;
            }
            for (attachment, bytes) in find!((attachment: Id,
                bytes: Inline<inlineencodings::Handle<secrets::encrypted::Encrypted<blobencodings::RawBytes>>>),
                pattern!(item.facts(), [{metadata::tag: KIND_ITEM, item_attachment: ?attachment},
                    {?attachment @ attachment_bytes: ?bytes}])) {
                out.line(format!("attachment {attachment:x} bytes blake3:{}", hex::encode(bytes.raw)))?;
            }
            for (attachment, name) in find!((attachment: Id, name: PrivateText), pattern!(item.facts(), [
                {metadata::tag: KIND_ITEM, item_attachment: ?attachment}, {?attachment @ private_name: ?name}])) {
                out.line(format!("attachment {attachment:x} private-name text blake3:{}", hex::encode(name.raw)))?;
            }
            Ok(())
        })
    }
    /// Explicitly open one exact version. Callers decide how plaintext is used.
    pub fn get(&self, secret: Id) -> Result<Zeroizing<Vec<u8>>> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            snapshot.open(secret, signer).map(Zeroizing::new)
        })
    }
    /// Metadata only. Listing never attempts plaintext decryption.
    pub fn list(&self) -> Result<Vec<SecretMetadata>> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let Some(facts) = snapshot.facts() else {
                return Ok(Vec::new());
            };
            secrets::secret_rows(facts)
                .into_iter()
                .map(|row| {
                    Ok(SecretMetadata {
                        id: row.id,
                        name: secrets::read_text(snapshot.store_snapshot(), row.name)?,
                    })
                })
                .collect()
        })
    }
    /// Explicit key-delivery maintenance; does not grant new capabilities.
    pub fn maintain(&self) -> Result<usize> {
        self.maintain_selected(&[])
    }
    pub fn maintain_selected(&self, selected: &[SecretTarget]) -> Result<usize> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(pile, signer.verifying_key(), runtime, true)?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, true)?;
            let count = secret_storage::maintain_selected_recipient_envelopes(
                pile,
                signer,
                &snapshot,
                collection,
                signer,
                selected,
                clock::now()?,
            )?;
            if count != 0 {
                drop(
                    runtime.block_on(crate::storage::ensure_downstream(
                        pile,
                        collection.source(),
                        signer,
                    ))
                    .context(
                        "Recipient envelopes were committed, but ensuring their derived views failed",
                    )?,
                );
            }
            Ok(count)
        })
    }
    /// Sign resource-specific delivery authority, without granting collection
    /// READ/WRITE and without delivering or exporting plaintext.
    pub fn grant(
        &self,
        target: SecretTarget,
        recipient: VerifyingKey,
        limits: DeliveryLimits,
        delegate: bool,
    ) -> Result<Vec<CapabilityProofId>> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let snapshot = match target {
                SecretTarget::Resource(_) => secret_storage::snapshot_acquiring(
                    AcquiringReader::new(pile.snapshot()?, std::sync::Arc::clone(runtime)),
                    collection,
                )?,
                SecretTarget::Secret(_) => {
                    acquiring_snapshot(pile, collection, signer, runtime, false)?
                }
            };
            secrets::resource::grant(pile, signer, &snapshot, target, recipient, limits, delegate)
        })
    }
}

/// One native representation shared by the explicit CLI/MCP disclosure paths.
pub(crate) fn totp_json(code: &secrets::totp::Code) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "code": code.code.as_str(),
        "generated_at_unix": code.generated_at_unix,
        "valid_from_unix": code.valid_from_unix,
        "valid_until_unix": code.valid_until_unix,
        "period": code.period,
        "digits": code.digits,
        "algorithm": code.algorithm.as_str(),
        "issuer": code.issuer,
        "account": code.account,
    }))?)
}

#[derive(Clone, Copy)]
struct SecretsStorage<'a> {
    storage: &'a crate::storage::Storage,
}

impl SecretsStorage<'_> {
    fn with_store<T>(
        self,
        operation: impl FnOnce(
            &mut crate::storage::FacultyStore,
            &SigningKey,
            &std::sync::Arc<tokio::runtime::Runtime>,
        ) -> Result<T>,
    ) -> Result<T> {
        self.storage.with_store(operation)
    }
}

/// Preserve the ordinary local-wrap possession contract. The selected
/// encrypted facts and delivery proofs stay fixed while exact ciphertext,
/// envelope and definition bytes are acquired; no plaintext crosses the wire.
fn acquiring_snapshot(
    store: &mut crate::storage::FacultyStore,
    collection: secret_storage::SecretsCollection,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    maintain: bool,
) -> Result<secrets::SecretsSnapshot<AcquiringReader<FacultySnapshot>>> {
    let result = if maintain {
        runtime.block_on(collection.maintain(store, signer))
    } else {
        runtime.block_on(collection.ensure(store, signer))
    };
    let snapshot =
        match result {
            Ok(snapshot) => snapshot,
            Err(error)
                if !maintain
                    && matches!(
            error.downcast_ref::<triblespace::core::collection::CollectionRealizationError>(),
            Some(triblespace::core::collection::CollectionRealizationError::HostMismatch { .. })
        ) =>
            {
                store.snapshot()?
            }
            Err(error) => return Err(error),
        };
    secret_storage::snapshot_acquiring(
        AcquiringReader::new(snapshot, std::sync::Arc::clone(runtime)),
        collection,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use triblespace::core::metadata;

    #[test]
    fn configured_native_import_replays_and_delivery_never_defaults_to_all() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("synthetic-import.pile");
        std::fs::File::create(&path).unwrap();
        crate::storage::initialize_signer(&path, None).unwrap();
        let operations = Secrets::new(path, None);
        let bytes = super::super::onepassword::tests::fixture();
        let limits = super::super::onepassword::ImportLimits::default();
        let imported = operations.import_1password(&bytes, limits).unwrap();
        assert!(!imported.reused);
        assert_eq!(operations.list_items().unwrap().len(), 2);
        assert_eq!(operations.list().unwrap().len(), 2);
        let retry = operations.import_1password(&bytes, limits).unwrap();
        assert!(retry.reused);
        assert_eq!(retry.item_bodies, imported.item_bodies);
        assert_eq!(retry.delivery_resources, imported.delivery_resources);
        assert!(operations
            .grant_selected(&[], &[], DeliveryLimits::default())
            .is_err());
        let targets = imported
            .delivery_resources
            .iter()
            .copied()
            .map(SecretTarget::Resource)
            .collect::<Vec<_>>();
        assert_eq!(
            operations
                .grant_selected(&targets, &[], DeliveryLimits::default())
                .unwrap(),
            DeliveryReport::default()
        );
    }

    #[test]
    fn configured_items_keep_raw_consumers_separate_and_open_typed_selected_values() {
        use secrets::item::{FieldInput, FieldValue, ItemInput, PrivateText};
        use secrets::schema::{field_role, field_text, item_field, KIND_ITEM, ROLE_PASSWORD};
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("items.pile");
        std::fs::File::create(&path).unwrap();
        let signer = crate::storage::initialize_signer(&path, None).unwrap();
        let operations = Secrets::new(path.clone(), None);
        let id = operations
            .add_item(
                "explicit public label",
                ItemInput {
                    fields: &[FieldInput {
                        role: ROLE_PASSWORD,
                        label: None,
                        value: FieldValue::Text("synthetic configured private value"),
                    }],
                    attachments: &[],
                },
            )
            .unwrap();
        let listed = operations.list_items().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].name.as_deref(), Some("explicit public label"));
        assert!(operations.list().unwrap().is_empty());
        assert!(
            operations.get(id).is_err(),
            "raw consumers do not reinterpret structured bodies"
        );
        let alternatives = operations.open_item(id).unwrap();
        assert_eq!(alternatives.len(), 1);
        assert_eq!(listed[0].body, alternatives[0]);
        let facts = operations.open_item_body(id, alternatives[0]).unwrap();
        let selected = find!(value: PrivateText, pattern!(&facts, [
            { metadata::tag: KIND_ITEM, item_field: _?field },
            { _?field @ field_role: ROLE_PASSWORD, field_text: ?value },
        ]))
        .next()
        .unwrap();
        assert_eq!(
            operations
                .open_item_child(id, alternatives[0], selected)
                .unwrap()
                .bytes
                .as_ref(),
            b"synthetic configured private value"
        );
        assert_eq!(
            operations
                .maintain_selected(&[SecretTarget::Secret(id)])
                .unwrap(),
            0
        );
        // A second native metadata row has a valid typed label handle but no
        // resident label bytes. Listing must disclose that row without losing
        // the usable unrelated item, or pretending a label was supplied.
        let missing_id = genid().id;
        let missing_name: Blob<blobencodings::UTF8String> =
            "missing public label fixture".to_owned().to_blob();
        let mut pile = crate::storage::open_pile_strict(&path).unwrap();
        let collection = open_secrets_collection_read(&mut pile, signer.verifying_key()).unwrap();
        let instant = hifitime::Epoch::from_unix_seconds(0.0);
        let created: secrets::IntervalValue = (instant, instant).try_to_inline().unwrap();
        pile.commit(
            collection.source(),
            &signer,
            entity! { ExclusiveId::force_ref(&missing_id) @
                metadata::tag: secrets::schema::KIND_ITEM_VERSION,
                metadata::name: missing_name.get_handle(), metadata::created_at: created,
                secrets::schema::structured_body: alternatives[0],
            },
        )
        .unwrap();
        pile.close().unwrap();
        let listed = operations.list_items().unwrap();
        assert!(listed
            .iter()
            .any(|row| row.id == id && row.name.as_deref() == Some("explicit public label")));
        assert!(listed
            .iter()
            .any(|row| row.id == missing_id && row.name.is_none()));
    }

    #[test]
    fn an_added_secret_is_queryable_and_openable_through_an_ordinary_read() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("secrets.pile");
        std::fs::File::create(&path).unwrap();
        let signer = crate::storage::initialize_signer(&path, None).unwrap();
        let id = Secrets::new(path.clone(), None)
            .add("eager fixture", b"generated disposable secret")
            .unwrap();

        let mut pile = crate::storage::open_pile_strict(&path).unwrap();
        let collection = open_secrets_collection_read(&mut pile, signer.verifying_key()).unwrap();
        // The ordinary read path prepares the target, exactly as get/list do.
        let snapshot = pollster::block_on(secret_storage::ensure_and_snapshot(
            &mut pile, collection, &signer,
        ))
        .unwrap();
        assert!(secrets::secret_rows(snapshot.facts().unwrap())
            .iter()
            .any(|row| row.id == id));
        assert_eq!(
            snapshot.open(id, &signer).unwrap(),
            b"generated disposable secret"
        );
        drop(snapshot);
        pile.close().unwrap();
    }
}
