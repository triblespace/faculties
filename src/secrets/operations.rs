//! Configured native operations over the standalone encrypted Secrets core.
use crate::clock;
use crate::secrets::{self, storage as secret_storage};
#[cfg(test)]
use crate::storage::open_secrets_collection_read;
use crate::storage::{AcquiringReader, FacultySnapshot};
use anyhow::{Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use faculties_secrets::protected::{self, ProtectedAccess, Protection, Purpose};
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
/// Where a `find` term may occur. Only the public label and the USERNAME and
/// URL fields are searchable; passwords, notes, custom fields, TOTP seeds and
/// attachments are never decrypted by a search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindRole {
    /// The public item label; matching it decrypts nothing.
    Title,
    Username,
    Url,
}
impl FindRole {
    pub const ALL: [Self; 3] = [Self::Title, Self::Username, Self::Url];
    pub fn name(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Username => "username",
            Self::Url => "url",
        }
    }
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.name() == raw)
    }
    /// The private field role this searches, `None` for the public label.
    fn field_role(self) -> Option<Id> {
        match self {
            Self::Title => None,
            Self::Username => Some(secrets::schema::ROLE_USERNAME),
            Self::Url => Some(secrets::schema::ROLE_URL),
        }
    }
}
/// One matching exact version body. Ids, labels and role names only: the
/// decrypted values that matched never leave the search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindMatch {
    pub id: Id,
    pub body: secrets::item::ItemBody,
    /// None explicitly means the public label bytes are unavailable/undecodable.
    pub name: Option<String>,
    pub roles: Vec<FindRole>,
}
/// One ordinary version this key cannot open. Public facts only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnopenableItem {
    pub id: Id,
    pub body: secrets::item::ItemBody,
    /// None explicitly means the public label bytes are unavailable/undecodable.
    pub name: Option<String>,
    pub created_at: secrets::IntervalValue,
    /// Recipient keys holding an envelope for this version id.
    pub recipients: Vec<secrets::RecipientPublicKey>,
    pub own_envelope: bool,
    pub body_resident: bool,
    /// This key opens the body as a general Secrets record (one without the
    /// password-item tag, such as a Care observation): it is not undelivered,
    /// only not an item.
    pub opens_as_record: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindReport {
    pub matches: Vec<FindMatch>,
    /// Exact version bodies considered.
    pub searched: usize,
    /// User-protected versions: each needs its own local unlock, which a
    /// search never requests, so only their public labels were searched.
    pub protected: usize,
    /// Ordinary versions this key could not open (no envelope delivered to
    /// it, or ciphertext not resident); only their public labels were searched.
    pub unopenable: usize,
    /// Versions this key opens as a general Secrets record that is not a
    /// password item (a Care observation shares the version kind); only their
    /// public labels were searched. Delivered, so not counted as unopenable.
    pub records: usize,
    /// Selected USERNAME/URL children that could not be opened or are not UTF-8.
    pub unreadable_values: usize,
}
impl Secrets {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self { storage }
    }
    /// User protection is prepared before opening storage. No node wrap is made.
    pub fn add_protected(
        &self,
        name: &str,
        plaintext: &[u8],
        protection: Protection<'_>,
    ) -> Result<Id> {
        let sealed = protected::prepare_raw(name, plaintext, clock::point_now()?, protection)?;
        self.publish_protected(sealed)
    }

    pub fn add_item_protected(
        &self,
        name: &str,
        input: secrets::item::ItemInput<'_>,
        protection: Protection<'_>,
    ) -> Result<Id> {
        let sealed = protected::prepare_item(name, input, clock::point_now()?, protection)?;
        self.publish_protected(sealed)
    }

    fn publish_protected(&self, sealed: secrets::SealedVersion) -> Result<Id> {
        use triblespace::core::collection::CollectionStoreExt;
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                true,
            )?;
            pile.commit(collection.source(), signer, sealed.fragment)
                .context("publish protected secret")?;
            drop(
                runtime
                    .block_on(crate::storage::ensure_downstream(
                        pile,
                        collection.source(),
                        signer,
                    ))
                    .context("Protected secret committed, but ensuring its views failed")?,
            );
            Ok(sealed.secret)
        })
    }

    fn prepare_protected(
        &self,
        secret: Id,
        body: Option<secrets::BytesHandle>,
        purpose: Purpose,
    ) -> Result<Option<ProtectedAccess>> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let Some(facts) = snapshot.facts() else {
                return Ok(None);
            };
            protected::prepare_access(
                snapshot.store_snapshot(),
                facts,
                collection.handle(),
                secret,
                body,
                purpose,
            )
        })
    }
    fn storage(&self) -> SecretsStorage<'_> {
        SecretsStorage {
            storage: &self.storage,
        }
    }
    /// Encrypt resident bytes once into the colony vault, returning the exact
    /// newly authored version. Every host the collection's READ policy admits
    /// receives an envelope right after the commit, with no grant.
    pub fn add(&self, name: &str, plaintext: &[u8]) -> Result<Id> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                true,
            )?;
            let secret = add_raw_to_colony(pile, signer, collection, runtime, name, plaintext)?;
            drop(
                runtime
                    .block_on(crate::storage::ensure_downstream(
                        pile,
                        collection.source(),
                        signer,
                    ))
                    .with_context(|| {
                        format!(
                            "Encrypted secret {secret:x} was committed and delivered, but ensuring its derived views failed"
                        )
                    })?,
            );
            Ok(secret)
        })
    }
    /// One fresh whole-item key and one immutable structured version, sealed
    /// into the colony vault and delivered to its hosts like [`Self::add`].
    pub fn add_item(&self, public_label: &str, input: secrets::item::ItemInput<'_>) -> Result<Id> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                true,
            )?;
            let id = secret_storage::add_item_to_colony(
                pile,
                signer,
                collection,
                |pile| acquiring_snapshot(pile, collection, signer, runtime, false),
                public_label,
                input,
                clock::point_now()?,
                clock::now()?,
            )?
            .secret;
            drop(
                runtime
                    .block_on(crate::storage::ensure_downstream(
                        pile,
                        collection.source(),
                        signer,
                    ))
                    .with_context(|| {
                        format!(
                            "Encrypted item {id:x} was committed and delivered, but ensuring its derived views failed"
                        )
                    })?,
            );
            Ok(id)
        })
    }
    /// Import one byte-exact 1PUX export. Validation precedes storage access;
    /// all encrypted versions and receipt relations publish in one commit,
    /// bound to the colony vault like [`Self::add`] and delivered to its hosts.
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
            let colony = secret_storage::colony_vault(&observed, collection)?;
            let mut report = archive.import_into(
                pile,
                signer,
                collection,
                &observed,
                clock::point_now()?,
                Some(colony),
            )?;
            if !report.reused {
                // Deliver the new versions to every host the colony vault
                // admits. A failure leaves them published; maintain completes it.
                let targets = [report.archive, report.receipt]
                    .into_iter()
                    .chain(report.item_versions.iter().copied())
                    .map(SecretTarget::Secret)
                    .collect::<Vec<_>>();
                let after = acquiring_snapshot(pile, collection, signer, runtime, false)?;
                secret_storage::maintain_selected_recipient_envelopes(
                    pile,
                    signer,
                    &after,
                    collection,
                    signer,
                    &targets,
                    clock::now()?,
                )
                .context(
                    "1Password import committed into the colony vault, but delivery to its other hosts failed; run `secrets maintain`",
                )?;
            }
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

    /// Local user-held protection, distinct from the ordinary node-readable
    /// importer. `None` means NEW; `Some(receipt)` verifies an exact retry.
    /// No password or source bytes enter tool arguments or public metadata.
    pub fn import_1password_protected(
        &self,
        bytes: &[u8],
        limits: super::onepassword::ImportLimits,
        protection: Protection<'_>,
        previous_receipt: Option<Id>,
    ) -> Result<super::onepassword::ImportReport> {
        use super::onepassword::{
            ProtectedImportCode, ProtectedImportRejection, ProtectedImportStage,
        };
        let archive = super::onepassword::OnePasswordArchive::parse(bytes, limits)
            .map_err(ProtectedImportRejection::parse)?;
        let (observed, signer) = self
            .storage()
            .with_store(|pile, signer, runtime| {
                let collection = self.storage.open_secrets_collection(
                    pile,
                    signer.verifying_key(),
                    runtime,
                    true,
                )?;
                // No view maintenance here: retries must not append incidental
                // derived work. The acquiring reader outlives this short lease.
                let observed = secret_storage::snapshot_acquiring(
                    AcquiringReader::new(pile.snapshot()?, runtime.clone()),
                    collection,
                )?;
                Ok((observed, signer.clone()))
            })
            .map_err(|error| {
                ProtectedImportRejection::from_error(
                    ProtectedImportStage::DestinationObservation,
                    ProtectedImportCode::DestinationUnavailable,
                    &error,
                )
            })?;
        let created_at = clock::point_now().map_err(ProtectedImportRejection::preparation)?;
        let prepared = archive
            .prepare_protected(&observed, &signer, created_at, protection, previous_receipt)
            .map_err(ProtectedImportRejection::preparation)?;
        let receipt = prepared.receipt();
        self.storage()
            .with_store(|pile, signer, runtime| {
                let collection = self.storage.open_secrets_collection(
                    pile,
                    signer.verifying_key(),
                    runtime,
                    true,
                )?;
                let mut report = prepared.publish(pile, signer, collection)?;
                if !report.reused {
                    report.view_maintenance_pending = runtime
                        .block_on(crate::storage::ensure_downstream(
                            pile,
                            collection.source(),
                            signer,
                        ))
                        .is_err();
                }
                Ok(report)
            })
            .map_err(|_| super::onepassword::ProtectedImportPublicationError { receipt }.into())
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
    /// Every ordinary structured version this key cannot open, with the public
    /// recipients that do hold an envelope for it. Only the item archive is
    /// tried, as `find` does; no field value is opened. Each row says whether
    /// this key has any envelope and whether the ciphertext body is resident,
    /// which separates "never delivered here" from "delivered but unusable".
    /// User-protected versions are not ordinary deliveries and are skipped.
    pub fn unopenable_items(&self) -> Result<Vec<UnopenableItem>> {
        self.storage().with_store(|pile, signer, runtime| {
            use secrets::schema::*;
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
            let reader = snapshot.store_snapshot();
            let own = signer.verifying_key().to_bytes();
            let mut seen = std::collections::BTreeSet::new();
            let mut rows = Vec::new();
            for row in secrets::item::rows(facts) {
                let (id, body) = (row.id, row.body);
                if !seen.insert((id, body))
                    || exists!(pattern!(facts, [{ id @ protected_envelope: _?envelope }]))
                    || secrets::item::open_exact(reader.clone(), facts, id, body, signer).is_ok()
                {
                    continue;
                }
                let recipients = secrets::wrap_recipients(facts, id);
                rows.push(UnopenableItem {
                    id,
                    body,
                    name: secrets::read_text(reader, row.name).ok(),
                    created_at: row.created_at,
                    own_envelope: recipients.contains(&own),
                    body_resident: reader
                        .get::<Blob<secrets::encrypted::Encrypted<blobencodings::SimpleArchive>>, _>(body)
                        .is_ok(),
                    recipients: recipients.into_iter().collect(),
                    opens_as_record: secrets::record::open(reader.clone(), facts, id, signer)
                        .is_ok_and(|records| records.iter().any(|record| record.body() == body)),
                });
            }
            Ok(rows)
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
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            if snapshot.facts().is_some_and(|facts| {
                exists!(pattern!(facts, [
                    { secret @ secrets::schema::protected_envelope: _?envelope }
                ]))
            }) {
                // Body alternatives are public metadata. Do not decrypt merely
                // to list a protected version's exact advertised selectors.
                return Ok(snapshot
                    .facts()
                    .map(|facts| {
                        secrets::item::rows(facts)
                            .into_iter()
                            .filter(|row| row.id == secret)
                            .map(|row| row.body)
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .collect()
                    })
                    .unwrap_or_default());
            }
            Ok(snapshot
                .open_item(secret, signer)?
                .into_iter()
                .map(|item| item.body())
                .collect())
        })
    }
    fn with_item_body<T>(
        &self,
        secret: Id,
        body: secrets::item::ItemBody,
        purpose: Purpose,
        operation: impl FnOnce(&LeasedItem) -> Result<T>,
    ) -> Result<T> {
        let prepared = self.prepare_protected(secret, Some(body.transmute()), purpose.clone())?;
        // The preparation closure has returned and released its store lease.
        // No tool is replayed: only this explicit read continuation proceeds.
        let unlocked = prepared
            .as_ref()
            .map(|request| self.storage.resolve_protected(request))
            .transpose()?;
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let fresh = snapshot
                .facts()
                .map(|facts| {
                    protected::prepare_access(
                        snapshot.store_snapshot(),
                        facts,
                        collection.handle(),
                        secret,
                        Some(body.transmute()),
                        purpose,
                    )
                })
                .transpose()?
                .flatten();
            let item = match (prepared, unlocked, fresh) {
                (Some(prepared), Some(unlocked), Some(fresh)) => {
                    unlocked.validate(&prepared)?;
                    unlocked.open_item(snapshot.store_snapshot().clone(), &fresh)?
                }
                (None, None, None) => snapshot.open_item_body(secret, body, signer)?,
                _ => anyhow::bail!("protected access binding changed; nothing was disclosed"),
            };
            operation(&item)
        })
    }
    /// Export native facts, not an acquiring reader whose owner has closed.
    pub fn open_item_body(&self, secret: Id, body: secrets::item::ItemBody) -> Result<TribleSet> {
        self.with_item_body(secret, body, Purpose::OpenFacts, |item| {
            Ok(item.facts().clone())
        })
    }
    /// Selected cold acquisition and decryption happen while the store lease is
    /// alive. Only independently owned plaintext bytes leave the operation.
    pub fn open_item_child<T: BlobEncoding>(
        &self,
        secret: Id,
        body: secrets::item::ItemBody,
        child: Inline<inlineencodings::Handle<secrets::encrypted::Encrypted<T>>>,
    ) -> Result<Blob<T>> {
        let purpose = if T::id() == blobencodings::UTF8String::id() {
            Purpose::ExportText { child: child.raw }
        } else {
            Purpose::ExportBytes { child: child.raw }
        };
        self.with_item_body(secret, body, purpose, |item| item.open_child(child))
    }
    /// Compute a current TOTP inside the same exact admitted item/store lease.
    /// Only the short-lived code, bounds, parameters and identity leave it.
    pub fn item_totp(
        &self,
        secret: Id,
        body: secrets::item::ItemBody,
        child: secrets::item::PrivateText,
    ) -> Result<secrets::totp::Code> {
        self.with_item_body(secret, body, Purpose::Totp { child: child.raw }, |item| {
            item.totp(child)
        })
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
        self.with_item_body(secret, body, Purpose::Inspect, |item| {
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
    /// Case-insensitive substring search over every structured version: its
    /// public label and, when selected, its USERNAME and URL fields. A version
    /// matches when every term occurs in at least one selected role; the
    /// roles in which any term occurred are reported. An empty role selection
    /// means all three. Only the item archive and its USERNAME/URL children
    /// are decrypted, in one store lease with the node key; user-protected
    /// versions are never unlocked and are counted instead, as are versions
    /// this key cannot open. Unknown roles, missing fields and non-UTF-8
    /// values are simply not matched.
    pub fn find_items(&self, terms: &[&str], roles: &[FindRole]) -> Result<FindReport> {
        let needles: Vec<Vec<char>> = terms
            .iter()
            .map(|term| term.chars().flat_map(char::to_lowercase).collect())
            .collect();
        anyhow::ensure!(
            !needles.is_empty() && needles.iter().all(|needle| !needle.is_empty()),
            "find needs at least one non-empty term"
        );
        let mut roles = if roles.is_empty() {
            FindRole::ALL.to_vec()
        } else {
            roles.to_vec()
        };
        roles.sort();
        roles.dedup();
        let private = roles.iter().any(|role| role.field_role().is_some());
        self.storage().with_store(|pile, signer, runtime| {
            use secrets::item::PrivateText;
            use secrets::schema::*;
            use triblespace::core::metadata;
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let mut report = FindReport::default();
            let Some(facts) = snapshot.facts() else {
                return Ok(report);
            };
            let reader = snapshot.store_snapshot();
            // Scratch for this one search: each exact body once, with every
            // public label its version carries.
            let mut versions = std::collections::BTreeMap::<_, Vec<Option<String>>>::new();
            for row in secrets::item::rows(facts) {
                versions
                    .entry((row.id, row.body))
                    .or_default()
                    .push(secrets::read_text(reader, row.name).ok());
            }
            report.searched = versions.len();
            for ((id, body), names) in versions {
                let mut hit = vec![false; needles.len()];
                let mut matched: Vec<FindRole> = Vec::new();
                let mut note = |role: FindRole, text: &str| {
                    let mut any = false;
                    for (needle, hit) in needles.iter().zip(hit.iter_mut()) {
                        if contains_folded(text, needle) {
                            *hit = true;
                            any = true;
                        }
                    }
                    if any && !matched.contains(&role) {
                        matched.push(role);
                    }
                };
                if roles.contains(&FindRole::Title) {
                    for name in names.iter().flatten() {
                        note(FindRole::Title, name);
                    }
                }
                if private {
                    if exists!(pattern!(facts, [{ id @ protected_envelope: _?envelope }])) {
                        report.protected += 1;
                    } else {
                        match secrets::item::open_exact(reader.clone(), facts, id, body, signer) {
                            Err(_)
                                if secrets::record::open(reader.clone(), facts, id, signer)
                                    .is_ok_and(|records| {
                                        records.iter().any(|record| record.body() == body)
                                    }) =>
                            {
                                report.records += 1
                            }
                            Err(_) => report.unopenable += 1,
                            Ok(item) => {
                                for &role in &roles {
                                    let Some(field_role_id) = role.field_role() else {
                                        continue;
                                    };
                                    for text in find!(text: PrivateText, pattern!(item.facts(), [
                                        { metadata::tag: KIND_ITEM, item_field: _?field },
                                        { _?field @ field_role: field_role_id, field_text: ?text },
                                    ])) {
                                        // The clear blob zeroizes its buffer on drop;
                                        // matching folds case without copying it.
                                        match item.open_child(text) {
                                            Ok(clear) => {
                                                match std::str::from_utf8(clear.bytes.as_ref()) {
                                                    Ok(value) => note(role, value),
                                                    Err(_) => report.unreadable_values += 1,
                                                }
                                            }
                                            Err(_) => report.unreadable_values += 1,
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if hit.iter().all(|hit| *hit) {
                    matched.sort();
                    report.matches.push(FindMatch {
                        id,
                        body,
                        name: names.into_iter().flatten().next(),
                        roles: matched,
                    });
                }
            }
            Ok(report)
        })
    }
    /// Explicitly open one exact version. Callers decide how plaintext is used.
    pub fn get(&self, secret: Id) -> Result<Zeroizing<Vec<u8>>> {
        let prepared = self.prepare_protected(secret, None, Purpose::Get)?;
        let unlocked = prepared
            .as_ref()
            .map(|request| self.storage.resolve_protected(request))
            .transpose()?;
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let fresh = snapshot
                .facts()
                .map(|facts| {
                    protected::prepare_access(
                        snapshot.store_snapshot(),
                        facts,
                        collection.handle(),
                        secret,
                        None,
                        Purpose::Get,
                    )
                })
                .transpose()?
                .flatten();
            match (prepared, unlocked, fresh) {
                (Some(prepared), Some(unlocked), Some(fresh)) => {
                    unlocked.validate(&prepared)?;
                    unlocked.open_raw(snapshot.store_snapshot(), &fresh)
                }
                (None, None, None) => snapshot.open(secret, signer).map(Zeroizing::new),
                _ => anyhow::bail!("protected access binding changed; nothing was disclosed"),
            }
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
    /// Bind every existing ordinary version this key opens, not protected and
    /// not already in a vault, to the colony vault, keeping its version id.
    /// A version whose bound descriptor cannot be read is skipped and
    /// reported. With `commit` false there is no adoption commit and the plan
    /// is returned (reads may still fetch or cache blobs); with `commit` true
    /// the bindings publish in one commit and the vault's hosts receive their
    /// envelopes. Running it again adopts nothing. Plaintext is never printed
    /// or returned, but bodies are decrypted internally to authenticate keys.
    pub fn adopt_colony(&self, commit: bool) -> Result<secret_storage::ColonyAdoption> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                commit,
            )?;
            let report = secret_storage::adopt_into_colony(
                pile,
                signer,
                collection,
                |pile| acquiring_snapshot(pile, collection, signer, runtime, false),
                clock::now()?,
                commit,
            )?;
            if report.committed || report.delivered != 0 {
                drop(
                    runtime
                        .block_on(crate::storage::ensure_downstream(
                            pile,
                            collection.source(),
                            signer,
                        ))
                        .context(
                            "Colony adoption was committed, but ensuring its derived views failed",
                        )?,
                );
            }
            Ok(report)
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

/// Whether `haystack` contains `needle` (already lowercased), comparing
/// lowercased characters without building a lowercased copy of `haystack`,
/// so a decrypted value is never duplicated into an unzeroized buffer.
fn contains_folded(haystack: &str, needle: &[char]) -> bool {
    haystack.char_indices().any(|(start, _)| {
        let mut folded = haystack[start..].chars().flat_map(char::to_lowercase);
        needle.iter().all(|want| folded.next() == Some(*want))
    })
}

/// One native rendering of a search, shared by the CLI and MCP. Ids, exact
/// body handles, public labels and role names only, then one count line.
pub(crate) fn write_find(report: &FindReport, out: &mut crate::out::Out<'_>) -> Result<()> {
    for found in &report.matches {
        let roles = found
            .roles
            .iter()
            .map(|role| role.name())
            .collect::<Vec<_>>()
            .join(",");
        let label = match &found.name {
            Some(name) => format!("public-name {name}"),
            None => "public-name-unavailable".to_owned(),
        };
        out.line(format!(
            "{:x} body blake3:{} matched {roles} {label}",
            found.id,
            hex::encode(found.body.raw)
        ))?;
    }
    out.line(format!(
        "{} match(es) in {} version(s); {} protected and {} not openable by this key, \
         {} general record(s) that are not items (public label searched only); \
         {} unreadable value(s)",
        report.matches.len(),
        report.searched,
        report.protected,
        report.unopenable,
        report.records,
        report.unreadable_values
    ))
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

/// Seal one raw version into the colony vault and deliver it to the vault's
/// hosts: the shared path for `secrets add` and for credentials other
/// faculties keep in Secrets (Profiles, Mail).
pub(crate) fn add_raw_to_colony(
    store: &mut crate::storage::FacultyStore,
    signer: &SigningKey,
    collection: secret_storage::SecretsCollection,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    name: &str,
    plaintext: &[u8],
) -> Result<Id> {
    Ok(secret_storage::add_secret_to_colony(
        store,
        signer,
        collection,
        |store| acquiring_snapshot(store, collection, signer, runtime, false),
        name,
        plaintext,
        clock::point_now()?,
        clock::now()?,
    )?
    .secret)
}

/// Preserve the ordinary local-wrap possession contract. The selected
/// encrypted facts and delivery proofs stay fixed while exact ciphertext,
/// envelope and definition bytes are acquired; no plaintext crosses the wire.
pub(crate) fn acquiring_snapshot(
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
    fn protected_facade_releases_lease_and_never_retains_unlock_or_delivers_node_wrap() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("protected.pile");
        std::fs::File::create(&path).unwrap();
        crate::storage::initialize_signer(&path, None).unwrap();
        let storage = crate::storage::Storage::shared_local(path, None);
        let plain = Secrets::with_storage(storage.clone());
        let id = plain
            .add_protected(
                "synthetic protected",
                b"synthetic private",
                Protection {
                    password: b"synthetic passphrase",
                    device: None,
                },
            )
            .unwrap();
        assert!(plain
            .get(id)
            .unwrap_err()
            .to_string()
            .contains("local Erlkonig unlock"));
        assert_eq!(plain.maintain().unwrap(), 0);
        let recipient = SigningKey::from_bytes(&[9; 32]).verifying_key();
        assert!(plain
            .grant(
                SecretTarget::Secret(id),
                recipient,
                DeliveryLimits::default(),
                false
            )
            .is_err());
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let reentrant_metadata = plain.clone();
        let prompted = Secrets::with_storage(storage.clone().with_protected_access(Arc::new(
            move |request| {
                // Would deadlock if the request still owned the shared store lease.
                assert_eq!(reentrant_metadata.list().unwrap().len(), 1);
                assert_eq!(request.secret(), id);
                assert_eq!(request.purpose(), &Purpose::Get);
                counted.fetch_add(1, Ordering::SeqCst);
                request.unlock_password(b"synthetic passphrase")
            },
        )));
        assert_eq!(prompted.list().unwrap().len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(&*prompted.get(id).unwrap(), b"synthetic private");
        assert_eq!(&*prompted.get(id).unwrap(), b"synthetic private");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let ordinary = plain.add("synthetic ordinary", b"ordinary").unwrap();
        assert_eq!(&*prompted.get(ordinary).unwrap(), b"ordinary");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let denied = Secrets::with_storage(
            storage
                .clone()
                .with_protected_access(Arc::new(|_| anyhow::bail!("local request cancelled"))),
        );
        assert!(denied
            .get(id)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(plain.get(id).is_err());
        storage.close().unwrap();
    }

    #[test]
    fn protected_facade_structured_metadata_does_not_unlock_but_inspection_and_export_do() {
        use secrets::item::{FieldInput, FieldValue, ItemInput, PrivateText};
        use secrets::schema::{field_text, ROLE_PASSWORD};
        use std::sync::{Arc, Mutex};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("protected-item.pile");
        std::fs::File::create(&path).unwrap();
        crate::storage::initialize_signer(&path, None).unwrap();
        let purposes = Arc::new(Mutex::new(Vec::new()));
        let recorded = purposes.clone();
        let storage = crate::storage::Storage::shared_local(path, None).with_protected_access(
            Arc::new(move |request| {
                recorded.lock().unwrap().push(request.purpose().clone());
                request.unlock_password(b"synthetic passphrase")
            }),
        );
        let operations = Secrets::with_storage(storage.clone());
        let id = operations
            .add_item_protected(
                "synthetic protected login",
                ItemInput {
                    fields: &[FieldInput {
                        role: ROLE_PASSWORD,
                        label: None,
                        value: FieldValue::Text("synthetic item value"),
                    }],
                    attachments: &[],
                },
                Protection {
                    password: b"synthetic passphrase",
                    device: None,
                },
            )
            .unwrap();
        assert_eq!(operations.list_items().unwrap().len(), 1);
        let body = operations.open_item(id).unwrap()[0];
        assert!(purposes.lock().unwrap().is_empty());
        let facts = operations.open_item_body(id, body).unwrap();
        let child = find!(child: PrivateText, pattern!(&facts, [{ field_text: ?child }]))
            .next()
            .unwrap();
        let bytes = operations
            .open_item_child::<blobencodings::UTF8String>(id, body, child)
            .unwrap();
        assert_eq!(bytes.bytes.as_ref(), b"synthetic item value");
        assert_eq!(
            *purposes.lock().unwrap(),
            vec![Purpose::OpenFacts, Purpose::ExportText { child: child.raw }]
        );
        storage.close().unwrap();
    }

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
        // Archive, receipt and items are born in the colony vault.
        assert!(operations.adopt_colony(false).unwrap().adopted.is_empty());
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
    fn protected_import_facade_releases_lease_and_retry_keeps_pile_byte_exact() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("synthetic-protected-import.pile");
        std::fs::File::create(&path).unwrap();
        crate::storage::initialize_signer(&path, None).unwrap();
        let storage = crate::storage::Storage::shared_local(path.clone(), None);
        let operations = Secrets::with_storage(storage.clone());
        let bytes = super::super::onepassword::tests::fixture();
        let limits = super::super::onepassword::ImportLimits::default();
        let calls = std::cell::Cell::new(0);
        let seal = |_: &[u8]| {
            // Re-entering the shared owner would deadlock if preparation held
            // its store lease while performing platform/password operations.
            operations.list()?;
            calls.set(calls.get() + 1);
            Ok(vec![1, 2, 3])
        };
        let started = std::time::Instant::now();
        let first = operations
            .import_1password_protected(
                &bytes,
                limits,
                Protection {
                    password: b"synthetic-password",
                    device: Some(protected::DeviceProtection {
                        key_id: b"synthetic-device",
                        seal: &seal,
                    }),
                },
                None,
            )
            .unwrap();
        let new_elapsed = started.elapsed();
        assert_eq!(calls.get(), 4);
        assert!(!first.reused);
        let before = std::fs::read(&path).unwrap();
        let started = std::time::Instant::now();
        let retry = operations
            .import_1password_protected(
                &bytes,
                limits,
                Protection {
                    password: b"synthetic-password",
                    device: None,
                },
                Some(first.receipt),
            )
            .unwrap();
        eprintln!(
            "synthetic debug protected import: items={}, new={:?}, explicit_retry={:?}",
            first.counts.items,
            new_elapsed,
            started.elapsed()
        );
        assert!(retry.reused);
        assert_eq!(retry.receipt, first.receipt);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let error = operations
            .import_1password_protected(
                &bytes,
                limits,
                Protection {
                    password: b"wrong-password",
                    device: None,
                },
                Some(first.receipt),
            )
            .unwrap_err();
        let rejection = error
            .downcast_ref::<super::super::onepassword::ProtectedImportRejection>()
            .unwrap();
        assert_eq!(
            rejection.stage,
            super::super::onepassword::ProtectedImportStage::ReceiptVerification
        );
        assert_eq!(
            rejection.code,
            super::super::onepassword::ProtectedImportCode::ReceiptRejected
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        storage.close().unwrap();
    }

    #[test]
    fn protected_import_rejections_classify_parse_without_opening_destination() {
        use super::super::onepassword::{
            ImportLimits, ProtectedImportCode as Code, ProtectedImportRejection,
            ProtectedImportStage,
        };
        let bytes = super::super::onepassword::tests::fixture();
        let defaults = ImportLimits::default();
        // These deliberately nonexistent paths must never be opened: parse
        // rejection precedes all destination/signing-key work.
        let operations = Secrets::new(
            PathBuf::from("SYNTHETIC-PRIVATE-unused-destination"),
            Some(PathBuf::from("SYNTHETIC-PRIVATE-unused-key")),
        );
        let cases = [
            (
                bytes.as_slice(),
                ImportLimits {
                    archive_bytes: 0,
                    ..defaults
                },
                Code::ArchiveByteLimit,
            ),
            (
                bytes.as_slice(),
                ImportLimits {
                    entries: 0,
                    ..defaults
                },
                Code::ArchiveEntryLimit,
            ),
            (
                bytes.as_slice(),
                ImportLimits {
                    member_bytes: 0,
                    ..defaults
                },
                Code::MemberByteLimit,
            ),
            (
                bytes.as_slice(),
                ImportLimits {
                    expanded_bytes: 0,
                    ..defaults
                },
                Code::ExpandedByteLimit,
            ),
            (
                bytes.as_slice(),
                ImportLimits {
                    items: 0,
                    ..defaults
                },
                Code::ItemCountLimit,
            ),
            (
                bytes.as_slice(),
                ImportLimits {
                    projected_bytes: 0,
                    ..defaults
                },
                Code::ProjectedByteLimit,
            ),
            (
                b"SYNTHETIC-PRIVATE-malformed".as_slice(),
                defaults,
                Code::ZipDirectory,
            ),
        ];
        for (input, limits, code) in cases {
            let error = operations
                .import_1password_protected(
                    input,
                    limits,
                    Protection {
                        password: b"SYNTHETIC-PRIVATE-password",
                        device: None,
                    },
                    None,
                )
                .unwrap_err();
            let rejection = error.downcast_ref::<ProtectedImportRejection>().unwrap();
            assert_eq!(rejection.stage, ProtectedImportStage::Parse);
            assert_eq!(rejection.code, code);
            assert_eq!(rejection.os_error_code, None);
            assert!(!format!("{error:#}").contains("SYNTHETIC-PRIVATE"));
        }
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
    fn find_matches_labels_usernames_and_urls_but_never_other_roles_or_values() {
        use secrets::item::{FieldInput, FieldValue, ItemInput};
        use secrets::schema::{ROLE_CUSTOM, ROLE_NOTES, ROLE_PASSWORD, ROLE_URL, ROLE_USERNAME};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("find.pile");
        std::fs::File::create(&path).unwrap();
        crate::storage::initialize_signer(&path, None).unwrap();
        let unlocks = Arc::new(AtomicUsize::new(0));
        let counted = unlocks.clone();
        let storage = crate::storage::Storage::shared_local(path, None).with_protected_access(
            Arc::new(move |request| {
                counted.fetch_add(1, Ordering::SeqCst);
                request.unlock_password(b"synthetic passphrase")
            }),
        );
        let operations = Secrets::with_storage(storage.clone());
        fn text(role: Id, value: &str) -> FieldInput<'_> {
            FieldInput {
                role,
                label: None,
                value: FieldValue::Text(value),
            }
        }
        let mail = operations
            .add_item(
                "Synthetic Mail Login",
                ItemInput {
                    fields: &[
                        text(ROLE_USERNAME, "SYNTHETIC-USER@Example-Find.test"),
                        text(ROLE_URL, "https://SYNTHETIC-mail.other.test"),
                        text(ROLE_PASSWORD, "SYNTHETIC-PASSWORD-zebra"),
                    ],
                    attachments: &[],
                },
            )
            .unwrap();
        let bank = operations
            .add_item(
                "Synthetic Bank",
                ItemInput {
                    fields: &[
                        text(ROLE_URL, "https://SYNTHETIC-bank.example-find.test/login"),
                        FieldInput {
                            role: ROLE_USERNAME,
                            label: None,
                            value: FieldValue::Integer(7),
                        },
                    ],
                    attachments: &[],
                },
            )
            .unwrap();
        // Every term sits only in roles find must never decrypt.
        let hidden = operations
            .add_item(
                "Synthetic Hidden",
                ItemInput {
                    fields: &[
                        text(ROLE_PASSWORD, "example-find zebra"),
                        text(ROLE_NOTES, "example-find zebra"),
                        FieldInput {
                            role: ROLE_CUSTOM,
                            label: Some("example-find"),
                            value: FieldValue::Text("example-find zebra"),
                        },
                        text(genid().id, "example-find zebra"),
                    ],
                    attachments: &[secrets::item::AttachmentInput {
                        filename: "example-find",
                        bytes: b"example-find zebra",
                    }],
                },
            )
            .unwrap();
        let protected = operations
            .add_item_protected(
                "Synthetic Protected",
                ItemInput {
                    fields: &[text(ROLE_USERNAME, "SYNTHETIC-USER@example-find.test")],
                    attachments: &[],
                },
                Protection {
                    password: b"synthetic passphrase",
                    device: None,
                },
            )
            .unwrap();
        let found = |terms: &[&str], roles: &[FindRole]| {
            let report = operations.find_items(terms, roles).unwrap();
            assert_eq!(report.searched, 4);
            report
        };
        let ids = |report: &FindReport| {
            let mut ids = report.matches.iter().map(|m| m.id).collect::<Vec<_>>();
            ids.sort();
            ids
        };
        let sorted = |mut ids: Vec<Id>| {
            ids.sort();
            ids
        };

        // Username (case-insensitive) and URL; the hidden item never matches,
        // and the protected one is counted, never unlocked.
        let report = found(&["EXAMPLE-find"], &[]);
        assert_eq!(ids(&report), sorted(vec![mail, bank]));
        assert_eq!(report.protected, 1);
        assert_eq!(report.unopenable, 0);
        assert_eq!(report.unreadable_values, 0);
        let roles_of = |report: &FindReport, id: Id| {
            report
                .matches
                .iter()
                .find(|m| m.id == id)
                .unwrap()
                .roles
                .clone()
        };
        assert_eq!(roles_of(&report, mail), vec![FindRole::Username]);
        assert_eq!(roles_of(&report, bank), vec![FindRole::Url]);
        let mail_row = report.matches.iter().find(|m| m.id == mail).unwrap();
        assert_eq!(mail_row.name.as_deref(), Some("Synthetic Mail Login"));
        assert_eq!(mail_row.body, operations.open_item(mail).unwrap()[0]);

        // A password containing the term is never searched.
        assert!(found(&["zebra"], &[]).matches.is_empty());

        // Title only: nothing is opened, so nothing is counted as skipped,
        // and a protected version is found by its public label.
        let report = found(&["synthetic"], &[FindRole::Title]);
        assert_eq!(ids(&report), sorted(vec![mail, bank, hidden, protected]));
        assert!(report.matches.iter().all(|m| m.roles == [FindRole::Title]));
        assert_eq!((report.protected, report.unopenable), (0, 0));
        assert_eq!(ids(&found(&["MAIL login"], &[FindRole::Title])), vec![mail]);

        // Role restriction.
        assert_eq!(ids(&found(&["example-find"], &[FindRole::Url])), vec![bank]);
        assert_eq!(
            ids(&found(&["example-find"], &[FindRole::Username])),
            vec![mail]
        );

        // Every term must occur, each in some selected role.
        let report = found(&["bank", "example-find"], &[]);
        assert_eq!(ids(&report), vec![bank]);
        assert_eq!(
            roles_of(&report, bank),
            vec![FindRole::Title, FindRole::Url]
        );
        assert!(found(&["bank", "example-find"], &[FindRole::Url])
            .matches
            .iter()
            .all(|m| m.id == bank));
        assert!(found(&["bank", "example-find"], &[FindRole::Title])
            .matches
            .is_empty());
        assert!(operations.find_items(&[], &[]).is_err());
        assert!(operations.find_items(&[""], &[]).is_err());
        assert_eq!(unlocks.load(Ordering::SeqCst), 0);

        // The rendering carries ids, labels and role names, never a value.
        let mut text = String::new();
        write_find(
            &found(&["example-find"], &[]),
            &mut crate::out::Out::new(&mut |part| {
                let crate::out::Part::Text { text: part } = part else {
                    panic!("search renders text only");
                };
                text.push_str(&part);
                Ok(())
            }),
        )
        .unwrap();
        assert!(text.contains(&format!("{mail:x} body blake3:")));
        assert!(text.contains("matched username public-name Synthetic Mail Login"));
        assert!(text.contains("matched url public-name Synthetic Bank"));
        assert!(text.contains("2 match(es) in 4 version(s); 1 protected and 0 not openable"));
        for value in [
            "SYNTHETIC-USER",
            "SYNTHETIC-mail",
            "SYNTHETIC-bank",
            "PASSWORD",
            "zebra",
            "https://",
        ] {
            assert!(!text.contains(value), "{value} leaked into {text}");
        }
        storage.close().unwrap();
    }

    #[test]
    fn find_reports_its_synthetic_cost_per_opened_version() {
        use secrets::item::{FieldInput, FieldValue, ItemInput};
        use secrets::schema::{ROLE_PASSWORD, ROLE_URL, ROLE_USERNAME};
        const VERSIONS: usize = 32;
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("find-cost.pile");
        std::fs::File::create(&path).unwrap();
        crate::storage::initialize_signer(&path, None).unwrap();
        let operations = Secrets::new(path, None);
        for index in 0..VERSIONS {
            let username = format!("synthetic-user-{index}@cost.example");
            let url = format!("https://synthetic-{index}.cost.example");
            operations
                .add_item(
                    &format!("synthetic cost {index}"),
                    ItemInput {
                        fields: &[
                            FieldInput {
                                role: ROLE_USERNAME,
                                label: None,
                                value: FieldValue::Text(&username),
                            },
                            FieldInput {
                                role: ROLE_URL,
                                label: None,
                                value: FieldValue::Text(&url),
                            },
                            FieldInput {
                                role: ROLE_PASSWORD,
                                label: None,
                                value: FieldValue::Text("synthetic"),
                            },
                        ],
                        attachments: &[],
                    },
                )
                .unwrap();
        }
        let started = std::time::Instant::now();
        let titles = operations
            .find_items(&["synthetic"], &[FindRole::Title])
            .unwrap();
        let titles_elapsed = started.elapsed();
        let started = std::time::Instant::now();
        let private = operations
            .find_items(&["user-7@"], &[FindRole::Username, FindRole::Url])
            .unwrap();
        let private_elapsed = started.elapsed();
        assert_eq!(titles.matches.len(), VERSIONS);
        assert_eq!(private.matches.len(), 1);
        assert_eq!(private.searched, VERSIONS);
        // Framing: one whole find call over VERSIONS local ordinary versions,
        // this test profile, local temporary pile; the private search opens
        // each archive plus its USERNAME and URL children, the title search
        // opens nothing. Both include the store open and collection ensure.
        eprintln!(
            "synthetic find cost: {VERSIONS} versions; titles only {titles_elapsed:?} total; \
             username+url {private_elapsed:?} total, {:?} per version",
            private_elapsed / VERSIONS as u32
        );
    }

    #[test]
    fn find_counts_a_version_this_key_cannot_open() {
        use secrets::item::{FieldInput, FieldValue, ItemInput};
        use secrets::schema::ROLE_USERNAME;
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("find-unopenable.pile");
        std::fs::File::create(&path).unwrap();
        let signer = crate::storage::initialize_signer(&path, None).unwrap();
        let operations = Secrets::new(path.clone(), None);
        let id = operations
            .add_item(
                "synthetic openable",
                ItemInput {
                    fields: &[FieldInput {
                        role: ROLE_USERNAME,
                        label: None,
                        value: FieldValue::Text("synthetic-user"),
                    }],
                    attachments: &[],
                },
            )
            .unwrap();
        let body = operations.open_item(id).unwrap()[0];
        // A second version advertising that body under another id has no
        // envelope for this key: its label is searched, its fields are not.
        let other = genid().id;
        let mut pile = crate::storage::open_pile_strict(&path).unwrap();
        let collection = open_secrets_collection_read(&mut pile, signer.verifying_key()).unwrap();
        let instant = hifitime::Epoch::from_unix_seconds(0.0);
        let created: secrets::IntervalValue = (instant, instant).try_to_inline().unwrap();
        let mut fragment = Fragment::empty();
        let label = fragment.put::<blobencodings::UTF8String, _>("synthetic unopenable".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&other) @
            metadata::tag: secrets::schema::KIND_ITEM_VERSION,
            metadata::name: label, metadata::created_at: created,
            secrets::schema::structured_body: body,
        };
        pile.commit(collection.source(), &signer, fragment).unwrap();
        pile.close().unwrap();
        let report = operations.find_items(&["synthetic"], &[]).unwrap();
        assert_eq!(report.searched, 2);
        assert_eq!(report.unopenable, 1);
        let mut found = report
            .matches
            .iter()
            .map(|m| (m.id, m.roles.clone()))
            .collect::<Vec<_>>();
        found.sort();
        let mut expected = vec![
            (id, vec![FindRole::Title, FindRole::Username]),
            (other, vec![FindRole::Title]),
        ];
        expected.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn find_counts_a_general_record_apart_from_unopenable_versions() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("find-record.pile");
        std::fs::File::create(&path).unwrap();
        let signer = crate::storage::initialize_signer(&path, None).unwrap();
        let operations = Secrets::new(path.clone(), None);
        // A record without the item tag, delivered to this key: the shape of
        // a Care observation. It opens, it is just not a password item.
        let instant = hifitime::Epoch::from_unix_seconds(0.0);
        let created: secrets::IntervalValue = (instant, instant).try_to_inline().unwrap();
        let sealed = secrets::record::seal(
            &signer,
            "synthetic record",
            created,
            secrets::record::Delivery::Own,
            |_| Ok(TribleSet::new()),
        )
        .unwrap();
        let mut pile = crate::storage::open_pile_strict(&path).unwrap();
        let collection = open_secrets_collection_read(&mut pile, signer.verifying_key()).unwrap();
        pile.commit(collection.source(), &signer, sealed.fragment)
            .unwrap();
        pile.close().unwrap();
        let report = operations.find_items(&["synthetic"], &[]).unwrap();
        assert_eq!(report.searched, 1);
        assert_eq!((report.records, report.unopenable), (1, 0));
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].roles, vec![FindRole::Title]);
        let rows = operations.unopenable_items().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].opens_as_record && rows[0].own_envelope && rows[0].body_resident);
    }

    #[test]
    fn colony_adoption_dry_runs_then_binds_old_versions_once() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("adopt.pile");
        std::fs::File::create(&path).unwrap();
        let signer = crate::storage::initialize_signer(&path, None).unwrap();
        let operations = Secrets::new(path.clone(), None);
        // One version sealed the old way, under its own direct(signer) R.
        let mut pile = crate::storage::open_pile_strict(&path).unwrap();
        let collection = open_secrets_collection_read(&mut pile, signer.verifying_key()).unwrap();
        let instant = hifitime::Epoch::from_unix_seconds(1.0);
        let old = secret_storage::add_secret(
            &mut pile,
            &signer,
            collection,
            "synthetic old",
            b"synthetic old value",
            (instant, instant).try_to_inline().unwrap(),
        )
        .unwrap();
        pile.close().unwrap();
        // New ordinary adds are already in the colony vault; protected ones never are.
        let fresh = operations
            .add("synthetic fresh", b"synthetic fresh value")
            .unwrap();
        let protected = operations
            .add_protected(
                "synthetic protected",
                b"synthetic protected value",
                Protection {
                    password: b"synthetic passphrase",
                    device: None,
                },
            )
            .unwrap();

        let plan = operations.adopt_colony(false).unwrap();
        assert!(!plan.committed);
        assert!(!plan.vault_created, "the fresh add created it");
        assert_eq!(plan.adopted.len(), 1);
        assert_eq!(plan.adopted[0].secret, old);
        assert_eq!(plan.adopted[0].name.as_deref(), Some("synthetic old"));
        assert!(!plan
            .adopted
            .iter()
            .any(|a| a.secret == fresh || a.secret == protected));
        assert_eq!(
            operations.adopt_colony(false).unwrap().adopted.len(),
            1,
            "dry run wrote nothing"
        );

        let done = operations.adopt_colony(true).unwrap();
        assert!(done.committed);
        assert_eq!(done.vault, plan.vault);
        assert_eq!(done.adopted.len(), 1);
        let again = operations.adopt_colony(true).unwrap();
        assert!(!again.committed);
        assert!(again.adopted.is_empty());
        assert_eq!(again.delivered, 0);
        assert_eq!(&*operations.get(old).unwrap(), b"synthetic old value");
        assert_eq!(&*operations.get(fresh).unwrap(), b"synthetic fresh value");
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
