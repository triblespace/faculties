//! Collection registration, publication, and maintained reads for Secrets.
//!
//! This module deliberately has no vault registry or access inbox. Callers
//! configure the collection descriptors they use. Authorization evidence is
//! interpreted by TribleSpace; Secrets consumes the audience of the immutable
//! resource pinned by each envelope. Collection READ governs encrypted-evidence
//! replication, never key delivery. Historical envelopes remain decryptable;
//! they do not implicitly acquire a new resource or a new delivery authority.

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};

use triblespace::core::collection::{
    succinctarchive_union, Collection, CollectionHandle, CollectionPolicy,
    CollectionRealizationError, CollectionSnapshotExt, CollectionStoreExt,
};
use triblespace::core::repo::async_store::AsyncBlobStoreAcquire;
use triblespace::core::repo::SnapshotSource;
use triblespace::core::repo::{BlobStoreGet, CapabilityProofRead, Store, StoreRead};
use triblespace::macros::{find, pattern};

use super::resource::SecretTarget;
use super::{IntervalValue, SecretsLag, SecretsSnapshot};

/// One logical Secrets policy boundary and its ordinary maintained encodings.
///
/// The source is the only commit target and the only policy boundary. The
/// Succinct and Rank9 collections are attached to it: indexes of its nodes
/// built by the store's host, with no policy of their own; neither is a
/// vault, custody epoch, or authorization boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretsCollection {
    source: Collection<SimpleArchive>,
    succinct: Collection<SuccinctArchiveBlob>,
    rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
}

impl SecretsCollection {
    /// Register the encrypted-evidence collection and its query encodings.
    pub fn register<S>(store: &mut S, name: &str, policy: CollectionPolicy) -> Result<Self>
    where
        S: CollectionStoreExt + SnapshotSource,
        S::Snapshot: BlobStoreGet,
    {
        let source = store
            .collection(name, policy)
            .map_err(|error| anyhow!("register Secrets source collection: {error}"))?;
        Self::from_source(store, source)
    }

    /// Attach the canonical query encodings to one existing source.
    ///
    /// An attached descriptor is the source's handle and the mapping, with
    /// no policy, so the same pair is registered on every host. Delivery
    /// policies belong to secret resources, so old source descriptors stay
    /// valid.
    pub fn from_source<S>(store: &mut S, source: Collection<SimpleArchive>) -> Result<Self>
    where
        S: CollectionStoreExt + SnapshotSource,
        S::Snapshot: BlobStoreGet,
    {
        let succinct = store
            .attach::<SuccinctArchiveBlob>(source, ())
            .map_err(|error| anyhow!("attach Succinct Secrets collection: {error}"))?;
        let rank9 = store
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
            .map_err(|error| anyhow!("attach Rank9 Secrets collection: {error}"))?;
        Ok(Self {
            source,
            succinct,
            rank9,
        })
    }

    pub const fn source(self) -> Collection<SimpleArchive> {
        self.source
    }

    pub const fn handle(self) -> CollectionHandle {
        self.source.handle()
    }

    pub const fn succinct(self) -> Collection<SuccinctArchiveBlob> {
        self.succinct
    }

    pub const fn rank9(self) -> Collection<Rank9AcceleratedSuccinctArchiveBlob> {
        self.rank9
    }

    /// Attach the source's current frontier into both encodings, Succinct
    /// first, without carrying the source. The store must be opened as
    /// `signer`: only its host's MAPs are believed.
    pub async fn ensure<S>(self, store: &mut S, signer: &SigningKey) -> Result<S::Snapshot>
    where
        S: Store + CollectionStoreExt + AsyncBlobStoreAcquire + Send,
    {
        drop(
            store
                .ensure_attached(self.succinct, signer)
                .await
                .context("ensure Succinct Secrets collection")?,
        );
        store
            .ensure_attached(self.rank9, signer)
            .await
            .context("ensure Rank9 Secrets collection")
    }

    /// Carry the source and attach what the carry leaves into both
    /// encodings, Succinct first.
    pub async fn maintain<S>(self, store: &mut S, signer: &SigningKey) -> Result<S::Snapshot>
    where
        S: Store + CollectionStoreExt + AsyncBlobStoreAcquire + Send,
    {
        drop(
            store
                .maintain_attached(self.succinct, signer)
                .await
                .context("maintain Succinct Secrets collection")?,
        );
        store
            .maintain_attached(self.rank9, signer)
            .await
            .context("maintain Rank9 Secrets collection")
    }
}

/// Read the configured collection at one immutable store boundary.
///
/// This never performs maintenance. It takes the attachments the snapshot
/// holds, reads every source foundation no attachment reaches from its own
/// bytes when they are here, and reports how many foundations each encoding
/// has no attachment for ([`SecretsLag`]); a foundation whose bytes are not
/// here is left out, never waited for.
pub fn snapshot<R>(store_snapshot: R, collection: SecretsCollection) -> Result<SecretsSnapshot<R>>
where
    R: StoreRead,
{
    snapshot_with(store_snapshot, collection, false)
}

/// Select encrypted evidence and proof records once, acquiring exact bytes
/// required by that selection through the supplied reader. Replication and
/// maintenance retain the passive [`snapshot`] boundary; this is the explicit
/// foreground read path. Acquisition does not grant delivery authority.
pub fn snapshot_acquiring<R>(
    store_snapshot: R,
    collection: SecretsCollection,
) -> Result<SecretsSnapshot<R>>
where
    R: StoreRead,
{
    snapshot_with(store_snapshot, collection, true)
}

fn snapshot_with<R>(
    store_snapshot: R,
    collection: SecretsCollection,
    acquire: bool,
) -> Result<SecretsSnapshot<R>>
where
    R: StoreRead,
{
    let observed = if acquire {
        store_snapshot.attached_acquiring(collection.rank9)
    } else {
        store_snapshot.attached(collection.rank9)
    }
    .context("observe maintained Secrets collection")?;
    let succinct = if acquire {
        store_snapshot.attached_acquiring(collection.succinct)
    } else {
        store_snapshot.attached(collection.succinct)
    }
    .context("observe maintained Succinct Secrets collection")?;
    let lag = SecretsLag {
        succinct: succinct.residual().len(),
        rank9: observed.residual().len(),
    };
    let (support, facts) = if observed.cover().is_empty() && observed.residual().is_empty() {
        (observed.support().clone(), None)
    } else {
        let read = if acquire {
            succinctarchive_union::read_attached_acquiring(&observed)
        } else {
            succinctarchive_union::read_attached(&observed)
        };
        let (facts, unread) = read
            .context("read maintained Secrets collection")?
            .into_parts();
        // What the facts stand for: the attachments' foundations and every
        // residual one read from its bytes.
        let read = observed
            .residual()
            .difference(&unread)
            .and_then(|read| observed.support().union(&read))
            .map_err(|error| anyhow!("combine the Secrets read's support: {error}"))?;
        (read, Some(facts))
    };
    Ok(SecretsSnapshot::new(
        store_snapshot,
        collection.handle(),
        support,
        lag,
        facts,
    ))
}

/// Maintain the configured collection, then attach its actual resulting snapshot.
pub async fn maintain_and_snapshot<S>(
    store: &mut S,
    collection: SecretsCollection,
    signer: &SigningKey,
) -> Result<SecretsSnapshot<S::Snapshot>>
where
    S: Store + CollectionStoreExt + AsyncBlobStoreAcquire + Send,
{
    let store_snapshot = collection.maintain(store, signer).await?;
    snapshot(store_snapshot, collection)
}

/// Attach the source's current frontier, then read the actual resulting
/// snapshot.
///
/// A read never refuses for lagging. A key that is not the store's host
/// cannot attach, and reads the attachments present and the rest from its
/// bytes; the snapshot's [`SecretsSnapshot::lag`] counts the source
/// foundations no attachment reaches. Other errors propagate. Nothing is
/// carried.
pub async fn ensure_and_snapshot<S>(
    store: &mut S,
    collection: SecretsCollection,
    signer: &SigningKey,
) -> Result<SecretsSnapshot<S::Snapshot>>
where
    S: Store + CollectionStoreExt + AsyncBlobStoreAcquire + Send,
{
    let store_snapshot = match collection.ensure(store, signer).await {
        Ok(snapshot) => snapshot,
        Err(error)
            if matches!(
                error.downcast_ref::<CollectionRealizationError>(),
                Some(CollectionRealizationError::HostMismatch { .. })
            ) =>
        {
            store
                .snapshot()
                .context("freeze resident Secrets target after unavailable upkeep")?
        }
        Err(error) => return Err(error),
    };
    snapshot(store_snapshot, collection)
}

/// Publish one immutable version to the source collection.
///
/// The adding signer is the new resource's delivery root and first recipient.
/// Generic collection admission independently decides whether this commit is
/// in the view; authoring an envelope does not grant collection WRITE or READ.
pub fn add_secret<S>(
    store: &mut S,
    signing_key: &SigningKey,
    collection: SecretsCollection,
    name: &str,
    plaintext: &[u8],
    created_at: IntervalValue,
) -> Result<triblespace::core::id::Id>
where
    S: Store + CollectionStoreExt,
{
    let sealed = super::resource::seal_version(
        collection.handle(),
        signing_key,
        name,
        plaintext,
        created_at,
    )?;
    let secret = sealed.secret;
    store
        .commit(collection.source, signing_key, sealed.fragment)
        .map_err(|error| anyhow!("publish encrypted secret version: {error}"))?;
    Ok(secret)
}

/// Add missing envelopes across every secret in one policy boundary.
///
/// The supplied snapshot fixes both the secrets and self-contained key-delivery
/// proofs to inspect. Concurrent grants and secrets wait for the next additive
/// maintenance call. `now` is the caller's explicit delivery evaluation time,
/// independent of when that storage observation was acquired.
pub fn maintain_recipient_envelopes<S, R>(
    store: &mut S,
    signing_key: &SigningKey,
    secrets: &SecretsSnapshot<R>,
    collection: SecretsCollection,
    holder: &SigningKey,
    now: Epoch,
) -> Result<usize>
where
    S: Store + CollectionStoreExt,
    R: BlobStoreGet + CapabilityProofRead,
{
    maintain_selected_recipient_envelopes(store, signing_key, secrets, collection, holder, &[], now)
}

/// Maintain only the selected versions/resources; an empty selection visits
/// every bound resource this holder can open. Other writers' secrets and legacy
/// unbound envelopes remain untouched rather than failing the whole pass.
pub fn maintain_selected_recipient_envelopes<S, R>(
    store: &mut S,
    signing_key: &SigningKey,
    secrets: &SecretsSnapshot<R>,
    collection: SecretsCollection,
    holder: &SigningKey,
    selected: &[SecretTarget],
    now: Epoch,
) -> Result<usize>
where
    S: Store + CollectionStoreExt,
    R: BlobStoreGet + CapabilityProofRead,
{
    if secrets.collection() != collection.handle() {
        bail!("Secrets snapshot belongs to a different collection");
    }
    let Some(facts) = secrets.facts() else {
        return Ok(0);
    };
    let secret_ids = find!(
        id: triblespace::core::id::Id,
        pattern!(facts, [{
            ?id @ triblespace::core::metadata::tag: super::schema::KIND_SECRET,
        }])
    )
    .collect::<std::collections::BTreeSet<_>>();
    let mut fragment = triblespace::core::trible::Fragment::empty();
    let mut count = 0usize;
    for secret in secret_ids {
        for binding in super::envelope::recover(secrets.store_snapshot(), facts, secret, holder)? {
            if !selected.is_empty()
                && !selected.iter().any(|target| match target {
                    SecretTarget::Secret(id) => *id == secret,
                    SecretTarget::Resource(handle) => *handle == binding.resource,
                })
            {
                continue;
            }
            let recipients = super::resource::recipients(
                secrets.store_snapshot(),
                collection.handle(),
                &binding,
                now,
            )?;
            let envelopes =
                super::envelope::missing(secrets.store_snapshot(), facts, &binding, recipients)?;
            count += envelopes.recipients.len();
            fragment += envelopes.fragment;
        }
    }
    if count == 0 {
        return Ok(0);
    }
    store
        .commit(collection.source, signing_key, fragment)
        .map_err(|error| anyhow!("publish additive recipient envelopes: {error}"))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;

    use anybytes::Bytes;
    use dryoc::types::NewByteArray;
    use ed25519_dalek::VerifyingKey;
    use hifitime::Epoch;
    use rand_core::OsRng;
    use triblespace::core::blob::encodings::UnknownBlob;
    use triblespace::core::blob::{Blob, BlobEncoding, IntoBlob};
    use triblespace::core::capability::{CapabilityProof, CapabilityResource};
    use triblespace::core::collection::{
        grant_collection_capability, grant_collection_read, grant_collection_write,
        write_capability, AdmissionPolicy, CollectionCommit, CollectionData, CollectionPolicy,
        CollectionRead, CollectionRecord, CollectionStore,
    };
    use triblespace::core::inline::encodings::hash::Handle;
    use triblespace::core::inline::{Inline, InlineEncoding};
    use triblespace::core::metadata;
    use triblespace::core::repo::memoryrepo::{MemoryRepo, MemoryRepoSnapshot};
    use triblespace::core::repo::{
        BlobStoreList, BlobStorePut, CapabilityProofStore, SnapshotSource, WantRead,
    };
    use triblespace::prelude::TryToInline;

    use super::super::resource::{self, DeliveryLimits, SecretTarget};
    use super::super::{key_delivery_capability, seal_version};
    use super::*;

    fn at(second: i64) -> IntervalValue {
        let epoch = Epoch::from_unix_seconds(second as f64);
        (epoch, epoch).try_to_inline().unwrap()
    }

    fn direct_policy(key: VerifyingKey) -> CollectionPolicy {
        CollectionPolicy::new(AdmissionPolicy::direct(key), AdmissionPolicy::direct(key))
            .with_capability(
                super::super::key_delivery_definition(),
                AdmissionPolicy::direct(key),
            )
    }

    struct AcquiringStore {
        inner: MemoryRepo,
        offered: BTreeMap<CollectionData, Bytes>,
        acquired: Vec<CollectionData>,
    }

    impl AcquiringStore {
        fn for_host(host: VerifyingKey) -> Self {
            Self {
                inner: MemoryRepo::for_host(host),
                offered: BTreeMap::new(),
                acquired: Vec::new(),
            }
        }
    }

    impl SnapshotSource for AcquiringStore {
        type Snapshot = MemoryRepoSnapshot;
        type SnapshotError = Infallible;

        fn snapshot(&mut self) -> std::result::Result<Self::Snapshot, Self::SnapshotError> {
            self.inner.snapshot()
        }
    }

    impl BlobStorePut for AcquiringStore {
        type PutError = <MemoryRepo as BlobStorePut>::PutError;

        fn put<S, T>(&mut self, item: T) -> std::result::Result<Inline<Handle<S>>, Self::PutError>
        where
            S: BlobEncoding + 'static,
            T: IntoBlob<S>,
            Handle<S>: InlineEncoding,
        {
            self.inner.put(item)
        }
    }

    impl CollectionStore for AcquiringStore {
        type InsertError = <MemoryRepo as CollectionStore>::InsertError;

        fn insert(
            &mut self,
            record: CollectionRecord,
        ) -> std::result::Result<(), Self::InsertError> {
            self.inner.insert(record)
        }
    }

    impl CapabilityProofStore for AcquiringStore {
        type InsertError = <MemoryRepo as CapabilityProofStore>::InsertError;

        fn insert_proof(
            &mut self,
            proof: CapabilityProof,
        ) -> std::result::Result<(), Self::InsertError> {
            self.inner.insert_proof(proof)
        }
    }

    impl AsyncBlobStoreAcquire for AcquiringStore {
        type AcquireError = Infallible;

        fn acquire(
            &mut self,
            handle: Inline<Handle<UnknownBlob>>,
        ) -> impl std::future::Future<Output = std::result::Result<Option<Bytes>, Self::AcquireError>>
               + Send {
            let data = Handle::<UnknownBlob>::to_hash(handle);
            self.acquired.push(data);
            let bytes = self.offered.get(&data).cloned();
            if let Some(bytes) = &bytes {
                self.inner.put::<UnknownBlob, _>(bytes.clone()).unwrap();
            }
            std::future::ready(Ok(bytes))
        }
    }

    fn detached_secret_commit(
        collection: Collection<SimpleArchive>,
        signing_key: &SigningKey,
        name: &str,
        plaintext: &[u8],
        created_at: IntervalValue,
    ) -> (
        triblespace::core::id::Id,
        CollectionCommit,
        Vec<Blob<UnknownBlob>>,
    ) {
        let sealed =
            seal_version(name, plaintext, [signing_key.verifying_key()], created_at).unwrap();
        let secret = sealed.secret;
        let mut staging = MemoryRepo::default();
        staging
            .commit(collection, signing_key, sealed.fragment)
            .unwrap();
        let snapshot = staging.snapshot().unwrap();
        let commit = snapshot
            .records()
            .unwrap()
            .find_map(|record| match record.unwrap() {
                CollectionRecord::Commit(commit) => Some(commit),
                _ => None,
            })
            .expect("staging one fragment publishes one commit");
        let blobs = snapshot
            .blobs()
            .map(|info| {
                let info = info.unwrap();
                snapshot
                    .get(info.handle)
                    .expect("listed staging blob remains readable")
            })
            .collect();
        (secret, commit, blobs)
    }

    /// A replica believes only its own host's MAPs. Another key's
    /// attachments of a commit whose payload is not here read as nothing;
    /// the host attaches only nodes it holds, fetching nothing and recording
    /// no WANT, and once the payload arrives the host attaches the commit
    /// and reads it.
    #[test]
    fn a_replica_uses_only_its_hosts_attachments_and_attaches_once_the_payload_arrives() {
        pollster::block_on(async {
            let authority = SigningKey::generate(&mut OsRng);
            let writer = SigningKey::generate(&mut OsRng);
            let mut staging = MemoryRepo::for_host(writer.verifying_key());
            let collection = SecretsCollection::register(
                &mut staging,
                "attached-elsewhere",
                direct_policy(authority.verifying_key()),
            )
            .unwrap();
            let (secret, commit, blobs) = detached_secret_commit(
                collection.source(),
                &writer,
                "attached",
                b"resident attached value",
                at(40),
            );
            for blob in &blobs {
                staging.put::<UnknownBlob, _>(blob.clone()).unwrap();
            }
            staging.insert(CollectionRecord::Commit(commit)).unwrap();
            let source_grant = CapabilityProof::new(
                CapabilityResource::from(collection.handle()),
                &authority,
                write_capability(),
                writer.verifying_key(),
            );
            staging.insert_proof(source_grant.clone()).unwrap();
            let attached = collection.ensure(&mut staging, &writer).await.unwrap();
            assert!(snapshot(attached, collection).unwrap().contains(secret));

            // Replicate the writer's records, its MAPs among them, and its
            // attachments, but not the commit's payload.
            let realized = staging.snapshot().unwrap();
            let mut store = AcquiringStore::for_host(authority.verifying_key());
            for info in realized.blobs() {
                let info = info.unwrap();
                if Handle::<UnknownBlob>::to_hash(info.handle) != commit.data() {
                    store
                        .inner
                        .put::<UnknownBlob, _>(
                            realized.get::<Blob<UnknownBlob>, _>(info.handle).unwrap(),
                        )
                        .unwrap();
                }
            }
            for record in realized.records().unwrap() {
                store.inner.insert(record.unwrap()).unwrap();
            }
            store.insert_proof(source_grant).unwrap();
            let before = store.snapshot().unwrap();
            let records_before = before.records().unwrap().count();
            let read = snapshot(before, collection).unwrap();
            assert!(!read.contains(secret), "another key's MAPs are not used");
            assert_eq!(
                read.lag(),
                SecretsLag {
                    succinct: 1,
                    rank9: 1
                }
            );

            let observed = ensure_and_snapshot(&mut store, collection, &authority)
                .await
                .unwrap();
            assert!(!observed.contains(secret));
            assert!(store.acquired.is_empty(), "nothing is fetched");
            assert_eq!(store.snapshot().unwrap().wants().unwrap().count(), 0);
            assert_eq!(
                store.snapshot().unwrap().records().unwrap().count(),
                records_before,
                "nothing is attached without the payload"
            );

            // The payload arrives, as sync would bring it.
            let payload = blobs
                .iter()
                .find(|blob| Handle::<UnknownBlob>::to_hash(blob.get_handle()) == commit.data())
                .expect("the staged commit's payload")
                .clone();
            store.inner.put::<UnknownBlob, _>(payload).unwrap();
            let observed = ensure_and_snapshot(&mut store, collection, &authority)
                .await
                .unwrap();
            assert!(observed.contains(secret));
            assert!(observed.lag().is_current());
            assert_eq!(observed.support().len(), 1);
        });
    }

    /// A key that is not the store's host cannot attach. Its read takes the
    /// attachments present, reads the rest from its bytes and publishes
    /// nothing; explicit maintenance by it names the mismatch, and the
    /// host's next pass attaches what it wrote.
    #[test]
    fn a_key_that_is_not_the_host_reads_the_rest_raw_and_publishes_nothing() {
        pollster::block_on(async {
            let owner = SigningKey::generate(&mut OsRng);
            let writer = SigningKey::generate(&mut OsRng);
            let mut store = AcquiringStore::for_host(owner.verifying_key());
            let collection = SecretsCollection::register(
                &mut store,
                "reader-not-host",
                direct_policy(owner.verifying_key()),
            )
            .unwrap();
            let old = add_secret(&mut store, &owner, collection, "old", b"old", at(50)).unwrap();
            let warm = ensure_and_snapshot(&mut store, collection, &owner)
                .await
                .unwrap();
            let support = warm.support().clone();
            assert!(warm.lag().is_current());
            grant_collection_write(
                &mut store,
                collection.handle(),
                &owner,
                writer.verifying_key(),
            )
            .unwrap();
            let new = add_secret(&mut store, &writer, collection, "new", b"new", at(51)).unwrap();
            let records_before = store.snapshot().unwrap().records().unwrap().count();

            let observed = ensure_and_snapshot(&mut store, collection, &writer)
                .await
                .unwrap();
            assert!(observed.contains(old));
            assert!(observed.contains(new), "read from its own bytes");
            // The support is what the facts stand for: the attached commit
            // and the one read from its bytes.
            assert!(support.is_subset(observed.support()).unwrap());
            assert_eq!(observed.support().len(), support.len() + 1);
            assert_eq!(
                observed.lag(),
                SecretsLag {
                    succinct: 1,
                    rank9: 1
                }
            );
            assert_eq!(
                store.snapshot().unwrap().records().unwrap().count(),
                records_before,
            );
            assert!(store.acquired.is_empty());

            let error = maintain_and_snapshot(&mut store, collection, &writer)
                .await
                .err()
                .expect("maintenance by a key that is not the host is refused");
            assert!(matches!(
                error.downcast_ref::<CollectionRealizationError>(),
                Some(CollectionRealizationError::HostMismatch { .. }),
            ));

            let hosted = ensure_and_snapshot(&mut store, collection, &owner)
                .await
                .unwrap();
            assert!(hosted.contains(new));
            assert!(hosted.lag().is_current());
        });
    }

    /// An own commit whose payload is not here has no attachment. The read
    /// still attaches what is present; nothing is fetched and no WANT is
    /// recorded. The commit is admitted all the same, so the read counts it
    /// as lag and a lookup of its secret says the view lags rather than that
    /// the secret does not exist.
    #[test]
    fn ordinary_read_attaches_what_is_present_when_an_own_payload_is_nowhere() {
        pollster::block_on(async {
            let owner = SigningKey::generate(&mut OsRng);
            let mut store = AcquiringStore::for_host(owner.verifying_key());
            let collection = SecretsCollection::register(
                &mut store,
                "missing-source-payload",
                direct_policy(owner.verifying_key()),
            )
            .unwrap();
            let (secret, commit, _) =
                detached_secret_commit(collection.source(), &owner, "cold", b"unavailable", at(60));
            store.insert(CollectionRecord::Commit(commit)).unwrap();

            let observed = ensure_and_snapshot(&mut store, collection, &owner)
                .await
                .unwrap();
            assert!(!observed.contains(secret));
            assert_eq!(
                observed.lag(),
                SecretsLag {
                    succinct: 1,
                    rank9: 1
                }
            );
            let error = observed.open(secret, &owner).unwrap_err().to_string();
            assert!(error.contains("lags its source"), "{error}");
            assert!(store.acquired.is_empty());
            assert_eq!(store.snapshot().unwrap().wants().unwrap().count(), 0);
        });
    }

    fn observed(
        store: &mut MemoryRepo,
        collection: SecretsCollection,
        owner: &SigningKey,
    ) -> SecretsSnapshot<MemoryRepoSnapshot> {
        drop(pollster::block_on(ensure_and_snapshot(store, collection, owner)).unwrap());
        snapshot(store.snapshot().unwrap(), collection).unwrap()
    }

    fn resource_of(
        secrets: &SecretsSnapshot<MemoryRepoSnapshot>,
        secret: triblespace::core::id::Id,
        holder: &SigningKey,
    ) -> CollectionHandle {
        super::super::envelope::recover(
            secrets.store_snapshot(),
            secrets.facts().unwrap(),
            secret,
            holder,
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .resource
    }

    #[test]
    fn collection_read_and_old_collection_delivery_grants_do_not_deliver_new_secrets() {
        let owner = SigningKey::generate(&mut OsRng);
        let bob = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::for_host(owner.verifying_key());
        let collection = SecretsCollection::register(
            &mut store,
            "secrets",
            direct_policy(owner.verifying_key()),
        )
        .unwrap();
        grant_collection_read(&mut store, collection.handle(), &owner, bob.verifying_key())
            .unwrap();
        grant_collection_capability(
            &mut store,
            collection.handle(),
            key_delivery_capability(),
            &owner,
            bob.verifying_key(),
        )
        .unwrap();
        let secret = add_secret(&mut store, &owner, collection, "token", b"value", at(1)).unwrap();
        let before = observed(&mut store, collection, &owner);
        assert_eq!(before.open(secret, &owner).unwrap(), b"value");
        assert!(before.open(secret, &bob).is_err());
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &owner,
                &before,
                collection,
                &owner,
                Epoch::from_unix_seconds(10.0),
            )
            .unwrap(),
            0
        );
        assert!(collection
            .source()
            .reader_is_admitted(before.store_snapshot(), bob.verifying_key())
            .unwrap());
        resource::grant(
            &mut store,
            &owner,
            &before,
            SecretTarget::Secret(secret),
            bob.verifying_key(),
            DeliveryLimits::default(),
            false,
        )
        .unwrap();
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &owner,
                &before,
                collection,
                &owner,
                Epoch::from_unix_seconds(10.0),
            )
            .unwrap(),
            0,
            "frozen AUTH frontier"
        );
        let current = observed(&mut store, collection, &owner);
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &owner,
                &current,
                collection,
                &owner,
                Epoch::from_unix_seconds(11.0),
            )
            .unwrap(),
            1
        );
        let after = observed(&mut store, collection, &owner);
        assert_eq!(after.open(secret, &bob).unwrap(), b"value");
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &owner,
                &after,
                collection,
                &owner,
                Epoch::from_unix_seconds(12.0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            super::super::secret_rows_for(before.facts().unwrap(), secret)[0].body,
            super::super::secret_rows_for(after.facts().unwrap(), secret)[0].body
        );
    }

    #[test]
    fn historical_collection_descriptor_and_open_replication_need_no_delivery_binding() {
        let owner = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::for_host(owner.verifying_key());
        let source = store
            .collection(
                "historical",
                CollectionPolicy::new(
                    AdmissionPolicy::Open,
                    AdmissionPolicy::direct(owner.verifying_key()),
                ),
            )
            .unwrap();
        let handle = source.handle();
        let collection = SecretsCollection::from_source(&mut store, source).unwrap();
        assert_eq!(collection.handle(), handle);
        let secret =
            add_secret(&mut store, &owner, collection, "token", b"private", at(2)).unwrap();
        assert_eq!(
            observed(&mut store, collection, &owner)
                .open(secret, &owner)
                .unwrap(),
            b"private"
        );
    }

    #[test]
    fn legacy_envelopes_open_but_cannot_infer_new_delivery_roots() {
        let owner = SigningKey::generate(&mut OsRng);
        let bob = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::for_host(owner.verifying_key());
        let collection =
            SecretsCollection::register(&mut store, "legacy", direct_policy(owner.verifying_key()))
                .unwrap();
        let sealed = seal_version(
            "old",
            b"already delivered",
            [owner.verifying_key(), bob.verifying_key()],
            at(3),
        )
        .unwrap();
        let secret = sealed.secret;
        store
            .commit(collection.source(), &owner, sealed.fragment)
            .unwrap();
        let current = observed(&mut store, collection, &owner);
        assert_eq!(current.open(secret, &bob).unwrap(), b"already delivered");
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &owner,
                &current,
                collection,
                &owner,
                Epoch::from_unix_seconds(500.0),
            )
            .unwrap(),
            0
        );
        assert!(resource::grant(
            &mut store,
            &owner,
            &current,
            SecretTarget::Secret(secret),
            bob.verifying_key(),
            DeliveryLimits::default(),
            false
        )
        .is_err());
    }

    #[test]
    fn an_offline_commit_preserves_its_own_adding_signer_after_write_admission() {
        let owner = SigningKey::generate(&mut OsRng);
        let writer = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::for_host(owner.verifying_key());
        let collection = SecretsCollection::register(
            &mut store,
            "offline",
            direct_policy(owner.verifying_key()),
        )
        .unwrap();
        let secret =
            add_secret(&mut store, &writer, collection, "token", b"offline", at(3)).unwrap();
        assert!(!observed(&mut store, collection, &owner).contains(secret));
        grant_collection_write(
            &mut store,
            collection.handle(),
            &owner,
            writer.verifying_key(),
        )
        .unwrap();
        // Admitted to the source, the commit is a node of it like any other:
        // the host attaches it on its next read, and nobody needs WRITE on
        // the encodings.
        let after = observed(&mut store, collection, &owner);
        assert!(after.lag().is_current());
        assert_eq!(after.open(secret, &writer).unwrap(), b"offline");
        assert!(
            after.open(secret, &owner).is_err(),
            "collection owner is not every secret's recipient"
        );
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &owner,
                &after,
                collection,
                &owner,
                Epoch::from_unix_seconds(10.0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn selected_maintenance_and_grants_do_not_cross_secret_versions() {
        let alice = SigningKey::generate(&mut OsRng);
        let bob = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::for_host(alice.verifying_key());
        let collection = SecretsCollection::register(
            &mut store,
            "selected",
            direct_policy(alice.verifying_key()),
        )
        .unwrap();
        let first = add_secret(&mut store, &alice, collection, "token", b"first", at(1)).unwrap();
        let second = add_secret(&mut store, &alice, collection, "token", b"second", at(2)).unwrap();
        let before = observed(&mut store, collection, &alice);
        for secret in [first, second] {
            resource::grant(
                &mut store,
                &alice,
                &before,
                SecretTarget::Secret(secret),
                bob.verifying_key(),
                DeliveryLimits::default(),
                false,
            )
            .unwrap();
        }
        let current = observed(&mut store, collection, &alice);
        assert_eq!(
            maintain_selected_recipient_envelopes(
                &mut store,
                &alice,
                &current,
                collection,
                &alice,
                &[SecretTarget::Secret(first)],
                Epoch::from_unix_seconds(10.0),
            )
            .unwrap(),
            1
        );
        let after = observed(&mut store, collection, &alice);
        assert_eq!(after.open(first, &bob).unwrap(), b"first");
        assert!(after.open(second, &bob).is_err());
        let resource = resource_of(&after, second, &alice);
        assert_eq!(
            maintain_selected_recipient_envelopes(
                &mut store,
                &alice,
                &after,
                collection,
                &alice,
                &[SecretTarget::Resource(resource)],
                Epoch::from_unix_seconds(10.0),
            )
            .unwrap(),
            1
        );
        let third = add_secret(&mut store, &alice, collection, "token", b"third", at(3)).unwrap();
        let current = observed(&mut store, collection, &alice);
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &alice,
                &current,
                collection,
                &alice,
                Epoch::from_unix_seconds(10.0),
            )
            .unwrap(),
            0
        );
        assert!(current.open(third, &bob).is_err());
    }

    #[test]
    fn resource_delegation_needs_no_dek_and_ancestor_deadlines_limit_only_new_delivery() {
        let alice = SigningKey::generate(&mut OsRng);
        let bob = SigningKey::generate(&mut OsRng);
        let carol = SigningKey::generate(&mut OsRng);
        let dave = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::for_host(alice.verifying_key());
        let collection =
            SecretsCollection::register(&mut store, "expiry", direct_policy(alice.verifying_key()))
                .unwrap();
        let secret = add_secret(&mut store, &alice, collection, "token", b"kept", at(1)).unwrap();
        let before = observed(&mut store, collection, &alice);
        let resource = resource_of(&before, secret, &alice);
        resource::grant(
            &mut store,
            &alice,
            &before,
            SecretTarget::Secret(secret),
            bob.verifying_key(),
            DeliveryLimits {
                not_before: Some(Epoch::from_unix_seconds(50.0)),
                expires_at: Some(Epoch::from_unix_seconds(150.0)),
            },
            true,
        )
        .unwrap();
        let delegated = observed(&mut store, collection, &alice);
        assert!(delegated.open(secret, &bob).is_err());
        resource::grant(
            &mut store,
            &bob,
            &delegated,
            SecretTarget::Resource(resource),
            carol.verifying_key(),
            DeliveryLimits::default(),
            false,
        )
        .unwrap();
        let early = observed(&mut store, collection, &alice);
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &alice,
                &early,
                collection,
                &alice,
                Epoch::from_unix_seconds(49.0),
            )
            .unwrap(),
            0
        );
        let expired = observed(&mut store, collection, &alice);
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &alice,
                &expired,
                collection,
                &alice,
                Epoch::from_unix_seconds(150.0),
            )
            .unwrap(),
            0
        );
        let current = observed(&mut store, collection, &alice);
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &alice,
                &current,
                collection,
                &alice,
                Epoch::from_unix_seconds(100.0),
            )
            .unwrap(),
            2
        );
        // A child without bounds cannot erase its parent's restriction.
        let later = observed(&mut store, collection, &alice);
        resource::grant(
            &mut store,
            &bob,
            &later,
            SecretTarget::Resource(resource),
            dave.verifying_key(),
            DeliveryLimits::default(),
            false,
        )
        .unwrap();
        let later = observed(&mut store, collection, &alice);
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &alice,
                &later,
                collection,
                &alice,
                Epoch::from_unix_seconds(200.0),
            )
            .unwrap(),
            0
        );
        assert_eq!(later.open(secret, &bob).unwrap(), b"kept");
        assert_eq!(later.open(secret, &carol).unwrap(), b"kept");
        assert!(later.open(secret, &dave).is_err());
        assert!(!collection
            .source()
            .reader_is_admitted(later.store_snapshot(), bob.verifying_key())
            .unwrap());
        assert!(!collection
            .source()
            .writer_is_admitted(later.store_snapshot(), bob.verifying_key())
            .unwrap());
    }

    #[test]
    fn invocation_only_grant_cannot_delegate() {
        let alice = SigningKey::generate(&mut OsRng);
        let bob = SigningKey::generate(&mut OsRng);
        let carol = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::for_host(alice.verifying_key());
        let collection =
            SecretsCollection::register(&mut store, "invoke", direct_policy(alice.verifying_key()))
                .unwrap();
        let secret = add_secret(&mut store, &alice, collection, "token", b"value", at(1)).unwrap();
        let before = observed(&mut store, collection, &alice);
        let resource = resource_of(&before, secret, &alice);
        resource::grant(
            &mut store,
            &alice,
            &before,
            SecretTarget::Secret(secret),
            bob.verifying_key(),
            DeliveryLimits::default(),
            false,
        )
        .unwrap();
        let current = observed(&mut store, collection, &alice);
        assert!(resource::grant(
            &mut store,
            &bob,
            &current,
            SecretTarget::Resource(resource),
            carol.verifying_key(),
            DeliveryLimits::default(),
            false
        )
        .is_err());
    }

    #[test]
    fn arbitrary_policy_fact_and_foreign_collection_do_not_rebind_a_real_dek() {
        use triblespace::core::capability::policy::{
            resource_collection, resource_handle, resource_policy,
        };
        use triblespace::prelude::*;
        let alice = SigningKey::generate(&mut OsRng);
        let attacker = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::for_host(alice.verifying_key());
        let collection =
            SecretsCollection::register(&mut store, "honest", direct_policy(alice.verifying_key()))
                .unwrap();
        let other = SecretsCollection::register(
            &mut store,
            "other",
            direct_policy(attacker.verifying_key()),
        )
        .unwrap();
        let secret = add_secret(
            &mut store,
            &alice,
            collection,
            "token",
            b"real secret",
            at(1),
        )
        .unwrap();
        let before = observed(&mut store, collection, &alice);
        let binding = super::super::envelope::recover(
            before.store_snapshot(),
            before.facts().unwrap(),
            secret,
            &alice,
        )
        .unwrap()
        .remove(0);
        let false_descriptor = entity! {
            metadata::tag: super::super::schema::KIND_SECRET_RESOURCE,
            super::super::schema::wrap_secret: secret,
            super::super::schema::secret_body: binding.body,
            resource_collection: other.handle(),
            resource_policy*: AdmissionPolicy::direct(attacker.verifying_key()).binding(key_delivery_capability()),
        };
        let false_resource = store
            .put::<SimpleArchive, _>(false_descriptor.facts().clone())
            .unwrap();
        let same_collection_descriptor = entity! {
            metadata::tag: super::super::schema::KIND_SECRET_RESOURCE,
            super::super::schema::wrap_secret: secret,
            super::super::schema::secret_body: binding.body,
            resource_collection: collection.handle(),
            resource_policy*: AdmissionPolicy::direct(attacker.verifying_key()).binding(key_delivery_capability()),
        };
        let same_collection_resource = store
            .put::<SimpleArchive, _>(same_collection_descriptor.facts().clone())
            .unwrap();
        grant_collection_write(
            &mut store,
            collection.handle(),
            &alice,
            attacker.verifying_key(),
        )
        .unwrap();
        store.commit(collection.source(), &attacker, entity! {
            ExclusiveId::force_ref(&secret) @ resource_handle*: [false_resource, same_collection_resource],
            resource_policy*: AdmissionPolicy::direct(attacker.verifying_key()).binding(key_delivery_capability()),
        }).unwrap();
        store
            .insert_proof(CapabilityProof::new(
                CapabilityResource::from(binding.resource),
                &attacker,
                key_delivery_capability(),
                attacker.verifying_key(),
            ))
            .unwrap();
        store
            .insert_proof(CapabilityProof::new(
                CapabilityResource::from(same_collection_resource),
                &attacker,
                key_delivery_capability(),
                attacker.verifying_key(),
            ))
            .unwrap();
        let current = observed(&mut store, collection, &alice);
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &alice,
                &current,
                collection,
                &alice,
                Epoch::from_unix_seconds(10.0),
            )
            .unwrap(),
            0
        );
        assert!(current.open(secret, &attacker).is_err());
        assert!(
            resource::grant(
                &mut store,
                &attacker,
                &current,
                SecretTarget::Resource(false_resource),
                attacker.verifying_key(),
                DeliveryLimits::default(),
                false
            )
            .is_err(),
            "immutable containing collection differs"
        );
    }

    #[test]
    fn forged_well_shaped_recipient_wrap_does_not_suppress_delivery() {
        use triblespace::prelude::*;
        let alice = SigningKey::generate(&mut OsRng);
        let bob = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::for_host(alice.verifying_key());
        let collection = SecretsCollection::register(
            &mut store,
            "forged-wrap",
            direct_policy(alice.verifying_key()),
        )
        .unwrap();
        let secret = add_secret(&mut store, &alice, collection, "token", b"value", at(1)).unwrap();
        let before = observed(&mut store, collection, &alice);
        let binding = super::super::envelope::recover(
            before.store_snapshot(),
            before.facts().unwrap(),
            secret,
            &alice,
        )
        .unwrap()
        .remove(0);
        let false_binding = super::super::envelope::BoundKey {
            secret: binding.secret,
            body: binding.body,
            resource: binding.resource,
            dek: dryoc::dryocsecretbox::Key::gen(),
        };
        let fake =
            super::super::envelope::seal(&false_binding, bob.verifying_key().to_bytes()).unwrap();
        let fragment = super::super::recipient_wrap_fragment(
            genid().id,
            secret,
            bob.verifying_key().to_bytes(),
            fake,
        )
        .unwrap();
        store.commit(collection.source(), &alice, fragment).unwrap();
        resource::grant(
            &mut store,
            &alice,
            &before,
            SecretTarget::Secret(secret),
            bob.verifying_key(),
            DeliveryLimits::default(),
            false,
        )
        .unwrap();
        let current = observed(&mut store, collection, &alice);
        assert!(current.open(secret, &bob).is_err());
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &alice,
                &current,
                collection,
                &alice,
                Epoch::from_unix_seconds(10.0),
            )
            .unwrap(),
            1
        );
        let after = observed(&mut store, collection, &alice);
        assert_eq!(after.open(secret, &bob).unwrap(), b"value");
        assert_eq!(
            maintain_recipient_envelopes(
                &mut store,
                &alice,
                &after,
                collection,
                &alice,
                Epoch::from_unix_seconds(10.0),
            )
            .unwrap(),
            0
        );
    }
}
