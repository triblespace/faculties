//! Explicit local-user import. No public source fingerprint, node envelopes,
//! delivery authority, or implicit conversion of ordinary imports.
use super::*;
use crate::secrets::protected::{self, Protection, Purpose};
use crate::secrets::{BytesHandle, SealedVersion};

/// Ciphertext-only publication prepared outside a live store lease. No retained
/// password or plaintext. An explicit verified retry contains no publication.
pub struct PreparedProtectedImport {
    collection: CollectionHandle,
    author: ed25519_dalek::VerifyingKey,
    fragment: Option<Fragment>,
    report: ImportReport,
}

/// Publication or final durability could not be confirmed. The opaque receipt
/// lets a caller explicitly verify the same import without blindly recreating it.
#[derive(Debug)]
pub struct ProtectedImportPublicationError {
    pub receipt: Id,
}
impl std::fmt::Display for ProtectedImportPublicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("protected import publication outcome unknown; verify the retained receipt")
    }
}
impl std::error::Error for ProtectedImportPublicationError {}

impl PreparedProtectedImport {
    pub fn receipt(&self) -> Id {
        self.report.receipt
    }

    /// Publish once. This is not a transaction across replicas: independent NEW
    /// imports may intentionally produce independent protected copies.
    pub fn publish<S: CollectionStoreExt>(
        self,
        store: &mut S,
        signer: &SigningKey,
        collection: SecretsCollection,
    ) -> Result<ImportReport> {
        if collection.handle() != self.collection || signer.verifying_key() != self.author {
            bail!("protected import: publication binding changed; nothing published");
        }
        if let Some(fragment) = self.fragment {
            store
                .commit(collection.source(), signer, fragment)
                .map_err(|_| ProtectedImportPublicationError {
                    receipt: self.report.receipt,
                })?;
        }
        Ok(self.report)
    }
}

impl OnePasswordArchive<'_> {
    /// `None` explicitly creates a NEW protected copy. `Some(receipt)` performs
    /// an authenticated, exact-source retry and never falls back to creation.
    /// No content-equality hint is public: callers retain the returned opaque
    /// receipt ID when they want idempotent retry. All KDF/encryption/decryption
    /// work uses this detached snapshot, outside the live store lease.
    pub fn prepare_protected<R: StoreRead>(
        &self,
        observed: &SecretsSnapshot<R>,
        signer: &SigningKey,
        created_at: super::super::IntervalValue,
        protection: Protection<'_>,
        previous_receipt: Option<Id>,
    ) -> Result<PreparedProtectedImport> {
        let author = signer.verifying_key();
        let collection = observed.collection();
        if let Some(receipt) = previous_receipt {
            let report = self
                .verify_protected_receipt(observed, signer, receipt, protection.password)
                .map_err(|error| {
                    ProtectedImportRejection::from_error(
                        ProtectedImportStage::ReceiptVerification,
                        ProtectedImportCode::ReceiptRejected,
                        &error,
                    )
                })?;
            return Ok(PreparedProtectedImport {
                collection,
                author,
                fragment: None,
                report,
            });
        }
        let protect = || Protection {
            password: protection.password,
            device: protection
                .device
                .as_ref()
                .map(|device| protected::DeviceProtection {
                    key_id: device.key_id,
                    seal: device.seal,
                }),
        };
        let anchor = protected::prepare_raw(
            "Protected original export",
            self.original,
            created_at,
            protect(),
        )
        .map_err(|error| {
            ProtectedImportRejection::from_error(
                ProtectedImportStage::Encryption,
                ProtectedImportCode::EncryptionFailed,
                &error,
            )
        })?;
        let archive = anchor.secret;
        let (archive_body, archive_resource, archive_envelope) = raw_binding(&anchor)?;
        let mut fragment = anchor.fragment;
        let mut entries = Fragment::empty();
        let mut item_bodies = Vec::new();
        let mut counts = self.counts;
        for (ordinal, (account_path, account, vault_path, vault, item_path, source)) in
            self.items().enumerate()
        {
            let mut projection = self.project(
                &account_path,
                account,
                &vault_path,
                vault,
                &item_path,
                source,
            );
            // Ordinary imports use the title as their public label. Here it is
            // always a private field, including byte-exact whitespace/NUL cases
            // already represented by project().
            if let Some(title) = source.pointer("/overview/title").and_then(Value::as_str) {
                if title == public_title(source) {
                    projection.fields.push(FieldInput {
                        role: ROLE_CUSTOM,
                        label: Some("1Password original title"),
                        value: FieldValue::Text(title),
                    });
                    counts.fields += 1;
                }
            }
            let sealed = protected::prepare_item(
                "Protected imported item",
                ItemInput {
                    fields: &projection.fields,
                    attachments: &projection.attachments,
                },
                created_at,
                protect(),
            )
            .map_err(|error| {
                ProtectedImportRejection::from_error(
                    ProtectedImportStage::Encryption,
                    ProtectedImportCode::EncryptionFailed,
                    &error,
                )
            })?;
            let (body, resource, envelope) =
                find!((body: ItemBody, resource: CollectionHandle, envelope: BytesHandle),
                pattern!(sealed.fragment.facts(), [{ sealed.secret @ structured_body: ?body,
                    resource_handle: ?resource, protected_envelope: ?envelope }]))
                .next()
                .ok_or_else(|| anyhow!("protected import: prepared item binding unavailable"))?;
            entries += entity! { item_ordinal: ordinal as u64, import_item: sealed.secret,
            imported_body: body, resource_handle: resource, protected_envelope: envelope };
            item_bodies.push((sealed.secret, body));
            fragment += sealed.fragment;
        }
        // The raw source digest and all item associations remain encrypted.
        let private = entity! { metadata::tag: KIND_PROTECTED_1PASSWORD_IMPORT,
        source_fingerprint: self.fingerprint(), source_archive: archive,
        secret_body: archive_body, resource_handle: archive_resource,
        protected_envelope: archive_envelope, import_entry*: entries };
        let plaintext: Blob<SimpleArchive> = private.into_facts().to_blob();
        let receipt = protected::prepare_raw(
            "Protected import receipt",
            plaintext.bytes.as_ref(),
            created_at,
            protect(),
        )
        .map_err(|error| {
            ProtectedImportRejection::from_error(
                ProtectedImportStage::Encryption,
                ProtectedImportCode::EncryptionFailed,
                &error,
            )
        })?;
        let receipt_id = receipt.secret;
        fragment += receipt.fragment;
        fragment += entity! { ExclusiveId::force_ref(&receipt_id) @ metadata::tag: KIND_PROTECTED_1PASSWORD_IMPORT };
        Ok(PreparedProtectedImport {
            collection,
            author,
            fragment: Some(fragment),
            report: ImportReport {
                archive,
                receipt: receipt_id,
                item_versions: item_bodies.iter().map(|(id, _)| *id).collect(),
                item_bodies,
                delivery_resources: Vec::new(),
                counts,
                reused: false,
                view_maintenance_pending: false,
            },
        })
    }

    fn verify_protected_receipt<R: StoreRead>(
        &self,
        observed: &SecretsSnapshot<R>,
        signer: &SigningKey,
        receipt: Id,
        password: &[u8],
    ) -> Result<ImportReport> {
        let reader = observed.store_snapshot();
        let source = Collection::<SimpleArchive>::open(reader, observed.collection())?;
        if !source.writer_is_admitted_acquiring(reader, signer.verifying_key())? {
            bail!("protected import: importer is not admitted");
        }
        // Query the source's actual admitted foundations (including missing
        // ones), not merely the materialized view's resident subset. Selected
        // missing evidence must not silently turn into a new import.
        for member in source.admitted(reader)?.members() {
            let selectors = BTreeSet::from([CollectionRecordSelector::CommitMember(
                observed.collection(),
                member.transmute(),
            )]);
            let signed = reader
                .select_records(&selectors)?
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
            // Locator, raw body, envelope and resource must be coupled by ONE
            // genuine author publication. Union annotations are not authority.
            for (body, resource, envelope) in find!((body: BytesHandle, resource: CollectionHandle, envelope: BytesHandle),
                pattern!(&publication, [{ receipt @ metadata::tag: KIND_PROTECTED_1PASSWORD_IMPORT,
                    metadata::tag: KIND_SECRET, secret_body: ?body,
                    resource_handle: ?resource, protected_envelope: ?envelope }]))
            {
                let attempt = || -> Result<ImportReport> {
                    let access = exact_access(
                        reader,
                        &publication,
                        observed.collection(),
                        receipt,
                        body,
                        resource,
                        envelope,
                        Purpose::Get,
                    )?;
                    let bytes = access
                        .unlock_password(password)?
                        .open_raw(reader, &access)?;
                    let private: TribleSet =
                        Blob::<SimpleArchive>::new(bytes.to_vec().into()).try_from_blob()?;
                    for (root, archive, archive_body, archive_resource, archive_envelope) in find!((
                        root: Id, archive: Id, archive_body: BytesHandle, archive_resource: CollectionHandle, archive_envelope: BytesHandle),
                        pattern!(&private, [{ ?root @ metadata::tag: KIND_PROTECTED_1PASSWORD_IMPORT,
                            source_fingerprint: self.fingerprint(), source_archive: ?archive,
                            secret_body: ?archive_body, resource_handle: ?archive_resource,
                            protected_envelope: ?archive_envelope }]))
                    {
                        let candidate = || -> Result<ImportReport> {
                            let access = exact_access(
                                reader,
                                &publication,
                                observed.collection(),
                                archive,
                                archive_body,
                                archive_resource,
                                archive_envelope,
                                Purpose::Get,
                            )?;
                            let original = access
                                .unlock_password(password)?
                                .open_raw(reader, &access)?;
                            if original.as_slice() != self.original {
                                bail!("protected import: source mismatch");
                            }
                            let mut item_bodies = Vec::new();
                            for ordinal in 0..self.counts.items {
                                let mut selected = None;
                                for (id, body, resource, envelope) in find!((id: Id, body: ItemBody, resource: CollectionHandle, envelope: BytesHandle),
                                pattern!(&private, [
                                    { root @ import_entry: _?entry },
                                    { _?entry @ item_ordinal: ordinal as u64, import_item: ?id,
                                        imported_body: ?body, resource_handle: ?resource, protected_envelope: ?envelope }
                                ])) {
                                    let verify = || -> Result<()> {
                                        let access = exact_access(
                                            reader,
                                            &publication,
                                            observed.collection(),
                                            id,
                                            body.transmute(),
                                            resource,
                                            envelope,
                                            Purpose::Inspect,
                                        )?;
                                        let opened = access
                                            .unlock_password(password)?
                                            .open_item(reader.clone(), &access)?;
                                        // Authenticate every projected child, not only the root;
                                        // absent attachment/name/value bytes make retry incomplete.
                                        for child in find!(child: item::PrivateText,
                                            pattern!(opened.facts(), [{ field_text: ?child }]))
                                            .chain(find!(child: item::PrivateText,
                                                pattern!(opened.facts(), [{ private_name: ?child }]))) {
                                            opened.open_child(child)?;
                                        }
                                        for child in find!(child: Inline<inlineencodings::Handle<super::super::encrypted::Encrypted<blobencodings::RawBytes>>>,
                                            pattern!(opened.facts(), [{ attachment_bytes: ?child }]))
                                        {
                                            opened.open_child(child)?;
                                        }
                                        Ok(())
                                    };
                                    if verify().is_ok() {
                                        selected = Some((id, body));
                                        break;
                                    }
                                }
                                item_bodies.push(
                                    selected.ok_or_else(|| {
                                        anyhow!("protected import: incomplete item")
                                    })?,
                                );
                            }
                            let mut counts = self.counts;
                            counts.fields += self
                                .items()
                                .filter(|(_, _, _, _, _, source)| {
                                    source
                                        .pointer("/overview/title")
                                        .and_then(Value::as_str)
                                        .is_some_and(|title| title == public_title(source))
                                })
                                .count();
                            Ok(ImportReport {
                                archive,
                                receipt,
                                item_versions: item_bodies.iter().map(|(id, _)| *id).collect(),
                                item_bodies,
                                delivery_resources: Vec::new(),
                                counts,
                                reused: true,
                                view_maintenance_pending: false,
                            })
                        };
                        if let Ok(report) = candidate() {
                            return Ok(report);
                        }
                    }
                    bail!("protected import: incomplete receipt")
                };
                if let Ok(report) = attempt() {
                    return Ok(report);
                }
            }
        }
        bail!("protected import: selected receipt unavailable or unauthenticated")
    }
}

fn raw_binding(sealed: &SealedVersion) -> Result<(BytesHandle, CollectionHandle, BytesHandle)> {
    find!((body: BytesHandle, resource: CollectionHandle, envelope: BytesHandle),
        pattern!(sealed.fragment.facts(), [{ sealed.secret @ secret_body: ?body,
            resource_handle: ?resource, protected_envelope: ?envelope }]))
    .next()
    .ok_or_else(|| anyhow!("protected import: prepared raw binding unavailable"))
}

/// Exact signed binding, never an ambiguous union-wide envelope selection.
fn exact_access<R: BlobStoreGet>(
    reader: &R,
    publication: &TribleSet,
    collection: CollectionHandle,
    secret: Id,
    body: BytesHandle,
    resource: CollectionHandle,
    envelope: BytesHandle,
    purpose: Purpose,
) -> Result<protected::ProtectedAccess> {
    let structured = purpose != Purpose::Get;
    if !exists!(
        pattern!(publication, [{ secret @ resource_handle: resource, protected_envelope: envelope }])
    ) {
        bail!("protected import: missing signed binding");
    }
    let binding = if structured {
        let item_body: ItemBody = body.transmute();
        if !exists!(
            pattern!(publication, [{ secret @ metadata::tag: KIND_ITEM_VERSION, structured_body: item_body }])
        ) {
            bail!("protected import: missing signed item body");
        }
        entity! { ExclusiveId::force_ref(&secret) @ metadata::tag: KIND_ITEM_VERSION,
        structured_body: item_body, resource_handle: resource, protected_envelope: envelope }
    } else {
        if !exists!(
            pattern!(publication, [{ secret @ metadata::tag: KIND_SECRET, secret_body: body }])
        ) {
            bail!("protected import: missing signed raw body");
        }
        entity! { ExclusiveId::force_ref(&secret) @ metadata::tag: KIND_SECRET,
        secret_body: body, resource_handle: resource, protected_envelope: envelope }
    };
    if !super::super::wrap_recipients(publication, secret).is_empty() {
        bail!("protected import: ordinary recipient envelope is not protected");
    }
    let descriptor: TribleSet = reader.get(resource)?;
    if exists!(
        pattern!(&descriptor, [{ triblespace::core::capability::policy::resource_policy: _?policy }])
    ) || !exists!(pattern!(&descriptor, [{ wrap_secret: secret, secret_body: body }]))
    {
        bail!("protected import: wrong resource protection");
    }
    let access = protected::prepare_access(
        reader,
        binding.facts(),
        collection,
        secret,
        Some(body),
        purpose,
    )?
    .ok_or_else(|| anyhow!("protected import: missing protected envelope"))?;
    if access.resource() != resource {
        bail!("protected import: substituted resource");
    }
    Ok(access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
    use triblespace::core::repo::{
        memoryrepo::MemoryRepo, BlobStoreList, BlobStorePut, SnapshotSource,
    };

    const PASSWORD: &[u8] = b"synthetic-local-user-password";
    fn protection() -> Protection<'static> {
        Protection {
            password: PASSWORD,
            device: None,
        }
    }
    fn at() -> super::super::super::IntervalValue {
        let time = hifitime::Epoch::from_unix_seconds(1.0);
        (time, time).try_to_inline().unwrap()
    }
    fn setup() -> (MemoryRepo, SigningKey, SecretsCollection) {
        let signer = SigningKey::from_bytes(&[73; 32]);
        let mut store = MemoryRepo::for_host(signer.verifying_key());
        let collection = SecretsCollection::register(
            &mut store,
            "protected synthetic fixture",
            CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
        )
        .unwrap();
        (store, signer, collection)
    }

    #[test]
    fn uncertain_publication_retains_receipt_and_does_not_replay_commit() {
        use triblespace::core::collection::CollectionStore;
        struct LostCommitAck {
            inner: MemoryRepo,
            attempts: usize,
        }
        impl BlobStorePut for LostCommitAck {
            type PutError = <MemoryRepo as BlobStorePut>::PutError;
            fn put<S, T>(
                &mut self,
                item: T,
            ) -> std::result::Result<Inline<inlineencodings::Handle<S>>, Self::PutError>
            where
                S: BlobEncoding + 'static,
                T: IntoBlob<S>,
                inlineencodings::Handle<S>: InlineEncoding,
            {
                self.inner.put(item)
            }
        }
        impl CollectionStore for LostCommitAck {
            type InsertError = std::io::Error;
            fn insert(
                &mut self,
                record: CollectionRecord,
            ) -> std::result::Result<(), Self::InsertError> {
                self.attempts += 1;
                self.inner.insert(record).unwrap();
                Err(std::io::Error::other(
                    "synthetic lost commit acknowledgement",
                ))
            }
        }
        let bytes = super::super::tests::fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let (mut store, signer, collection) = setup();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let prepared = archive
            .prepare_protected(&observed, &signer, at(), protection(), None)
            .unwrap();
        let expected = prepared.receipt();
        let mut store = LostCommitAck {
            inner: store,
            attempts: 0,
        };
        let error = prepared
            .publish(&mut store, &signer, collection)
            .unwrap_err();
        let retained = error
            .downcast_ref::<ProtectedImportPublicationError>()
            .unwrap()
            .receipt;
        assert_eq!(retained, expected);
        assert_eq!(store.attempts, 1);
        let observed =
            super::super::super::storage::snapshot(store.inner.snapshot().unwrap(), collection)
                .unwrap();
        let retry = archive
            .prepare_protected(&observed, &signer, at(), protection(), Some(retained))
            .unwrap()
            .publish(&mut store, &signer, collection)
            .unwrap();
        assert!(retry.reused);
        assert_eq!(store.attempts, 1);
    }

    #[test]
    fn protected_import_preserves_source_graph_privacy_and_exact_retry() {
        let bytes = super::super::tests::fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let (mut store, signer, collection) = setup();
        let before =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let prepared = archive
            .prepare_protected(&before, &signer, at(), protection(), None)
            .unwrap();
        assert_eq!(
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection)
                .unwrap()
                .support()
                .len(),
            0
        );
        let first = prepared.publish(&mut store, &signer, collection).unwrap();
        assert!(!first.reused);
        assert!(first.delivery_resources.is_empty());
        let after =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert_eq!(after.support().len(), 1);
        let facts = after.facts().unwrap();
        assert!(!exists!(
            pattern!(facts, [{ source_fingerprint: _?fingerprint }])
        ));
        for id in [first.archive, first.receipt]
            .into_iter()
            .chain(first.item_versions.iter().copied())
        {
            assert!(super::super::super::wrap_recipients(facts, id).is_empty());
            assert!(after.open(id, &signer).is_err());
        }
        for (id, body) in &first.item_bodies {
            assert!(
                item::open_exact(after.store_snapshot().clone(), facts, *id, *body, &signer)
                    .is_err()
            );
            let access = protected::prepare_access(
                after.store_snapshot(),
                facts,
                collection.handle(),
                *id,
                Some(body.transmute()),
                Purpose::Inspect,
            )
            .unwrap()
            .unwrap();
            let opened = access
                .unlock_password(PASSWORD)
                .unwrap()
                .open_item(after.store_snapshot().clone(), &access)
                .unwrap();
            let titles: Vec<_> =
                find!(text: item::PrivateText, pattern!(opened.facts(), [{ field_text: ?text }]))
                    .map(|text| opened.open_child(text).unwrap().bytes.to_vec())
                    .collect();
            assert!(titles
                .iter()
                .any(|text| text == b"Public synthetic title" || text == b"Second public title"));
        }
        for info in after.store_snapshot().blobs() {
            let blob: Blob<blobencodings::UnknownBlob> =
                after.store_snapshot().get(info.unwrap().handle).unwrap();
            for private in [
                b"SYNTHETIC-PRIVATE-".as_slice(),
                b"Public synthetic title",
                b"Second public title",
                b"synthetic@example.invalid",
            ] {
                assert!(!blob
                    .bytes
                    .windows(private.len())
                    .any(|part| part == private));
            }
        }
        let blob_count = after.store_snapshot().blobs().count();
        let retry = archive
            .prepare_protected(&after, &signer, at(), protection(), Some(first.receipt))
            .unwrap()
            .publish(&mut store, &signer, collection)
            .unwrap();
        assert!(retry.reused);
        assert_eq!(retry.item_bodies, first.item_bodies);
        assert_eq!(retry.counts, first.counts);
        assert_eq!(retry.archive, first.archive);
        assert_eq!(retry.receipt, first.receipt);
        let final_snapshot =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert_eq!(final_snapshot.support(), after.support());
        assert_eq!(final_snapshot.store_snapshot().blobs().count(), blob_count);
        assert!(archive
            .prepare_protected(
                &after,
                &signer,
                at(),
                Protection {
                    password: b"wrong",
                    device: None
                },
                Some(first.receipt)
            )
            .is_err());
    }

    #[test]
    fn protected_retry_rejects_ordinary_foreign_missing_and_changed_source() {
        let bytes = super::super::tests::fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let (mut store, signer, collection) = setup();
        let before =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let ordinary = archive
            .import(&mut store, &signer, collection, &before, at())
            .unwrap();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert!(archive
            .prepare_protected(
                &observed,
                &signer,
                at(),
                protection(),
                Some(ordinary.receipt)
            )
            .is_err());
        let protected = archive
            .prepare_protected(&observed, &signer, at(), protection(), None)
            .unwrap()
            .publish(&mut store, &signer, collection)
            .unwrap();
        assert_ne!(protected.archive, ordinary.archive);
        assert_ne!(protected.receipt, ordinary.receipt);
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert_eq!(observed.open(ordinary.archive, &signer).unwrap(), bytes);
        let foreign = SigningKey::from_bytes(&[74; 32]);
        assert!(archive
            .prepare_protected(
                &observed,
                &foreign,
                at(),
                protection(),
                Some(protected.receipt)
            )
            .is_err());
        assert!(archive
            .prepare_protected(&observed, &signer, at(), protection(), Some(genid().id))
            .is_err());
        let changed = super::super::tests::zip(&[
            ("export.attributes", br#"{"version":3}"#),
            ("export.data", br#"{"accounts":[]}"#),
        ]);
        let changed = OnePasswordArchive::parse(&changed, ImportLimits::default()).unwrap();
        assert!(changed
            .prepare_protected(
                &observed,
                &signer,
                at(),
                protection(),
                Some(protected.receipt)
            )
            .is_err());
        assert_eq!(
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection)
                .unwrap()
                .support(),
            observed.support()
        );
    }

    #[test]
    fn protected_preparation_failure_never_publishes_or_echoes_device_error() {
        let bytes = super::super::tests::fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let (mut store, signer, collection) = setup();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let calls = std::cell::Cell::new(0);
        let device = |_: &[u8]| {
            calls.set(calls.get() + 1);
            if calls.get() > 1 {
                bail!("SYNTHETIC-PRIVATE-device-error");
            }
            Ok(vec![1, 2, 3])
        };
        let result = archive.prepare_protected(
            &observed,
            &signer,
            at(),
            Protection {
                password: PASSWORD,
                device: Some(protected::DeviceProtection {
                    key_id: b"synthetic-device",
                    seal: &device,
                }),
            },
            None,
        );
        assert!(result.is_err());
        let error = result.err().unwrap();
        assert!(!error.to_string().contains("SYNTHETIC-PRIVATE"));
        let rejection = error.downcast_ref::<ProtectedImportRejection>().unwrap();
        assert_eq!(rejection.stage, ProtectedImportStage::Encryption);
        assert_eq!(rejection.code, ProtectedImportCode::EncryptionFailed);
        assert_eq!(rejection.os_error_code, None);
        assert_eq!(
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection)
                .unwrap()
                .support()
                .len(),
            0
        );
        assert!(
            OnePasswordArchive::parse(b"SYNTHETIC-PRIVATE-invalid", ImportLimits::default())
                .is_err()
        );
    }

    #[test]
    fn protected_encryption_failure_keeps_only_static_stage_and_io_errno() {
        let bytes = super::super::tests::fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let (mut store, signer, collection) = setup();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let device = |_: &[u8]| -> Result<Vec<u8>> {
            Err(anyhow::Error::new(std::io::Error::from_raw_os_error(24))
                .context("SYNTHETIC-PRIVATE-device/path-and-value"))
        };
        let error = archive
            .prepare_protected(
                &observed,
                &signer,
                at(),
                Protection {
                    password: PASSWORD,
                    device: Some(protected::DeviceProtection {
                        key_id: b"synthetic-device",
                        seal: &device,
                    }),
                },
                None,
            )
            .err()
            .unwrap();
        let rejection = error.downcast_ref::<ProtectedImportRejection>().unwrap();
        assert_eq!(rejection.stage, ProtectedImportStage::Encryption);
        assert_eq!(rejection.code, ProtectedImportCode::EncryptionFailed);
        assert_eq!(rejection.os_error_code, Some(24));
        assert!(!format!("{error:#}").contains("SYNTHETIC-PRIVATE"));
        assert_eq!(
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection)
                .unwrap()
                .support()
                .len(),
            0
        );
    }

    #[test]
    fn protected_receipt_requires_one_signed_publication_and_complete_blobs() {
        let bytes = super::super::tests::fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let (mut store, signer, collection) = setup();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let prepared = archive
            .prepare_protected(&observed, &signer, at(), protection(), None)
            .unwrap();
        let receipt = prepared.receipt();
        // Publish the exact facts, but omit all encrypted dependencies. Missing
        // receipt/original/item evidence cannot silently create a second copy.
        store
            .commit(
                collection.source(),
                &signer,
                prepared.fragment.unwrap().into_facts().into(),
            )
            .unwrap();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert!(archive
            .prepare_protected(&observed, &signer, at(), protection(), Some(receipt))
            .is_err());
        assert_eq!(observed.support().len(), 1);
    }

    #[test]
    fn protected_retry_cannot_assemble_authority_from_split_publications() {
        let bytes = super::super::tests::fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let (mut store, signer, collection) = setup();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let prepared = archive
            .prepare_protected(&observed, &signer, at(), protection(), None)
            .unwrap();
        let receipt = prepared.receipt();
        let mut fragment = prepared.fragment.unwrap();
        let marker = entity! { ExclusiveId::force_ref(&receipt) @ metadata::tag: KIND_PROTECTED_1PASSWORD_IMPORT };
        let complete = fragment.clone();
        *fragment.facts_mut() = fragment.facts().difference(marker.facts());
        store
            .commit(collection.source(), &signer, fragment)
            .unwrap();
        store.commit(collection.source(), &signer, marker).unwrap();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert!(archive
            .prepare_protected(&observed, &signer, at(), protection(), Some(receipt))
            .is_err());
        // All bytes already existed; a genuine complete owner publication is
        // precisely the missing provenance, not an alternate decryption route.
        store
            .commit(collection.source(), &signer, complete)
            .unwrap();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert!(archive
            .prepare_protected(&observed, &signer, at(), protection(), Some(receipt))
            .is_ok());
    }

    #[test]
    fn protected_retry_requires_attachment_bytes_and_ignores_unrelated_envelope_annotations() {
        let bytes = super::super::tests::fixture();
        let archive = OnePasswordArchive::parse(&bytes, ImportLimits::default()).unwrap();
        let (mut store, signer, collection) = setup();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let prepared = archive
            .prepare_protected(&observed, &signer, at(), protection(), None)
            .unwrap();
        let receipt = prepared.receipt();
        let (id, body) = prepared.report.item_bodies[0];
        let fragment = prepared.fragment.unwrap();
        let mut blobs = fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();
        let access = protected::prepare_access(
            &reader,
            fragment.facts(),
            collection.handle(),
            id,
            Some(body.transmute()),
            Purpose::Inspect,
        )
        .unwrap()
        .unwrap();
        let opened = access
            .unlock_password(PASSWORD)
            .unwrap()
            .open_item(reader.clone(), &access)
            .unwrap();
        let missing = find!(child: Inline<inlineencodings::Handle<super::super::super::encrypted::Encrypted<blobencodings::RawBytes>>>,
            pattern!(opened.facts(), [{ attachment_bytes: ?child }])).next().unwrap();
        for info in reader.blobs() {
            let handle = info.unwrap().handle;
            if handle.raw != missing.raw {
                store
                    .put::<blobencodings::UnknownBlob, _>(
                        reader
                            .get::<Blob<blobencodings::UnknownBlob>, _>(handle)
                            .unwrap(),
                    )
                    .unwrap();
            }
        }
        store
            .commit(collection.source(), &signer, fragment.into_facts().into())
            .unwrap();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert!(archive
            .prepare_protected(&observed, &signer, at(), protection(), Some(receipt))
            .is_err());
        assert_eq!(observed.support().len(), 1);
        // Recover the exact missing dependency, not a replacement import.
        store
            .put::<blobencodings::UnknownBlob, _>(
                reader
                    .get::<Blob<blobencodings::UnknownBlob>, _>(
                        missing.transmute::<inlineencodings::Handle<blobencodings::UnknownBlob>>(),
                    )
                    .unwrap(),
            )
            .unwrap();
        let annotation = entity! { ExclusiveId::force_ref(&receipt) @
        protected_envelope: BytesHandle::new([101; 32]), secret_body: BytesHandle::new([102; 32]) };
        let foreign = SigningKey::from_bytes(&[74; 32]);
        store
            .commit(collection.source(), &foreign, annotation)
            .unwrap();
        let observed =
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let retry = archive
            .prepare_protected(&observed, &signer, at(), protection(), Some(receipt))
            .unwrap()
            .publish(&mut store, &signer, collection)
            .unwrap();
        assert!(retry.reused);
        assert_eq!(retry.receipt, receipt);
        assert_eq!(
            super::super::super::storage::snapshot(store.snapshot().unwrap(), collection)
                .unwrap()
                .support(),
            observed.support()
        );
    }
}
