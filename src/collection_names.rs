//! Canonical root descriptors for faculty collections, which of them a faculty
//! reads, and which one it writes.
//!
//! A root collection used to be anchored by an opaque minted scope id. It
//! discriminated roots correctly and told a reader nothing: the id lived as a
//! hex constant in one faculty's source, so "which collection is this?" was
//! answerable only by someone holding the code. A root is now a self-describing
//! fragment containing its name, representation, and immutable READ and WRITE
//! admission policies. The fragment's content handle is the collection
//! identity.
//!
//! The scope ids have not gone anywhere — they remain each schema's stable
//! identifier and the key this table is read by, because the migration that
//! re-seats existing data has to speak both languages at once.
//!
//! # Reading every collection of a name
//!
//! One name can stand for several collections: this host's own, one somebody
//! shared with it, an older generation. A faculty reads every one of them its
//! key may READ ([`read_union`]), and where it shows a person rows from more
//! than one, it says which collection and owner each came from ([`label`]).
//! The candidates are the collections this pile holds records for and the
//! ones a held proof names the key in ([`available`]). The owners of a
//! collection are the keys its policies are rooted at ([`owners`]).
//!
//! # Writing to one target
//!
//! A write goes to exactly one collection: the target the command names, or
//! else the default ([`write_target`]): the one same-named collection with the
//! writer's key among its policy roots, else the one with the writer's key as
//! its only root, else an error naming the candidates. The candidates are the
//! same-named collections this pile holds records for. A host's private
//! descriptor is registered before anything commits to it and has no record
//! until then, so counting descriptors instead would make every host's empty
//! private descriptor a candidate beside the collection it actually writes.
//! A pile holding no record of the name at all starts the writer's private
//! collection.
//!
//! Nothing here reads the process environment: an environment is a copy taken
//! when the process started, and a stale one used to point long-lived shells
//! at retired generations while every pile said otherwise.

use std::collections::BTreeSet;

use anybytes::View;
use anyhow::{anyhow, bail, Context};
use ed25519_dalek::VerifyingKey;

use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::utf8string::UTF8String;
use triblespace::core::blob::{Blob, TryFromBlob};
use triblespace::core::capability::policy::{admission_policy_root, resource_policy};
use triblespace::core::collection::{
    descriptor, records::CollectionHandle, records::KIND_COLLECTION_DESCRIPTOR, Collection,
    CollectionRead, CollectionRegistrationError, CollectionStoreExt,
};
use triblespace::core::id::Id;
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::inline::Inline;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::{
    BlobStoreGet, BlobStoreList, BlobStorePut, CapabilityProofRead, SnapshotSource, StoreSnapshot,
};
use triblespace::core::trible::TribleSet;
use triblespace::prelude::{exists, find, pattern};

use crate::schemas::{
    atlas, blockdag, body, code, cognition, compass, config, decide, discord, embeddings, files,
    habit, headspace, mail, memory, message, orient, planner, posture, relations, status,
    swarm_health, teams, voice, web, wiki,
};
use crate::secrets::DEFAULT_SCOPE_ID as SECRETS_SCOPE_ID;

/// Every root collection this build writes: the scope that used to anchor it,
/// and the name it is known by.
///
/// A faculty that is missing here cannot be opened at all, which is the point:
/// a nameless collection is one the pile cannot describe, and shipping one
/// silently is how the old scope model stayed opaque for so long. Every
/// collection in this table deliberately uses a direct READ and WRITE policy
/// rooted at the pile's durable signer. Sharing a collection later means
/// creating or migrating to a descriptor whose policy says so; it is not an
/// ambient property hidden in this table.
///
/// The ones worth naming individually:
///
/// - `memory` and `memory-comb` are the journal, first-person and personal.
/// - `compass` and `wiki` are the two JP has floated sharing. Neither becomes
///   public here: his own design for compass is a collection that shares its
///   goals but not the personal notes attached to them, which is *two*
///   collections, not one made public. A public sibling is the shape, with its
///   own explicit admission policy.
pub fn table() -> Vec<(Id, &'static str)> {
    vec![
        (atlas::DEFAULT_SCOPE_ID, "atlas"),
        (blockdag::DEFAULT_SCOPE_ID, "blockdag"),
        (body::DEFAULT_SCOPE_ID, "body"),
        (code::DEFAULT_SCOPE_ID, "code"),
        (cognition::DEFAULT_SCOPE_ID, "cognition"),
        (compass::DEFAULT_SCOPE_ID, "compass"),
        (config::DEFAULT_SCOPE_ID, config::COLLECTION_NAME),
        (decide::DEFAULT_SCOPE_ID, "decide"),
        (discord::DEFAULT_SCOPE_ID, "discord"),
        (embeddings::DEFAULT_SCOPE_ID, "embeddings"),
        (files::DEFAULT_SCOPE_ID, "files"),
        (habit::DEFAULT_SCOPE_ID, "habit"),
        (headspace::DEFAULT_SCOPE_ID, "headspace"),
        (mail::DEFAULT_SCOPE_ID, "mail"),
        (memory::DEFAULT_SCOPE_ID, "memory-journal"),
        (memory::DEFAULT_COMB_SCOPE_ID, "memory-comb"),
        (message::DEFAULT_SCOPE_ID, "message"),
        (orient::DEFAULT_SCOPE_ID, "orient"),
        (planner::DEFAULT_SCOPE_ID, "planner"),
        (posture::DEFAULT_POLICY_SCOPE_ID, "posture-policy"),
        (posture::DEFAULT_SCAN_SCOPE_ID, "posture-scan"),
        (relations::DEFAULT_SCOPE_ID, "relations"),
        (SECRETS_SCOPE_ID, "secrets"),
        (status::DEFAULT_SCOPE_ID, "status"),
        (
            swarm_health::DEFAULT_SCOPE_ID,
            swarm_health::COLLECTION_NAME,
        ),
        (teams::DEFAULT_SCOPE_ID, "teams"),
        (voice::COLLECTION_SCOPE_ID, "voice"),
        (web::DEFAULT_SCOPE_ID, "web"),
        (wiki::DEFAULT_SCOPE_ID, "wiki"),
    ]
}

/// The name for one scope, or `None` if this build does not know it.
pub fn name_for(scope: Id) -> Option<&'static str> {
    table()
        .into_iter()
        .find(|(candidate, _)| *candidate == scope)
        .map(|(_, name)| name)
}

/// The name for one scope, or a panic naming the scope that is missing.
///
/// Every collection this build opens is one it wrote the table entry for, so an
/// absence is a bug in this crate rather than anything a pile can cause. It is
/// loud because the alternative — inventing a name — would root real data at a
/// collection nothing else can find.
pub fn require_name(scope: Id) -> &'static str {
    name_for(scope).unwrap_or_else(|| {
        panic!(
            "no collection name for scope {scope:X}; add it to \
             faculties::collection_names::table"
        )
    })
}

/// The private policy deliberately shared by every current faculty root.
pub use triblespace::core::collection::private_policy;

/// The keys `descriptor`'s capability policies are rooted at, each once, in
/// byte order: the owners of the collection it describes.
pub fn owners(descriptor: &TribleSet) -> Vec<VerifyingKey> {
    let mut owners: Vec<VerifyingKey> = find!(
        root: VerifyingKey,
        pattern!(descriptor, [
            { _?collection @
                metadata::tag: KIND_COLLECTION_DESCRIPTOR,
                resource_policy: _?binding,
            },
            { _?binding @ admission_policy_root: ?root },
        ])
    )
    .collect();
    owners.sort_unstable_by_key(VerifyingKey::to_bytes);
    owners.dedup();
    owners
}

/// A root collection's descriptor facts and name, from bytes this snapshot
/// holds. Anything else -- an absent or undecodable descriptor, a derived
/// collection, a name that is not here -- is simply not a named root, and
/// nothing is fetched to find out.
fn resident_root<S>(snapshot: &S, handle: CollectionHandle) -> Option<(TribleSet, View<str>)>
where
    S: BlobStoreList + BlobStoreGet,
{
    if !snapshot.contains_blob(handle).ok()? {
        return None;
    }
    let facts: TribleSet = snapshot.get(handle).ok()?;
    let name = descriptor::name(&facts).ok()??;
    if !snapshot.contains_blob(name).ok()? {
        return None;
    }
    let name = snapshot.get::<View<str>, UTF8String>(name).ok()?;
    Some((facts, name))
}

/// Every collection a held proof names `key` in, and every collection this
/// pile holds records for that `key` owns: the collections this key can
/// read from or be given to.
///
/// A proof names a key when the key is one of its delegates, so a grant
/// lists its collection for the key it was issued to. The proofs are not
/// validated here; admission decides what they authorize.
pub fn available<S>(snapshot: &S, key: VerifyingKey) -> anyhow::Result<BTreeSet<CollectionHandle>>
where
    S: CollectionRead + CapabilityProofRead + BlobStoreList + BlobStoreGet,
{
    let mut available = granted(snapshot, key)?;
    for handle in snapshot
        .collections()
        .map_err(|error| anyhow!("list collections: {error}"))?
    {
        if resident_root(snapshot, handle).is_some_and(|(facts, _)| owners(&facts).contains(&key)) {
            available.insert(handle);
        }
    }
    Ok(available)
}

/// The resources the held proofs name `key` in.
fn granted<S>(snapshot: &S, key: VerifyingKey) -> anyhow::Result<BTreeSet<CollectionHandle>>
where
    S: CapabilityProofRead,
{
    let mut granted = BTreeSet::new();
    for proof in snapshot
        .proofs()
        .map_err(|error| anyhow!("list held proofs: {error}"))?
    {
        let proof = proof.map_err(|error| anyhow!("read a held proof: {error}"))?;
        if proof.delegated_keys().any(|delegate| delegate == key) {
            granted.insert(CollectionHandle::new(proof.resource().into_bytes()));
        }
    }
    Ok(granted)
}

/// Every collection of `scope`'s name that `key` may READ in `snapshot`, in
/// handle order: the read-union a faculty reads.
///
/// The candidates are the collections the snapshot holds records for and
/// the ones a held proof names `key` in, which together cover every
/// [`available`] one; a candidate counts when it is a root whose
/// descriptor and name are here and carry the faculty's name. Admission
/// reads the snapshot's frozen proofs; through an acquiring reader it may
/// fetch the capability definitions those proofs name, never the
/// descriptors themselves.
pub fn read_union_in<S>(
    snapshot: &S,
    scope: Id,
    key: VerifyingKey,
) -> anyhow::Result<Vec<Collection<SimpleArchive>>>
where
    S: StoreSnapshot + CollectionRead + CapabilityProofRead + BlobStoreList + BlobStoreGet,
{
    let name = require_name(scope);
    let mut candidates = granted(snapshot, key)?;
    candidates.extend(
        snapshot
            .collections()
            .map_err(|error| anyhow!("list collections: {error}"))?,
    );
    let mut readable = Vec::new();
    for handle in candidates {
        if !resident_root(snapshot, handle).is_some_and(|(_, found)| &*found == name) {
            continue;
        }
        let Ok(collection) = Collection::<SimpleArchive>::open(snapshot, handle) else {
            continue;
        };
        if collection
            .reader_is_admitted_acquiring(snapshot, key)
            .with_context(|| format!("check READ admission to {name} {}", label_of(handle)))?
        {
            readable.push(collection);
        }
    }
    Ok(readable)
}

/// [`read_union_in`] over a snapshot of `storage` taken now.
pub fn read_union<S>(
    storage: &mut S,
    scope: Id,
    key: VerifyingKey,
) -> anyhow::Result<Vec<Collection<SimpleArchive>>>
where
    S: SnapshotSource,
    S::Snapshot:
        StoreSnapshot + CollectionRead + CapabilityProofRead + BlobStoreList + BlobStoreGet,
{
    let snapshot = storage.snapshot().with_context(|| {
        format!(
            "freeze the store to find the {} collections",
            require_name(scope)
        )
    })?;
    read_union_in(&snapshot, scope, key)
}

/// [`read_union`] at a synchronous foreground I/O boundary: admission may
/// acquire the capability definitions the frozen proofs name. Call outside
/// `Runtime::block_on`.
pub fn read_union_acquiring<S>(
    storage: &mut S,
    scope: Id,
    key: VerifyingKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
) -> anyhow::Result<Vec<Collection<SimpleArchive>>>
where
    S: SnapshotSource,
    S::Snapshot: StoreSnapshot
        + CollectionRead
        + CapabilityProofRead
        + BlobStoreList
        + triblespace::core::repo::async_store::AsyncBlobStoreGet,
{
    let snapshot = storage.snapshot().with_context(|| {
        format!(
            "freeze the store to find the {} collections",
            require_name(scope)
        )
    })?;
    let reader = crate::storage::AcquiringReader::new(snapshot, runtime.clone());
    read_union_in(&reader, scope, key)
}

/// The collection a write by `key` to `scope` goes to: `target` when the
/// caller names one, else the default.
///
/// The default is the one same-named collection this pile holds records for
/// with `key` among its policy roots, else the one with `key` as its only
/// root, else an error naming every candidate and its owners. A pile with no
/// record of the name starts the writer's private collection, registering
/// it. A named target must be a root carrying the faculty's name; it need
/// not hold anything yet.
///
/// Choosing a target checks no admission. A command applies
/// [`require_command_write_admission`] to the target before it publishes;
/// publication itself stays unconditional, because later evidence may admit
/// an offline commit.
pub fn write_target<S>(
    storage: &mut S,
    scope: Id,
    key: VerifyingKey,
    target: Option<CollectionHandle>,
) -> anyhow::Result<Collection<SimpleArchive>>
where
    S: CollectionStoreExt + SnapshotSource,
    S::Snapshot: CollectionRead + BlobStoreList + BlobStoreGet,
{
    let snapshot = storage.snapshot().with_context(|| {
        format!(
            "freeze the store to choose the {} target",
            require_name(scope)
        )
    })?;
    if let Some(target) = target {
        return open_exact_in(&snapshot, scope, target);
    }
    let chosen = default_target_in(&snapshot, scope, key)?;
    drop(snapshot);
    match chosen {
        Some(collection) => Ok(collection),
        None => open(storage, scope, key).context("register the writer's private descriptor"),
    }
}

/// [`write_target`] at a synchronous foreground I/O boundary: a named target
/// whose descriptor or name is not here is acquired. Call outside
/// `Runtime::block_on`.
pub fn write_target_acquiring<S>(
    storage: &mut S,
    scope: Id,
    key: VerifyingKey,
    target: Option<CollectionHandle>,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
) -> anyhow::Result<Collection<SimpleArchive>>
where
    S: CollectionStoreExt + SnapshotSource,
    S::Snapshot: CollectionRead
        + BlobStoreList
        + BlobStoreGet
        + triblespace::core::repo::async_store::AsyncBlobStoreGet,
{
    let Some(target) = target else {
        return write_target(storage, scope, key, None);
    };
    let snapshot = storage.snapshot().with_context(|| {
        format!(
            "freeze the store to open the {} target",
            require_name(scope)
        )
    })?;
    let reader = crate::storage::AcquiringReader::new(snapshot, runtime.clone());
    open_exact_in(&reader, scope, target)
}

/// Every root collection with `scope`'s name that `snapshot` holds records
/// for, in handle order, whoever may read it: the candidates a default write
/// target is chosen from.
pub fn named_in<S>(snapshot: &S, scope: Id) -> anyhow::Result<Vec<Collection<SimpleArchive>>>
where
    S: CollectionRead + BlobStoreList + BlobStoreGet,
{
    let name = require_name(scope);
    let mut named = Vec::new();
    for handle in snapshot
        .collections()
        .map_err(|error| anyhow!("list collections: {error}"))?
    {
        if !resident_root(snapshot, handle).is_some_and(|(_, found)| &*found == name) {
            continue;
        }
        if let Ok(collection) = Collection::<SimpleArchive>::open(snapshot, handle) {
            named.push(collection);
        }
    }
    Ok(named)
}

/// Parse a `--target` collection handle: 64 hexadecimal digits, optionally
/// prefixed `blake3:`, as every handle here is printed.
pub fn parse_target(raw: &str) -> anyhow::Result<CollectionHandle> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("blake3:").unwrap_or(raw);
    if raw.len() != 64 {
        bail!("a collection handle is 64 hexadecimal digits, optionally prefixed blake3:");
    }
    let mut bytes = [0_u8; 32];
    hex::decode_to_slice(raw, &mut bytes).context("a collection handle is hexadecimal")?;
    Ok(Inline::new(bytes))
}

/// The default write target in `snapshot`, or `None` when it holds no record
/// of a collection with `scope`'s name.
pub(crate) fn default_target_in<S>(
    snapshot: &S,
    scope: Id,
    key: VerifyingKey,
) -> anyhow::Result<Option<Collection<SimpleArchive>>>
where
    S: CollectionRead + BlobStoreList + BlobStoreGet,
{
    let name = require_name(scope);
    let candidates: Vec<(Collection<SimpleArchive>, Vec<VerifyingKey>)> =
        named_in(snapshot, scope)?
            .into_iter()
            .map(|collection| {
                let roots = resident_root(snapshot, collection.handle())
                    .map(|(facts, _)| owners(&facts))
                    .unwrap_or_default();
                (collection, roots)
            })
            .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    let rooted: Vec<Collection<SimpleArchive>> = candidates
        .iter()
        .filter(|(_, owners)| owners.contains(&key))
        .map(|(collection, _)| *collection)
        .collect();
    let only: Vec<Collection<SimpleArchive>> = candidates
        .iter()
        .filter(|(_, owners)| owners.len() == 1 && owners.contains(&key))
        .map(|(collection, _)| *collection)
        .collect();
    match (rooted.as_slice(), only.as_slice()) {
        ([collection], _) | (_, [collection]) => Ok(Some(*collection)),
        _ => bail!(
            "{} {name:?} collections are here and none is the default for key {}: it must be the \
             one rooted at that key, or else the one rooted at that key alone. Pass --target with \
             one of: {}",
            candidates.len(),
            hex::encode_upper(key.to_bytes()),
            candidates
                .iter()
                .map(|(collection, owners)| format!(
                    "blake3:{} (owned by {})",
                    hex::encode(collection.handle().raw),
                    owner_list(owners)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// How a person tells one collection of a name from another: its handle and
/// its owners, shortened. A descriptor that is not here shows its handle
/// alone.
pub fn label<S>(snapshot: &S, collection: CollectionHandle) -> String
where
    S: BlobStoreList + BlobStoreGet,
{
    match resident_root(snapshot, collection) {
        Some((facts, _)) => format!(
            "{} owned by {}",
            label_of(collection),
            owner_list(&owners(&facts))
        ),
        None => label_of(collection),
    }
}

/// Which collections of a read-union hold facts about `entity`, as
/// [`label`]s, when the union has more than one collection to tell apart;
/// `None` when it reads one, or none of them says anything about `entity`.
///
/// `members` pairs each collection of the union with its facts, as the read
/// that shows the row took them.
pub fn provenance<S, P>(
    snapshot: &S,
    members: &[(CollectionHandle, P)],
    entity: Id,
) -> Option<String>
where
    S: BlobStoreList + BlobStoreGet,
    P: TriblePattern,
{
    if members.len() < 2 {
        return None;
    }
    let labels: Vec<String> = members
        .iter()
        .filter(|(_, facts)| {
            exists!(
                (attribute: Id, value: Inline<UnknownInline>),
                pattern!(facts, [{ entity @ ?attribute: ?value }])
            )
        })
        .map(|(collection, _)| label(snapshot, *collection))
        .collect();
    (!labels.is_empty()).then(|| labels.join("; "))
}

fn label_of(collection: CollectionHandle) -> String {
    format!("blake3:{}", hex::encode(&collection.raw[..8]))
}

fn owner_list(owners: &[VerifyingKey]) -> String {
    if owners.is_empty() {
        return "nobody".to_owned();
    }
    owners
        .iter()
        .map(|owner| hex::encode_upper(&owner.to_bytes()[..8]))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Refuse a COMMAND whose record would be published but never admitted.
///
/// Publication itself stays unconditional: a library may publish an offline
/// COMMIT that later evidence activates. What must not stay silent is a
/// command a person typed. An unadmitted COMMIT is appended and then
/// invisible — every read goes through a maintained projection that carries
/// admitted support only — so the command prints an id, exits zero, and
/// changes nothing anyone can observe. That is how a wrong signing key ran
/// for eight hours without a single symptom.
///
/// A command applies this to its write target ([`write_target`]). The
/// default target is a collection rooted at the writer's key, but a root of
/// one policy need not be admitted by the other, and a named target may be
/// somebody else's collection entirely.
///
/// `faculty` names the collection in the message and `reader_hint` names a
/// command whose output would silently not change, because "your write went
/// nowhere" is only actionable if the reader knows where to look.
pub fn require_command_write_admission<S>(
    store: &mut S,
    collection: Collection<SimpleArchive>,
    signer: &ed25519_dalek::SigningKey,
    faculty: &str,
    reader_hint: &str,
) -> anyhow::Result<()>
where
    S: SnapshotSource,
    S::Snapshot: StoreSnapshot + BlobStoreGet + CapabilityProofRead,
{
    let snapshot = store
        .snapshot()
        .map_err(|error| anyhow!("freeze {faculty} publication authority: {error}"))?;
    require_command_write_in(&snapshot, collection, signer, faculty, reader_hint)
}

/// The command WRITE guard with exact-byte acquisition and frozen proof evidence.
/// Like the other acquiring entry points, call only at the synchronous boundary.
pub fn require_command_write_admission_acquiring<S>(
    store: &mut S,
    collection: Collection<SimpleArchive>,
    signer: &ed25519_dalek::SigningKey,
    faculty: &str,
    reader_hint: &str,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
) -> anyhow::Result<()>
where
    S: SnapshotSource,
    S::Snapshot: CapabilityProofRead + triblespace::core::repo::async_store::AsyncBlobStoreGet,
{
    let snapshot = store.snapshot().context("freeze publication authority")?;
    let reader = crate::storage::AcquiringReader::new(snapshot, runtime.clone());
    require_command_write_in(&reader, collection, signer, faculty, reader_hint)
}

fn require_command_write_in<S>(
    snapshot: &S,
    collection: Collection<SimpleArchive>,
    signer: &ed25519_dalek::SigningKey,
    faculty: &str,
    reader_hint: &str,
) -> anyhow::Result<()>
where
    S: StoreSnapshot + BlobStoreGet + CapabilityProofRead,
{
    let admitted = collection
        .writer_is_admitted_acquiring(snapshot, signer.verifying_key())
        .map_err(|error| anyhow!("check {faculty} collection WRITE admission: {error}"))?;
    if !admitted {
        bail!(
            "key {} is not admitted to write the {faculty} collection {}. The record would be \
             appended as a raw ledger entry that never enters an admitted snapshot, so no \
             reader — `{reader_hint}` included — would ever see it. Grant that key WRITE on the \
             collection, or run with an admitted key.",
            hex::encode_upper(signer.verifying_key().to_bytes()),
            hex::encode_upper(collection.handle().raw),
        );
    }
    Ok(())
}

/// Open and validate one exact faculty descriptor in an existing snapshot.
///
/// This is the coherent publication-boundary form used by callers which
/// already froze a pile prefix, and the form a named write target takes. It
/// validates only the descriptor's type and faculty name. Local publication
/// does not require present WRITE admission.
pub fn open_exact_in<S>(
    snapshot: &S,
    scope: Id,
    handle: CollectionHandle,
) -> anyhow::Result<Collection<SimpleArchive>>
where
    S: BlobStoreGet,
{
    let expected = require_name(scope);
    let collection = Collection::open(snapshot, handle)
        .with_context(|| format!("open {expected} collection {}", label_of(handle)))?;
    let blob: Blob<SimpleArchive> = snapshot
        .get(handle)
        .context("read collection descriptor while checking its name")?;
    let facts = TribleSet::try_from_blob(blob)
        .context("decode collection descriptor while checking its name")?;
    let name_handle = descriptor::name(&facts)
        .context("decode collection name")?
        .ok_or_else(|| {
            anyhow!(
                "faculty collection {} is derived and has no root name",
                label_of(handle)
            )
        })?;
    let name: View<str> = snapshot
        .get::<View<str>, UTF8String>(name_handle)
        .context("read collection name")?;
    if &*name != expected {
        bail!(
            "collection {} is named {:?}, not the expected faculty collection {:?}",
            label_of(handle),
            &*name,
            expected,
        );
    }
    Ok(collection)
}

/// Register one faculty root and return its typed descriptor handle.
///
/// Registration is idempotent and owns the descriptor's complete attachment
/// closure. Later publication and snapshots take only the returned handle;
/// the store remains owned by its caller.
pub fn open<S>(
    storage: &mut S,
    scope: Id,
    authority: VerifyingKey,
) -> Result<Collection<SimpleArchive>, CollectionRegistrationError<<S as BlobStorePut>::PutError>>
where
    S: CollectionStoreExt,
{
    storage.collection(require_name(scope), private_policy(authority))
}

#[cfg(test)]
mod tests {
    use super::*;

    type MemorySnapshot = <MemoryRepo as SnapshotSource>::Snapshot;

    /// Exact-byte provider for startup tests. Its proof view never advances
    /// when a requested blob is supplied from the later source observation.
    #[derive(Clone)]
    struct StartupSnapshot {
        frozen: MemorySnapshot,
        source: MemorySnapshot,
        requested: std::sync::Arc<std::sync::Mutex<Vec<[u8; 32]>>>,
    }

    impl StoreSnapshot for StartupSnapshot {}

    impl CapabilityProofRead for StartupSnapshot {
        type ProofsError = <MemorySnapshot as CapabilityProofRead>::ProofsError;
        type ProofIter<'a> = <MemorySnapshot as CapabilityProofRead>::ProofIter<'a>;

        fn proofs(&self) -> Result<Self::ProofIter<'_>, Self::ProofsError> {
            self.frozen.proofs()
        }
    }

    impl CollectionRead for StartupSnapshot {
        type RecordsError = <MemorySnapshot as CollectionRead>::RecordsError;
        type RecordIter<'a> = <MemorySnapshot as CollectionRead>::RecordIter<'a>;

        fn records(&self) -> Result<Self::RecordIter<'_>, Self::RecordsError> {
            self.frozen.records()
        }

        fn collections(&self) -> Result<Vec<CollectionHandle>, Self::RecordsError> {
            self.frozen.collections()
        }
    }

    impl BlobStoreList for StartupSnapshot {
        type Err = <MemorySnapshot as BlobStoreList>::Err;
        type Iter<'a> = <MemorySnapshot as BlobStoreList>::Iter<'a>;

        fn blobs(&self) -> Self::Iter<'_> {
            self.frozen.blobs()
        }
    }

    impl triblespace::core::repo::async_store::AsyncBlobStoreGet for StartupSnapshot {
        type GetError<E: std::error::Error + Send + Sync + 'static> =
            <MemorySnapshot as BlobStoreGet>::GetError<E>;

        fn get<T, E>(
            &self,
            handle: Inline<triblespace::core::inline::encodings::hash::Handle<E>>,
        ) -> impl std::future::Future<Output = Result<T, Self::GetError<T::Error>>> + Send
        where
            E: triblespace::core::blob::BlobEncoding + 'static,
            T: TryFromBlob<E>,
            triblespace::core::inline::encodings::hash::Handle<E>:
                triblespace::core::inline::InlineEncoding,
        {
            let raw = handle.raw;
            async move {
                let handle =
                    Inline::<triblespace::core::inline::encodings::hash::Handle<E>>::new(raw);
                if self.frozen.contains_blob(handle).unwrap() {
                    return self.frozen.get(handle);
                }
                self.requested.lock().unwrap().push(raw);
                self.source.get(handle)
            }
        }
    }

    fn startup_reader(
        source: &mut MemoryRepo,
        missing: Option<[u8; 32]>,
        include_proofs: bool,
    ) -> (
        crate::storage::AcquiringReader<StartupSnapshot>,
        StartupSnapshot,
    ) {
        use anybytes::Bytes;
        use triblespace::core::blob::encodings::UnknownBlob;
        let source = source.snapshot().unwrap();
        let mut local = MemoryRepo::default();
        for info in source.blobs() {
            let handle = info.unwrap().handle;
            if Some(handle.raw) != missing {
                let bytes: Bytes = source.get(handle).unwrap();
                local.put::<UnknownBlob, _>(bytes).unwrap();
            }
        }
        if include_proofs {
            for proof in source.proofs().unwrap() {
                local.insert_proof(proof.unwrap()).unwrap();
            }
        }
        let snapshot = StartupSnapshot {
            frozen: local.snapshot().unwrap(),
            source,
            requested: Default::default(),
        };
        let reader = crate::storage::AcquiringReader::new(
            snapshot.clone(),
            std::sync::Arc::new(crate::storage::runtime().unwrap()),
        );
        (reader, snapshot)
    }

    #[test]
    fn startup_acquires_missing_descriptor_and_name_without_relaxing_validation() {
        let owner = SigningKey::from_bytes(&[0x65; 32]);
        let mut source = MemoryRepo::default();
        let collection = open(&mut source, wiki::DEFAULT_SCOPE_ID, owner.verifying_key()).unwrap();
        let snapshot = source.snapshot().unwrap();
        let descriptor: Blob<SimpleArchive> = snapshot.get(collection.handle()).unwrap();
        let name = descriptor::name(&TribleSet::try_from_blob(descriptor).unwrap())
            .unwrap()
            .unwrap();
        for missing in [collection.handle().raw, name.raw] {
            let (reader, evidence) = startup_reader(&mut source, Some(missing), true);
            assert!(open_exact_in(
                &evidence.frozen,
                wiki::DEFAULT_SCOPE_ID,
                collection.handle()
            )
            .is_err());
            assert_eq!(
                open_exact_in(&reader, wiki::DEFAULT_SCOPE_ID, collection.handle()).unwrap(),
                collection
            );
            let requested = evidence.requested.lock().unwrap();
            assert!(!requested.is_empty());
            assert!(requested.iter().all(|handle| *handle == missing));
            drop(requested);
            assert!(
                !reader
                    .contains_blob(Inline::<
                        triblespace::core::inline::encodings::hash::Handle<
                            triblespace::core::blob::encodings::UnknownBlob,
                        >,
                    >::new(missing))
                    .unwrap(),
                "acquisition must not advance frozen residency"
            );
        }
        let (reader, evidence) = startup_reader(&mut source, None, true);
        assert!(open_exact_in(&reader, relations::DEFAULT_SCOPE_ID, collection.handle()).is_err());
        assert!(evidence.requested.lock().unwrap().is_empty());
    }

    #[test]
    fn startup_union_acquires_admission_definitions_but_never_later_proofs() {
        let owner = SigningKey::from_bytes(&[0x66; 32]);
        let subject = SigningKey::from_bytes(&[0x67; 32]);
        let outsider = SigningKey::from_bytes(&[0x68; 32]);
        let wiki = wiki::DEFAULT_SCOPE_ID;
        let mut source = MemoryRepo::default();
        let collection = open(&mut source, wiki, owner.verifying_key()).unwrap();
        grant_collection_read(
            &mut source,
            collection.handle(),
            &owner,
            subject.verifying_key(),
        )
        .unwrap();
        let definition = triblespace::core::collection::read_capability();
        let (reader, evidence) = startup_reader(&mut source, Some(definition.raw), true);
        assert!(
            read_union_in(&evidence.frozen, wiki, subject.verifying_key())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            read_union_in(&reader, wiki, subject.verifying_key()).unwrap(),
            [collection]
        );
        assert!(evidence.requested.lock().unwrap().contains(&definition.raw));

        let (resident, evidence) = startup_reader(&mut source, None, true);
        assert!(read_union_in(&resident, wiki, outsider.verifying_key())
            .unwrap()
            .is_empty());
        assert!(evidence.requested.lock().unwrap().is_empty());

        let (before_grant, _) = startup_reader(&mut source, Some(definition.raw), false);
        assert!(
            read_union_in(&before_grant, wiki, subject.verifying_key())
                .unwrap()
                .is_empty(),
            "provider's newer proof must not enter frozen admission"
        );
        assert!(before_grant.proofs().unwrap().next().is_none());

        let (_, mut offline) = startup_reader(&mut source, Some(definition.raw), true);
        offline.source = offline.frozen.clone();
        let offline = crate::storage::AcquiringReader::new(
            offline,
            std::sync::Arc::new(crate::storage::runtime().unwrap()),
        );
        assert!(
            read_union_in(&offline, wiki, subject.verifying_key())
                .unwrap()
                .is_empty(),
            "an unavailable definition cannot grant READ"
        );
    }

    fn key(byte: u8) -> SigningKey {
        SigningKey::from_bytes(&[byte; 32])
    }

    /// A policy rooted at every one of `keys`, any one of which suffices.
    fn shared(keys: &[&SigningKey]) -> CollectionPolicy {
        let roots = keys.iter().map(|key| key.verifying_key());
        CollectionPolicy::new(
            AdmissionPolicy::quorum(roots.clone(), 1, None).unwrap(),
            AdmissionPolicy::quorum(roots, 1, None).unwrap(),
        )
    }

    /// Give `collection` a record, which is what makes it a candidate.
    fn commit(store: &mut MemoryRepo, collection: Collection<SimpleArchive>, signer: &SigningKey) {
        store
            .commit(
                collection,
                signer,
                entity! { metadata::description: "content".to_owned() },
            )
            .unwrap();
    }

    fn handles(collections: Vec<Collection<SimpleArchive>>) -> BTreeSet<CollectionHandle> {
        collections
            .into_iter()
            .map(|collection| collection.handle())
            .collect()
    }

    #[test]
    fn available_collections_are_the_granted_and_the_owned() {
        let (me, other) = (key(0x11), key(0x12));
        let mut store = MemoryRepo::default();
        let mine = open(&mut store, wiki::DEFAULT_SCOPE_ID, me.verifying_key()).unwrap();
        commit(&mut store, mine, &me);
        let theirs = open(&mut store, compass::DEFAULT_SCOPE_ID, other.verifying_key()).unwrap();
        commit(&mut store, theirs, &other);
        // Granted, and nothing of it is here yet.
        let granted = open(
            &mut store,
            relations::DEFAULT_SCOPE_ID,
            other.verifying_key(),
        )
        .unwrap();
        grant_collection_read(&mut store, granted.handle(), &other, me.verifying_key()).unwrap();
        // Registered and never written: no record lists it.
        open(&mut store, decide::DEFAULT_SCOPE_ID, me.verifying_key()).unwrap();

        let snapshot = store.snapshot().unwrap();
        assert_eq!(
            available(&snapshot, me.verifying_key()).unwrap(),
            BTreeSet::from([mine.handle(), granted.handle()])
        );
        assert_eq!(
            available(&snapshot, other.verifying_key()).unwrap(),
            BTreeSet::from([theirs.handle()])
        );
    }

    #[test]
    fn the_read_union_is_every_collection_of_the_name_the_key_may_read() {
        let (me, other, stranger) = (key(0x21), key(0x22), key(0x23));
        let wiki = wiki::DEFAULT_SCOPE_ID;
        let mut store = MemoryRepo::default();
        let mine = open(&mut store, wiki, me.verifying_key()).unwrap();
        commit(&mut store, mine, &me);
        let theirs = open(&mut store, wiki, other.verifying_key()).unwrap();
        commit(&mut store, theirs, &other);
        let elsewhere = open(&mut store, relations::DEFAULT_SCOPE_ID, me.verifying_key()).unwrap();
        commit(&mut store, elsewhere, &me);

        assert_eq!(
            handles(read_union(&mut store, wiki, me.verifying_key()).unwrap()),
            BTreeSet::from([mine.handle()])
        );
        grant_collection_read(&mut store, theirs.handle(), &other, me.verifying_key()).unwrap();
        assert_eq!(
            handles(read_union(&mut store, wiki, me.verifying_key()).unwrap()),
            BTreeSet::from([mine.handle(), theirs.handle()])
        );
        assert!(read_union(&mut store, wiki, stranger.verifying_key())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_pile_without_the_name_starts_the_writers_private_collection() {
        let me = key(0x31);
        let decide = decide::DEFAULT_SCOPE_ID;
        let mut store = MemoryRepo::default();
        // Another name's collection is no candidate.
        let elsewhere = open(&mut store, relations::DEFAULT_SCOPE_ID, me.verifying_key()).unwrap();
        commit(&mut store, elsewhere, &me);
        assert_eq!(
            write_target(&mut store, decide, me.verifying_key(), None).unwrap(),
            open(&mut store, decide, me.verifying_key()).unwrap()
        );
    }

    #[test]
    fn the_default_is_the_one_collection_rooted_at_the_writer() {
        let (me, other) = (key(0x41), key(0x42));
        let decide = decide::DEFAULT_SCOPE_ID;
        let mut store = MemoryRepo::default();
        let together = store.collection("decide", shared(&[&me, &other])).unwrap();
        commit(&mut store, together, &other);
        let theirs = open(&mut store, decide, other.verifying_key()).unwrap();
        commit(&mut store, theirs, &other);
        // The writer's private descriptor, registered and never written.
        open(&mut store, decide, me.verifying_key()).unwrap();
        assert_eq!(
            write_target(&mut store, decide, me.verifying_key(), None).unwrap(),
            together
        );
    }

    #[test]
    fn else_the_default_is_the_one_rooted_at_the_writer_alone() {
        let (me, other) = (key(0x51), key(0x52));
        let decide = decide::DEFAULT_SCOPE_ID;
        let mut store = MemoryRepo::default();
        let together = store.collection("decide", shared(&[&me, &other])).unwrap();
        commit(&mut store, together, &other);
        let mine = open(&mut store, decide, me.verifying_key()).unwrap();
        commit(&mut store, mine, &me);
        assert_eq!(
            write_target(&mut store, decide, me.verifying_key(), None).unwrap(),
            mine
        );
    }

    #[test]
    fn else_there_is_no_default_and_the_error_names_every_candidate() {
        let (me, one, two) = (key(0x61), key(0x62), key(0x63));
        let decide = decide::DEFAULT_SCOPE_ID;
        let mut store = MemoryRepo::default();
        let first = store.collection("decide", shared(&[&me, &one])).unwrap();
        commit(&mut store, first, &one);
        let second = store.collection("decide", shared(&[&me, &two])).unwrap();
        commit(&mut store, second, &two);
        let text = format!(
            "{:#}",
            write_target(&mut store, decide, me.verifying_key(), None).unwrap_err()
        );
        for candidate in [first, second] {
            assert!(
                text.contains(&hex::encode(candidate.handle().raw)),
                "{text}"
            );
        }
        assert!(text.contains("--target"), "{text}");
        // A key rooted in exactly one of them has its default.
        assert_eq!(
            write_target(&mut store, decide, one.verifying_key(), None).unwrap(),
            first
        );
        let stranger = key(0x64);
        let text = format!(
            "{:#}",
            write_target(&mut store, decide, stranger.verifying_key(), None).unwrap_err()
        );
        assert!(text.contains(&hex::encode(first.handle().raw)), "{text}");
    }

    #[test]
    fn a_named_target_is_used_as_named_when_it_carries_the_name() {
        let (me, other) = (key(0x71), key(0x72));
        let decide = decide::DEFAULT_SCOPE_ID;
        let mut store = MemoryRepo::default();
        let theirs = open(&mut store, decide, other.verifying_key()).unwrap();
        assert_eq!(
            write_target(
                &mut store,
                decide,
                me.verifying_key(),
                Some(theirs.handle())
            )
            .unwrap(),
            theirs
        );
        let misnamed = open(&mut store, relations::DEFAULT_SCOPE_ID, me.verifying_key()).unwrap();
        let text = format!(
            "{:#}",
            write_target(
                &mut store,
                decide,
                me.verifying_key(),
                Some(misnamed.handle())
            )
            .unwrap_err()
        );
        assert!(
            text.contains("not the expected faculty collection"),
            "{text}"
        );
    }

    #[test]
    fn a_target_without_write_is_refused_by_the_command_guard() {
        let (me, other) = (key(0x81), key(0x82));
        let decide = decide::DEFAULT_SCOPE_ID;
        let mut store = MemoryRepo::default();
        let theirs = open(&mut store, decide, other.verifying_key()).unwrap();
        let target = write_target(
            &mut store,
            decide,
            me.verifying_key(),
            Some(theirs.handle()),
        )
        .unwrap();
        let text = format!(
            "{:#}",
            require_command_write_admission(&mut store, target, &me, "Decide", "decide show")
                .unwrap_err()
        );
        assert!(text.contains("not admitted to write"), "{text}");
        grant_collection_write(&mut store, theirs.handle(), &other, me.verifying_key()).unwrap();
        require_command_write_admission(&mut store, target, &me, "Decide", "decide show").unwrap();
    }

    #[test]
    fn a_label_names_the_collection_and_its_owners() {
        let (me, other) = (key(0x91), key(0x92));
        let mut store = MemoryRepo::default();
        let together = store.collection("wiki", shared(&[&me, &other])).unwrap();
        let snapshot = store.snapshot().unwrap();
        let text = label(&snapshot, together.handle());
        assert!(
            text.contains(&hex::encode(&together.handle().raw[..8])),
            "{text}"
        );
        for owner in [&me, &other] {
            assert!(
                text.contains(&hex::encode_upper(&owner.verifying_key().to_bytes()[..8])),
                "{text}"
            );
        }
    }

    /// The guard refuses an unadmitted writer and says enough to fix it.
    ///
    /// Also pins the half that must NOT change: publication itself stays
    /// unconditional, so the same key can still append through the library
    /// path. The guard is on the command, not on the commit.
    #[test]
    fn the_command_guard_refuses_an_unadmitted_writer_and_names_the_remedy() {
        let mut store = MemoryRepo::default();
        let owner = SigningKey::from_bytes(&[61; 32]);
        let outsider = SigningKey::from_bytes(&[62; 32]);
        let collection = open(&mut store, decide::DEFAULT_SCOPE_ID, owner.verifying_key())
            .expect("register the signer-private descriptor");

        require_command_write_admission(&mut store, collection, &owner, "Decide", "decide show")
            .expect("the descriptor's own authority is admitted");

        let error = require_command_write_admission(
            &mut store,
            collection,
            &outsider,
            "Decide",
            "decide show",
        )
        .expect_err("an unadmitted writer must not get a silent success");
        let message = format!("{error:#}");
        assert!(message.contains("not admitted to write"), "{message}");
        assert!(
            message.contains(&hex::encode_upper(outsider.verifying_key().to_bytes())),
            "the refusal names the key to grant: {message}"
        );
        assert!(
            message.contains(&hex::encode_upper(collection.handle().raw)),
            "the refusal names the collection to grant it on: {message}"
        );
        assert!(
            message.contains("decide show"),
            "the refusal names a reader that would silently not change: {message}"
        );

        // The library path is deliberately untouched.
        let fragment = entity! { metadata::description: "raw outsider publication".to_owned() };
        store
            .commit(collection, &outsider, fragment)
            .expect("publication stays unconditional");
    }

    use ed25519_dalek::SigningKey;
    use triblespace::core::collection::{
        grant_collection_read, grant_collection_write, AdmissionPolicy, CollectionPolicy,
    };
    use triblespace::core::inline::Inline;
    use triblespace::core::repo::memoryrepo::MemoryRepo;
    use triblespace::core::repo::CapabilityProofStore;
    use triblespace::macros::entity;

    #[test]
    fn every_name_is_nonempty_and_no_two_scopes_share_one() {
        let mut names = BTreeSet::new();
        let mut scopes = BTreeSet::new();
        for (scope, name) in table() {
            assert!(!name.is_empty());
            assert!(names.insert(name), "two scopes both claim the name {name}");
            assert!(scopes.insert(scope), "scope {scope:X} appears twice");
        }
    }

    #[test]
    fn a_scope_with_no_name_is_loud_rather_than_invented() {
        assert!(name_for(Id::new([0x5a; 16]).unwrap()).is_none());
    }

    #[test]
    fn root_policy_is_identity_and_snapshot_admission() {
        let local = SigningKey::from_bytes(&[0x31; 32]);
        let foreign = SigningKey::from_bytes(&[0x73; 32]);
        let scope = wiki::DEFAULT_SCOPE_ID;
        let evidence = entity! { _ @ metadata::tag: &scope };
        let expected = evidence.facts().clone();
        let mut store = MemoryRepo::default();
        let collection = open(&mut store, scope, local.verifying_key()).unwrap();
        store
            .commit(collection, &foreign, evidence.clone())
            .unwrap();
        let store_snapshot = store.snapshot().unwrap();
        let facts = collection.read::<TribleSet, _>(&store_snapshot).unwrap();
        assert!(facts.is_empty());

        store.commit(collection, &local, evidence).unwrap();
        let store_snapshot = store.snapshot().unwrap();
        let facts = collection.read::<TribleSet, _>(&store_snapshot).unwrap();
        assert!(expected.difference(&facts).is_empty());
    }

    #[test]
    fn exact_publication_open_requires_the_expected_name_not_current_write_admission() {
        let operator = SigningKey::from_bytes(&[0x41; 32]);
        let tenant = SigningKey::from_bytes(&[0x52; 32]);
        let mut store = MemoryRepo::default();
        let shared = store
            .collection("wiki", private_policy(operator.verifying_key()))
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        let opened = open_exact_in(&snapshot, wiki::DEFAULT_SCOPE_ID, shared.handle()).unwrap();
        assert_eq!(opened, shared);
        let private = open(&mut store, wiki::DEFAULT_SCOPE_ID, tenant.verifying_key()).unwrap();

        assert_ne!(opened, private);

        let wrong_name = store
            .collection("relations", private_policy(tenant.verifying_key()))
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        let error =
            open_exact_in(&snapshot, wiki::DEFAULT_SCOPE_ID, wrong_name.handle()).unwrap_err();
        assert!(error
            .to_string()
            .contains("not the expected faculty collection"));
    }
}
