//! Durable signing identity and native-collection plumbing shared by every faculty.
//!
//! Three concerns that every faculty needs before it can read or write
//! anything, and that none of them should re-implement:
//!
//! - **Signing identity.** [`signer_path`], [`load_signer`], and [`initialize_signer`]
//!   resolve one durable signing key per pile. Ordinary commands load; only an
//!   explicit initialization mints. No faculty falls back to an ephemeral
//!   identity.
//! - **Opening.** A faculty opens a pile as a host exactly when what it does
//!   depends on which MERGEs and MAPs the fold believes: it publishes them (a
//!   carry, or attaching after a write) or reads through them (an attached
//!   read). It then opens as the key it signs with, whose MERGEs and MAPs the
//!   fold believes and no other key's: [`Storage`] does so for every
//!   operation, and [`open_pile_signed`] loads the signer and opens as it in
//!   one step, so the host is the signing key by construction.
//!   [`open_store_as`] and [`open_pile_strict_as`] take the host explicitly.
//!   [`open_store`] and [`open_pile_strict`] open with no host and believe no
//!   MERGE or MAP: right for a reader that holds no key (it reads every
//!   foundation from its bytes), and for a record writer that neither
//!   publishes nor reads merges or attachments, such as the
//!   `faculties-migrations` crate. [`open_store`] and [`open_store_as`]
//!   supply lazy exact-handle acquisition; the `open_pile_*` functions are the
//!   local-only boundary. All of them report a malformed suffix as evidence
//!   through [`pile_read_error`] rather than silently truncating it.
//! - **Publication and discovery.** [`publish_fragment`] / [`publish_fragments`]
//!   commit whole fragments into one scoped collection; [`discover_target`]
//!   reports what a scope already holds.
//! - **Attached upkeep.** Every faculty source is read through the Succinct
//!   and Rank9 pair attached to it ([`fact_pair`]): indexes of the source's
//!   own nodes that this host builds for itself, signed with its own key and
//!   believed by nobody else. A write calls [`ensure_downstream`] after its
//!   commit so the source's current frontier, the commit included, has its
//!   attachments; the maintenance daemon, or [`carry_facts`] standing in for
//!   it, calls [`maintain_downstream`], which carries the source first and
//!   attaches what the carry leaves. A read ([`FactRead::read_facts`]) takes the
//!   attached cover and reads the foundations no attachment reaches yet from
//!   their own bytes, so nothing the source holds here is missing from it;
//!   [`FactLag`] counts those foundations. Derived collections -- the Files
//!   semantic index -- still derive their leaves, found from the same
//!   listing, and carry their own leaves into the host's merges.
//!
//! This module was carved out of the storage cutover, which is where these
//! primitives were first written. The cutover itself now lives in the separate
//! `faculties-migrations` crate and depends on this module rather than the
//! other way round.

use std::collections::BTreeSet;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};

use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    OrderedUniverse, Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob, UnionArchive,
};
use triblespace::core::blob::encodings::UnknownBlob;
use triblespace::core::collection::{
    ensure_downstream as core_ensure_downstream, maintain_downstream as core_maintain_downstream,
    realize_attached_as, succinctarchive_union, Attached, AttachedRead, AttachedSnapshot,
    Collection, CollectionAttachment, CollectionCommit, CollectionData, CollectionDerive,
    CollectionEncoding, CollectionHandle, CollectionMerge, CollectionRead,
    CollectionRealizationError, CollectionRecord, CollectionRecordSelector, CollectionSnapshotExt,
    CollectionStoreExt, CoreRealizer, Derived, RealizeDerived, Realized, SourceLocator, Support,
    Upkeep, UpkeepReport,
};
use triblespace::core::id::Id;
use triblespace::core::inline::encodings::hash::Handle;
use triblespace::core::inline::InlineEncoding;
use triblespace::core::metadata::MetaDescribe;
use triblespace::core::repo::async_store::AsyncBlobStoreAcquire;
use triblespace::core::repo::pile::{Pile, ReadError};
use triblespace::core::repo::{
    BlobStoreGet, BlobStoreList, CapabilityProofRead, MissingBlob, SnapshotSource, StorageClose,
    Store, StoreRead,
};
use triblespace::core::signing_key_file;
use triblespace::core::trible::{Fragment, TribleSet};
use triblespace_search::portable_bm25::PortableBM25Blob;

/// The shard-preserving logical view used for ordinary Faculty fact queries.
pub type FactArchive = UnionArchive<OrderedUniverse>;

/// A live faculty store. Its snapshots freeze collection records, proofs, and
/// residency observations while permitting shared async exact-blob reads.
/// The network host starts only when an explicitly requested blob is missing.
/// Local writes remain available; foreground operations never serve the pile's
/// resident inventory or activate collection replication.
pub type FacultyStore = triblespace_net::peer::Leech<Pile>;

/// The live store's frozen observation with an async exact-blob reader.
pub type FacultySnapshot = <FacultyStore as SnapshotSource>::Snapshot;

/// Explicit storage ownership for native faculty operations.
///
/// A one-shot caller uses [`Self::new`]: each operation opens and closes its
/// pile, reporting close errors before returning. A long-lived application
/// uses [`Self::shared`] and passes clones to its faculties. Those clones
/// share one lazy leech, pile indexes and I/O runtime, not collection views or
/// snapshots. Construction and tool discovery perform no I/O in either case.
///
/// A shared owner must call [`Self::close`] (or [`Self::finish`]) at shutdown
/// to report persistence errors. Like any long-lived store, its successful
/// appends are not implicitly flushed after each operation. The last owner's
/// drop is a cleanup fallback, not a substitute for checking close errors.
#[derive(Clone)]
pub struct Storage {
    pile: PathBuf,
    key: Option<PathBuf>,
    shared: Option<Arc<Mutex<Option<Session>>>>,
}

struct Session {
    store: Option<FacultyStore>,
    runtime: Arc<tokio::runtime::Runtime>,
    /// The key the store was opened as. Constant for the session's life: the
    /// fold's host cannot change under an open store.
    host: VerifyingKey,
}

impl Session {
    fn close(mut self) -> Result<()> {
        self.store
            .take()
            .expect("an open session owns its store")
            .close()
            .context("close shared faculty store")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            if let Err(error) = store.close() {
                eprintln!("closing shared faculty store failed: {error}");
            }
        }
    }
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage")
            .field("pile", &self.pile)
            .field("key", &self.key)
            .field("shared", &self.shared.is_some())
            .finish()
    }
}

impl Storage {
    /// Configure operation-scoped storage, suitable for a short-lived CLI.
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self {
            pile,
            key,
            shared: None,
        }
    }

    /// Configure one application-owned store. Clones share this owner even
    /// when passed to different faculty adapters or MCP protocol sessions.
    /// Independently constructed owners never share implicitly by path.
    pub fn shared(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self {
            pile,
            key,
            shared: Some(Arc::new(Mutex::new(None))),
        }
    }

    pub fn path(&self) -> &Path {
        &self.pile
    }

    pub fn key_path(&self) -> Option<&Path> {
        self.key.as_deref()
    }

    /// Retain one store across a compound operation without holding a borrow
    /// over parsing, external I/O or output. A CLI creates and closes a local
    /// owner for this scope; an already-shared application owner is reused and
    /// remains open afterward. Nested scopes therefore preserve ownership.
    pub fn scope<T>(&self, operation: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        if self.shared.is_some() {
            return operation(self);
        }
        let storage = Self::shared(self.pile.clone(), self.key.clone());
        let result = operation(&storage);
        storage.finish(result)
    }

    /// Enter the shared session, opening its store as `host` the first time.
    ///
    /// Every entry loads the signer again from the same paths, so the host is
    /// one key for the session's life unless the key file itself is replaced
    /// under a running process. That is refused rather than served: the
    /// store's fold would keep believing the old key's merges while every
    /// carry signed with the new one failed, reads quietly wide and writes
    /// loudly broken. Restarting opens the store as the new key.
    fn with_session<T>(
        &self,
        host: VerifyingKey,
        operation: impl FnOnce(&mut Session) -> Result<T>,
    ) -> Result<T> {
        let mut session = self
            .shared
            .as_ref()
            .expect("shared storage owner")
            .lock()
            .map_err(|_| anyhow!("shared faculty store is poisoned"))?;
        if session.is_none() {
            let runtime = Arc::new(runtime()?);
            let store = open_store_as(&self.pile, host)?;
            *session = Some(Session {
                store: Some(store),
                runtime,
                host,
            });
        }
        let opened_as = session.as_ref().expect("initialized above").host;
        if opened_as != host {
            anyhow::bail!(
                "the durable signing key changed while the shared faculty store was open \
                 (opened as {}, now {}); restart the process to open it as the new key",
                hex::encode(opened_as.as_bytes()),
                hex::encode(host.as_bytes())
            );
        }
        // MCP reports a handler panic and keeps serving. Drop the ownership
        // guard normally before resuming that panic so a failed handler does
        // not poison the connection for every later tool call.
        let result = catch_unwind(AssertUnwindSafe(|| {
            operation(session.as_mut().expect("initialized above"))
        }));
        drop(session);
        match result {
            Ok(result) => result,
            Err(panic) => resume_unwind(panic),
        }
    }

    /// Borrow the live lazy-fetch store and its runtime for one operation.
    /// Take fresh snapshots at the point of use; never retain a selected view
    /// here. Do not recursively enter this owner from the callback.
    pub fn with_store<T>(
        &self,
        operation: impl FnOnce(
            &mut FacultyStore,
            &SigningKey,
            &Arc<tokio::runtime::Runtime>,
        ) -> Result<T>,
    ) -> Result<T> {
        let signer = load_signer(&self.pile, self.key.as_deref())?;
        if self.shared.is_some() {
            return self.with_session(signer.verifying_key(), |session| {
                operation(
                    session.store.as_mut().expect("open store"),
                    &signer,
                    &session.runtime,
                )
            });
        }
        let runtime = Arc::new(runtime()?);
        let mut store = open_store_as(&self.pile, signer.verifying_key())?;
        let result = operation(&mut store, &signer, &runtime);
        finish_close(result, store.close().context("close faculty store"))
    }

    /// Borrow the same local backend for operations which use resident-only
    /// Pile/PileSnapshot APIs. This does not start the peer or silently add
    /// network acquisition to such operations. Do not re-enter the peer while
    /// its local-backend guard is held.
    pub fn with_pile<T>(
        &self,
        operation: impl FnOnce(&mut Pile, &SigningKey) -> Result<T>,
    ) -> Result<T> {
        let signer = load_signer(&self.pile, self.key.as_deref())?;
        if self.shared.is_some() {
            return self.with_session(signer.verifying_key(), |session| {
                let mut pile = session.store.as_ref().expect("open store").store();
                pile.refresh()
                    .map_err(|error| pile_read_error(&self.pile, error))?;
                let result = catch_unwind(AssertUnwindSafe(|| operation(&mut pile, &signer)));
                drop(pile);
                match result {
                    Ok(result) => result,
                    Err(panic) => resume_unwind(panic),
                }
            });
        }
        let mut pile = open_pile_strict_as(&self.pile, signer.verifying_key())?;
        let result = operation(&mut pile, &signer);
        finish_pile(pile, result)
    }

    /// Close an initialized shared store once, leaving unopened configuration
    /// untouched. One-shot operations already report their own close errors.
    pub fn close(&self) -> Result<()> {
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        // A failed/panicking handler must not prevent shutdown from attempting
        // to close the owned file and report a persistence error.
        let session = shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        match session {
            Some(session) => session.close(),
            None => Ok(()),
        }
    }

    /// Preserve both a transport/operation failure and any shutdown failure.
    pub fn finish<T>(&self, result: Result<T>) -> Result<T> {
        finish_close(result, self.close())
    }
}

fn finish_close<T>(result: Result<T>, close: Result<()>) -> Result<T> {
    match (result, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(error), Err(close_error)) => {
            Err(error.context(format!("closing storage also failed: {close_error:#}")))
        }
    }
}

/// Enter the async I/O boundary of a foreground command.
pub fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create faculty I/O runtime")
}

/// Run a pure read, acquiring only the exact blobs that it asks for.
///
/// Capture the command's already-frozen facts/support in `read`. The argument
/// is its blob reader: acquisition may replace this reader with a later
/// resident snapshot. It must not be used to choose a newer collection frontier
/// or application evaluation time. Report output and publish facts
/// only after this function succeeds, since the read may run more than once.
pub async fn read<S, T>(
    store: &mut S,
    snapshot: &S::Snapshot,
    mut read: impl FnMut(&S::Snapshot) -> Result<T>,
) -> Result<T>
where
    S: SnapshotSource + AsyncBlobStoreAcquire,
    S::Snapshot: BlobStoreList,
{
    let mut reader = snapshot.clone();
    loop {
        let error = match read(&reader) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(missing) = error
            .chain()
            .find_map(|error| error.downcast_ref::<MissingBlob>())
        else {
            return Err(error);
        };
        // A closure still consulting an older snapshot must fail, not acquire
        // the same already-resident bytes forever. Only an actual miss from
        // the supplied blob reader can advance this operation.
        if reader.contains_blob(missing.handle)? {
            return Err(error);
        }
        if store.acquire(missing.handle).await?.is_none() {
            return Err(error);
        }
        reader = store.snapshot()?;
    }
}

/// Open a pile with lazy, exact-handle network acquisition, with no host:
/// the fold believes no MERGE, so every believed foundation is its own
/// frontier node. Right for a reader that holds no key; anything that
/// publishes or reads through merges opens with [`open_store_as`].
///
/// `TRIBLESPACE_PEERS` supplies comma-separated bootstrap endpoint tickets or
/// endpoint ids, not blob providers to probe in order. The DHT finds providers.
/// This foreground client joins no collection gossip topics and announces no
/// providers. Its ephemeral transport identity is deliberately separate from
/// both the durable author and any already-running replication daemon.
pub fn open_store(path: &Path) -> Result<FacultyStore> {
    lazy_store(|| open_pile_strict(path))
}

/// [`open_store`] as `host`, the durable key the caller signs with: the fold
/// believes the MERGEs `host` signed and no other key's, so the caller's own
/// carries are believed and its reads attach its own merges.
pub fn open_store_as(path: &Path, host: VerifyingKey) -> Result<FacultyStore> {
    lazy_store(|| open_pile_strict_as(path, host))
}

/// Wrap the pile `open` returns in the foreground leech. The pile is opened
/// only once the peer configuration has parsed, so a bad route leaves it
/// untouched.
fn lazy_store(open: impl FnOnce() -> Result<Pile>) -> Result<FacultyStore> {
    use iroh_base::{EndpointAddr, EndpointId};
    use iroh_tickets::endpoint::EndpointTicket;
    use rand_core::RngCore;
    use triblespace_net::peer::{PeerConfig, ReconcileDirection, ReconcileQos};

    let routes = std::env::var("TRIBLESPACE_PEERS").or_else(|error| match error {
        std::env::VarError::NotPresent => Ok(String::new()),
        error => Err(error),
    })?;
    let peers = routes
        .split(',')
        .map(str::trim)
        .filter(|route| !route.is_empty())
        .map(|route| {
            if let Ok(ticket) = route.parse::<EndpointTicket>() {
                return Ok(EndpointAddr::from(ticket));
            }
            route
                .parse::<EndpointId>()
                .map(EndpointAddr::from)
                .with_context(|| format!("invalid TRIBLESPACE_PEERS endpoint {route:?}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut secret = [0; 32];
    rand_core::OsRng
        .try_fill_bytes(&mut secret)
        .context("generate foreground transport identity")?;
    let key = SigningKey::from_bytes(&secret);
    use zeroize::Zeroize;
    secret.zeroize();
    Ok(FacultyStore::lazy(
        open()?,
        key,
        PeerConfig {
            peers,
            qos: ReconcileQos {
                direction: ReconcileDirection::ReadOnly,
            },
            provider_publication_budget: Some(0),
            bind: None,
        },
    ))
}

/// Open the explicitly configured Secrets policy boundary for publication.
///
/// `TRIBLESPACE_COLLECTION_SECRETS` selects an exact shared source descriptor;
/// otherwise a signer-private `secrets` descriptor is registered with an
/// explicit, separate key-delivery policy under that owner.
/// This selects the exact descriptor but deliberately performs no admission
/// check: local publication is unconditional, and WRITE admission is applied
/// when collection snapshots admit commits.
pub fn open_secrets_collection<S>(
    store: &mut S,
    subject: VerifyingKey,
) -> Result<crate::secrets::storage::SecretsCollection>
where
    S: CollectionStoreExt + SnapshotSource,
    S::Snapshot: BlobStoreGet,
{
    let scope = crate::secrets::DEFAULT_SCOPE_ID;
    let Some(handle) = crate::collection_names::configured_handle(scope)? else {
        return crate::secrets::storage::SecretsCollection::register(
            store,
            crate::collection_names::require_name(scope),
            crate::collection_names::private_policy(subject).with_capability(
                crate::secrets::key_delivery_definition(),
                triblespace::core::collection::AdmissionPolicy::direct(subject),
            ),
        )
        .context("register signer-private Secrets descriptor with key-delivery policy");
    };
    let snapshot = store
        .snapshot()
        .context("freeze configured Secrets descriptor")?;
    let source = crate::collection_names::open_exact_in(&snapshot, scope, handle)
        .context("open configured Secrets source collection")?;
    drop(snapshot);
    crate::secrets::storage::SecretsCollection::from_source(store, source)
        .context("register maintained Secrets collection descriptors")
}

/// Open the explicitly configured Secrets policy boundary for local reads.
///
/// Collection READ controls encrypted-evidence replication, not opening an
/// already-delivered local wrap. This retains exact descriptor/type/name
/// selection and ordinary signed WRITE admission of the facts, but adds no
/// READ or key-delivery expiry check to decryption by possession. An unset
/// override registers an explicit signer-private key-delivery policy.
pub fn open_secrets_collection_read<S>(
    store: &mut S,
    subject: VerifyingKey,
) -> Result<crate::secrets::storage::SecretsCollection>
where
    S: CollectionStoreExt + SnapshotSource,
    S::Snapshot: BlobStoreGet,
{
    open_secrets_collection(store, subject)
}

/// Canonical records currently known for one scoped target collection.
///
/// Discovery classifies trusted local records without repeating signature
/// verification. It does not assign WRITE authority: collection observation
/// decides which writers' signed assertions and equations are usable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetDiscovery {
    commits: Vec<CollectionCommit>,
    merges: Vec<CollectionMerge>,
    derives: Vec<CollectionDerive>,
}

impl TargetDiscovery {
    /// Retained signed commits targeting this collection, in deterministic
    /// store order.
    pub fn commits(&self) -> &[CollectionCommit] {
        &self.commits
    }

    /// Retained signed merge claims, in deterministic store order.
    /// WRITE admission has not been evaluated here.
    pub fn merges(&self) -> &[CollectionMerge] {
        &self.merges
    }

    /// Retained signed derive claims whose target is this collection,
    /// in deterministic store order. WRITE admission is not evaluated here.
    pub fn derives(&self) -> &[CollectionDerive] {
        &self.derives
    }
}

/// Discover one target directly through the native collection-record store.
///
/// `scope` resolves the faculty's canonical name. Without an exact override,
/// `authority` seeds the descriptor's direct READ and WRITE policies; with an
/// override, the selected descriptor keeps its own immutable policies. The
/// returned handle selects records. No definition registry, blob scan, or
/// legacy pin lookup participates in target discovery.
pub fn discover_target<S>(
    store: &mut S,
    scope: Id,
    authority: VerifyingKey,
) -> Result<TargetDiscovery>
where
    S: CollectionStoreExt + SnapshotSource,
    <S as SnapshotSource>::Snapshot: BlobStoreGet + CapabilityProofRead + CollectionRead,
{
    let collection = crate::collection_names::open_configured(store, scope, authority)
        .context("open target collection descriptor")?;
    let snapshot = store
        .snapshot()
        .context("freeze target collection store snapshot")?;
    let selectors = std::collections::BTreeSet::from([CollectionRecordSelector::Collection(
        collection.handle(),
    )]);
    let records = snapshot
        .select_records(&selectors)
        .context("discover native collection records")?;
    let mut commits = Vec::new();
    let mut merges = Vec::new();
    let mut derives = Vec::new();
    for record in records {
        match record {
            CollectionRecord::Commit(commit) => commits.push(commit),
            CollectionRecord::Merge(merge) => merges.push(merge),
            CollectionRecord::Derive(derive) => derives.push(derive),
            // A root collection holds no MAP of its own.
            CollectionRecord::Map(_) => {}
        }
    }

    Ok(TargetDiscovery {
        commits,
        merges,
        derives,
    })
}

/// Read the realized SimpleArchive collection through one coherent store
/// snapshot, returning the support certified by that actual target cover.
pub fn read_fact_collection<S>(
    collection: Collection<SimpleArchive>,
    snapshot: &S,
) -> Result<(TribleSet, Support)>
where
    S: StoreRead,
{
    let observed = snapshot
        .collection(collection)
        .context("attach realized collection")?;
    let support = observed
        .support()
        .context("resolve realized fact collection support")?
        .clone();
    let facts = observed
        .view::<TribleSet>()
        .context("read authorized collection facts")?;
    Ok((facts, support))
}

/// How many foundations `source` stands for in `snapshot` that `view` has no
/// usable leaf for: a view's freshness against its immediate source, as a
/// count.
///
/// Every admitted foundation counts, whether or not its payload is here. A
/// commit whose payload this reader never received, because it holds no
/// source READ or the payload is simply elsewhere, is still missing from the
/// view until its writer derives it, and a reader that could not count it
/// would call a view current that is missing it. A leaf counts only when its
/// output is here: one whose record arrived before its bytes answers for
/// nothing yet, which is also how maintenance schedules it. Each foundation
/// is looked up by its locator in the view's own leaves; two collections'
/// supports are never compared.
pub fn underived<R, S, T>(snapshot: &R, source: Collection<S>, view: Collection<T>) -> Result<usize>
where
    R: StoreRead,
    S: CollectionEncoding,
    T: CollectionEncoding,
{
    let foundations = source
        .admitted(snapshot)
        .context("read the foundations a view's source stands for")?;
    let coverage = snapshot
        .coverage(&BTreeSet::from([view.handle()]))
        .context("read a view's leaves")?;
    let mut underived = 0;
    for foundation in foundations.members() {
        let mut usable = false;
        for output in coverage.leaf_outputs(view.handle(), SourceLocator::of(foundation.raw)) {
            if snapshot
                .metadata(Handle::<UnknownBlob>::from_hash(output))
                .context("inspect a view leaf's output")?
                .is_some()
            {
                usable = true;
                break;
            }
        }
        if !usable {
            underived += 1;
        }
    }
    Ok(underived)
}

/// How far the Succinct and Rank9 pair attached to a source lags it in one
/// store snapshot: the source foundations no usable attachment reaches yet,
/// per attached collection.
///
/// A foundation without an attachment is read from its own bytes by
/// [`FactRead::read_facts`], so this is a cost, not missing facts, unless the
/// read cannot read it: its bytes are not here, or they cannot form one
/// archive (malformed, or too wide for one segment). Those are the read's
/// own gap, named by [`attached_facts_read`]. There is no globally
/// consistent state to be current against: a read attaches what is present
/// and reports this, it never waits or refuses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FactLag {
    /// Source foundations no Succinct attachment reaches.
    pub succinct: usize,
    /// Source foundations no Rank9 attachment reaches.
    pub rank9: usize,
}

impl FactLag {
    /// Measure the pair attached to `source` in `snapshot`.
    pub fn of<R>(
        snapshot: &R,
        _source: Collection<SimpleArchive>,
        succinct: Collection<SuccinctArchiveBlob>,
        rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
    ) -> Result<Self>
    where
        R: StoreRead,
    {
        Ok(Self {
            succinct: snapshot
                .attached(succinct)
                .context("read the Succinct attachments")?
                .residual()
                .len(),
            rank9: snapshot
                .attached(rank9)
                .context("read the Rank9 attachments")?
                .residual()
                .len(),
        })
    }

    /// Whether every source foundation the snapshot knows of is reached by
    /// an attachment in both collections.
    pub const fn is_current(self) -> bool {
        self.succinct == 0 && self.rank9 == 0
    }
}

impl std::fmt::Display for FactLag {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} source foundation(s) not yet attached to Succinct, {} not yet attached to Rank9",
            self.succinct, self.rank9
        )
    }
}

/// Read the facts a source holds here through its attached Rank9 cover.
///
/// The attached cover is taken as the snapshot has it; every foundation it
/// does not reach whose bytes are here is read from those bytes and joined
/// in as one more shard, so a commit whose attachment is not built yet --
/// written by another key, or before the last maintenance pass -- is read
/// all the same. A foundation whose bytes are not here is left out: it is
/// counted by [`FactLag`], and nothing is fetched for it.
///
/// A trait so the read is a method on whatever snapshot the caller holds,
/// owned or borrowed; [`attached_facts`] reads an attached read already
/// taken.
pub trait FactRead: StoreRead + Sized {
    /// The facts of the source `rank9` is attached to, as this snapshot
    /// holds them.
    fn read_facts(
        &self,
        rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
    ) -> Result<FactArchive> {
        attached_facts(
            &self
                .attached(rank9)
                .context("attach the Rank9 fact cover")?,
        )
    }
}

impl<R: StoreRead> FactRead for R {}

/// The facts of an attached Rank9 read already taken, as
/// [`attached_facts`] reads them: the method form, so a caller holding the
/// read never takes its bare view, which would leave the residual out.
pub trait FactView {
    /// The attached cover and the resident residual, as one fact archive.
    fn facts(&self) -> Result<FactArchive>;
}

impl<R: StoreRead> FactView for AttachedSnapshot<R, Rank9AcceleratedSuccinctArchiveBlob> {
    fn facts(&self) -> Result<FactArchive> {
        attached_facts(self)
    }
}

/// [`FactRead::read_facts`] over an attached read already taken: its cover,
/// and its residual read from the bytes its snapshot holds
/// ([`succinctarchive_union::read_attached`]).
pub fn attached_facts<R>(
    attached: &AttachedSnapshot<R, Rank9AcceleratedSuccinctArchiveBlob>,
) -> Result<FactArchive>
where
    R: StoreRead,
{
    attached_facts_read(attached).map(AttachedRead::into_value)
}

/// [`attached_facts`], with the residual foundations the read could not
/// include named beside the facts: those whose bytes are not here, and
/// those whose bytes cannot form one archive. A caller that must tell a
/// complete read from a partial one -- a cache key, a lag report -- reads
/// this.
pub fn attached_facts_read<R>(
    attached: &AttachedSnapshot<R, Rank9AcceleratedSuccinctArchiveBlob>,
) -> Result<AttachedRead<FactArchive>>
where
    R: StoreRead,
{
    succinctarchive_union::read_attached(attached)
        .map_err(|error| anyhow!("read the attached Rank9 facts: {error}"))
}

/// Settle one read-path upkeep result: what upkeep could not do is the
/// view's lag, not a failure of the read. For a derived view that is an own
/// foundation the mapping could not represent
/// ([`CollectionRealizationError::Unmappable`], raised only after everything
/// else was derived and carried), or own commits owed to a view the signer
/// may not write ([`CollectionRealizationError::UnauthorizedProducer`],
/// which only the per-write ensure raises; maintenance carries such a view
/// and reports nothing); for
/// an attached one, a key that is not the store's host
/// ([`CollectionRealizationError::HostMismatch`]), which attaches nothing. In
/// every case the read attaches what is present and reads or counts the rest
/// ([`FactRead::read_facts`], [`underived`]). Explicit maintenance and a
/// write's [`ensure_downstream`] still name them. Every other error is
/// returned unchanged.
pub fn tolerate_own_lag<T>(
    result: std::result::Result<T, CollectionRealizationError>,
) -> std::result::Result<(), CollectionRealizationError> {
    match result {
        Ok(_)
        | Err(CollectionRealizationError::Unmappable { .. })
        | Err(CollectionRealizationError::UnauthorizedProducer { .. })
        | Err(CollectionRealizationError::HostMismatch { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Resolve the durable signer path for a pile without touching the filesystem.
pub fn signer_path(pile: &Path, explicit: Option<&Path>) -> PathBuf {
    signing_key_file::resolve_path(explicit, pile)
}

/// Strictly load an existing durable signer.
///
/// This never creates a key and never substitutes an ephemeral identity.
pub fn load_signer(pile: &Path, explicit: Option<&Path>) -> Result<SigningKey> {
    let path = signer_path(pile, explicit);
    signing_key_file::load_existing(&path)
        .with_context(|| format!("load durable signing key {}", path.display()))
}

/// Explicitly initialize a durable signer, or load the concurrent winner.
///
/// Initialization is separate from ordinary reads and writes so publication
/// cannot silently mint a new identity.
pub fn initialize_signer(pile: &Path, explicit: Option<&Path>) -> Result<SigningKey> {
    let path = signer_path(pile, explicit);
    signing_key_file::init(&path)
        .with_context(|| format!("initialize durable signing key {}", path.display()))
}

/// Open and refresh an existing pile without automatic repair, with no host:
/// the fold believes no MERGE, and every believed foundation stays on its
/// collection's frontier. Correct for any read, only wider, and for a writer
/// of COMMITs, grants or descriptors, which WRITE admits whatever the host; a
/// carry on it refuses to publish, since nothing would believe its merges.
/// Anything that publishes or reads through merges opens as its signer
/// ([`open_pile_signed`]).
pub fn open_pile_strict(path: &Path) -> Result<Pile> {
    refreshed(
        path,
        Pile::open(path).with_context(|| format!("open pile {}", path.display()))?,
    )
}

/// [`open_pile_strict`] as `host`, the durable key the caller signs with: the
/// fold believes the MERGEs `host` signed and no other key's, so the caller's
/// own carries are believed and its reads attach its own merges.
pub fn open_pile_strict_as(path: &Path, host: VerifyingKey) -> Result<Pile> {
    refreshed(
        path,
        Pile::open_as(path, host).with_context(|| format!("open pile {}", path.display()))?,
    )
}

/// Load the durable signer and open the pile as it, in one step: the way a
/// faculty that publishes or reads through merges opens a local pile. The
/// store's host is the key its carries and attachments sign with by
/// construction, so the two cannot drift apart at a call site.
pub fn open_pile_signed(pile: &Path, key: Option<&Path>) -> Result<(Pile, SigningKey)> {
    let signer = load_signer(pile, key)?;
    let opened = open_pile_strict_as(pile, signer.verifying_key())?;
    Ok((opened, signer))
}

fn refreshed(path: &Path, mut pile: Pile) -> Result<Pile> {
    if let Err(error) = pile.refresh() {
        let close = pile.close();
        let mut failure = pile_read_error(path, error);
        if let Err(close_error) = close {
            failure = failure.context(format!(
                "closing pile after failed refresh also failed: {close_error}"
            ));
        }
        return Err(failure);
    }
    Ok(pile)
}

/// Publish one complete fragment into one scoped native collection.
///
/// The signer is loaded before the pile is touched. Facts become collection
/// data, metafacts become signed commit metadata, and the fragment's shared
/// blob store supplies attachments referenced by either channel. Publication
/// is performed only by [`CollectionStoreExt::commit`]; equality of its exact
/// canonical record makes replay idempotent.
pub fn publish_fragment(
    pile_path: &Path,
    key_path: Option<&Path>,
    scope: Id,
    fragment: Fragment,
) -> Result<CollectionCommit> {
    let mut commits = publish_fragments(pile_path, key_path, scope, [fragment])?;
    Ok(commits
        .pop()
        .expect("one input fragment produces one collection commit"))
}

/// Publish a deterministic sequence of complete fragments into one collection.
///
/// This is the authored-commit migration path: the target pile is opened once,
/// each input crosses the same narrow
/// [`CollectionStoreExt::commit`] boundary, and the
/// pile is closed even if a later publication fails. Replaying a prefix or the
/// whole sequence is idempotent because both blobs and collection records are
/// content addressed.
/// Stand in for the maintenance worker on one pile file: open it, carry the
/// `scope` collection's commits through its derived chain with the pile's
/// signer, and close it. A read that follows sees what a write before it
/// published, exactly as it would after the daemon's next pass.
pub fn carry_scope(pile_path: &Path, key_path: Option<&Path>, scope: Id) -> Result<()> {
    let (mut pile, signer) = open_pile_signed(pile_path, key_path)?;
    let collection =
        crate::collection_names::open_configured(&mut pile, scope, signer.verifying_key())
            .context("open native collection descriptor")?;
    carry_facts(&mut pile, collection, &signer);
    finish_pile(pile, Ok(()))
}

pub fn publish_fragments(
    pile_path: &Path,
    key_path: Option<&Path>,
    scope: Id,
    fragments: impl IntoIterator<Item = Fragment>,
) -> Result<Vec<CollectionCommit>> {
    let (mut pile, signer) = open_pile_signed(pile_path, key_path)?;
    let collection =
        crate::collection_names::open_configured(&mut pile, scope, signer.verifying_key())
            .context("open native collection descriptor")?;
    let result = (|| {
        let mut commits = Vec::new();
        for fragment in fragments {
            commits.push(
                pile.commit(collection, &signer, fragment)
                    .context("publish native collection fragment")?,
            );
        }
        if !commits.is_empty() {
            drop(
                pollster::block_on(ensure_downstream(&mut pile, collection, &signer))
                    .context("fragments were committed, but ensuring their derived views failed")?,
            );
        }
        Ok(commits)
    })();
    finish_pile(pile, result)
}

/// Render one non-mutating pile read failure without presenting data loss as
/// routine repair.
///
/// A malformed known record and an interrupted append share the same
/// conservative core error. Only an operator inspecting the bytes can decide
/// whether the suffix is disposable, so faculties report evidence and stop.
pub fn pile_read_error(path: &Path, error: ReadError) -> anyhow::Error {
    match error {
        ReadError::CorruptPile { valid_length } => anyhow!(
            "pile {} has a malformed or incomplete known record at byte {valid_length}; this \
             reader cannot prove that the remaining bytes are a disposable torn write. The pile \
             was left unchanged. Upgrade `trible` to the matching current source cohort, then \
             inspect that boundary with `trible pile diagnose record-at {} {valid_length}` before \
             considering any destructive action",
            path.display(),
            path.display()
        ),
        ReadError::UnsupportedRecord { .. } => anyhow!(
            "pile {} contains a record format unsupported by this binary ({error}); this is \
             likely version skew. Upgrade to a reader that recognizes the marker. The pile was \
             left unchanged",
            path.display()
        ),
        other => anyhow!("refresh pile {}: {other}", path.display()),
    }
}

fn finish_pile<T>(pile: Pile, result: Result<T>) -> Result<T> {
    let close = pile.close();
    match (result, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(anyhow!("close pile: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(close_error)) => {
            Err(error.context(format!("closing pile also failed: {close_error}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use anybytes::View;
    use ed25519_dalek::SigningKey;
    use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
    use triblespace::core::blob::encodings::utf8string::UTF8String;
    use triblespace::core::collection::{empty_metadata_handle, CollectionRecord, CollectionStore};
    use triblespace::core::inline::encodings::hash::Handle;
    use triblespace::core::inline::Inline;
    use triblespace::core::metadata;
    use triblespace::core::repo::memoryrepo::MemoryRepo;
    use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
    use triblespace::core::trible::TribleSet;
    use triblespace::macros::entity;

    use super::*;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct TestFiles {
        directory: PathBuf,
        pile: PathBuf,
        key: PathBuf,
    }

    impl TestFiles {
        fn new() -> Self {
            let serial = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "faculties-native-collection-{}-{serial}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&directory);
            fs::create_dir_all(&directory).unwrap();
            let pile = directory.join("test.pile");
            File::create(&pile).unwrap();
            let key = directory.join("test.key");
            Self {
                directory,
                pile,
                key,
            }
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn id(byte: u8) -> Id {
        Id::new([byte; 16]).unwrap()
    }

    #[test]
    fn shared_configuration_is_inert_and_thread_shareable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Storage>();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-opened.pile");
        let storage = Storage::shared(path.clone(), Some(dir.path().join("missing.key")));
        let clone = storage.clone();
        assert_eq!(storage.path(), path);
        clone.close().unwrap();
        assert_eq!(dir.path().read_dir().unwrap().count(), 0);
    }

    #[test]
    fn shared_operations_keep_the_peer_runtime_and_local_backend() {
        use triblespace::core::repo::BlobStorePut;

        let files = TestFiles::new();
        initialize_signer(&files.pile, Some(&files.key)).unwrap();
        let storage = Storage::shared(files.pile.clone(), Some(files.key.clone()));
        let clone = storage.clone();
        let (peer, runtime) = storage
            .with_store(|store, _, runtime| Ok((store.id(), Arc::clone(runtime))))
            .unwrap();
        // A reopen would create a different file and lose the original index.
        let moved = files.directory.join("still-open.pile");
        fs::rename(&files.pile, &moved).unwrap();
        let handle = clone
            .with_pile(|pile, _| Ok(pile.put::<UTF8String, _>("shared resident bytes")?))
            .unwrap();
        storage
            .scope(|storage| {
                storage.with_store(|store, _, current_runtime| {
                    assert_eq!(store.id(), peer);
                    assert!(Arc::ptr_eq(current_runtime, &runtime));
                    let snapshot = store.snapshot()?;
                    let text: View<str> = current_runtime.block_on(snapshot.get(handle))?;
                    assert_eq!(&*text, "shared resident bytes");
                    Ok(())
                })
            })
            .unwrap();
        clone
            .with_store(|store, _, _| {
                assert_eq!(
                    store.id(),
                    peer,
                    "nested scope must not close its application owner"
                );
                Ok(())
            })
            .unwrap();
        assert!(
            !files.pile.exists(),
            "no operation may reopen the configured pathname"
        );
        storage.close().unwrap();
        clone.close().unwrap();
        let mut reopened = open_pile_strict(&moved).unwrap();
        assert!(reopened.snapshot().unwrap().contains_blob(handle).unwrap());
        reopened.close().unwrap();
    }

    #[test]
    fn retained_store_observes_external_appends_without_changing_old_snapshots() {
        use triblespace::core::repo::BlobStorePut;

        let files = TestFiles::new();
        initialize_signer(&files.pile, Some(&files.key)).unwrap();
        let storage = Storage::shared(files.pile.clone(), Some(files.key.clone()));
        let before = storage
            .with_store(|store, _, _| Ok(store.snapshot()?))
            .unwrap();
        let mut other = open_pile_strict(&files.pile).unwrap();
        let handle = other
            .put::<UTF8String, _>("appended by another process")
            .unwrap();
        other.close().unwrap();
        let after = storage
            .with_store(|store, _, _| Ok(store.snapshot()?))
            .unwrap();
        assert!(!before.contains_blob(handle).unwrap());
        assert!(after.contains_blob(handle).unwrap());
        assert!(
            !before.contains_blob(handle).unwrap(),
            "refresh must not mutate an earlier observation"
        );
        storage.close().unwrap();
    }

    #[test]
    fn shared_operation_errors_do_not_discard_the_connection() {
        let files = TestFiles::new();
        initialize_signer(&files.pile, Some(&files.key)).unwrap();
        let storage = Storage::shared(files.pile.clone(), Some(files.key.clone()));
        let peer = storage.with_store(|store, _, _| Ok(store.id())).unwrap();
        let error = storage
            .with_store::<()>(|_, _, _| anyhow::bail!("operation failed"))
            .unwrap_err();
        assert_eq!(error.to_string(), "operation failed");
        storage
            .with_store(|store, _, _| {
                assert_eq!(store.id(), peer);
                Ok(())
            })
            .unwrap();
        storage.close().unwrap();
    }

    #[test]
    fn panicking_local_handler_does_not_poison_shared_storage() {
        let files = TestFiles::new();
        initialize_signer(&files.pile, Some(&files.key)).unwrap();
        let storage = Storage::shared(files.pile.clone(), Some(files.key.clone()));
        let peer = storage.with_store(|store, _, _| Ok(store.id())).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            storage.with_pile::<()>(|_, _| panic!("handler panic"))
        }));
        assert!(result.is_err());
        storage
            .with_store(|store, _, _| {
                assert_eq!(store.id(), peer);
                Ok(())
            })
            .unwrap();
        storage.close().unwrap();
    }

    #[test]
    fn independent_owners_do_not_share_by_path() {
        let files = TestFiles::new();
        initialize_signer(&files.pile, Some(&files.key)).unwrap();
        let first = Storage::shared(files.pile.clone(), Some(files.key.clone()));
        let second = Storage::shared(files.pile.clone(), Some(files.key.clone()));
        let first_peer = first.with_store(|store, _, _| Ok(store.id())).unwrap();
        let second_peer = second.with_store(|store, _, _| Ok(store.id())).unwrap();
        assert_ne!(
            first_peer, second_peer,
            "sharing is explicit, never an ambient path cache"
        );
        first.close().unwrap();
        second.close().unwrap();
    }

    #[test]
    fn compound_cli_scope_reuses_and_closes_its_own_store() {
        let files = TestFiles::new();
        initialize_signer(&files.pile, Some(&files.key)).unwrap();
        let cli = Storage::new(files.pile.clone(), Some(files.key.clone()));
        let owner = cli
            .scope(|storage| {
                let peer = storage.with_store(|store, _, _| Ok(store.id()))?;
                storage.with_store(|store, _, _| {
                    assert_eq!(store.id(), peer);
                    Ok(())
                })?;
                Ok(storage.clone())
            })
            .unwrap();
        assert!(owner.shared.as_ref().unwrap().lock().unwrap().is_none());
        assert!(cli.shared.is_none());
    }

    #[test]
    fn live_read_fetches_only_demanded_bytes_and_preserves_its_observation() {
        use anybytes::Bytes;
        use triblespace::core::blob::encodings::UnknownBlob;
        use triblespace::core::repo::{BlobStorePut, WantRead};

        struct Supply {
            pile: Pile,
            payload: Bytes,
            requested: Vec<Inline<Handle<UnknownBlob>>>,
        }
        impl SnapshotSource for Supply {
            type Snapshot = triblespace::core::repo::pile::PileSnapshot;
            type SnapshotError = <Pile as SnapshotSource>::SnapshotError;

            fn snapshot(&mut self) -> Result<Self::Snapshot, Self::SnapshotError> {
                self.pile.snapshot()
            }
        }
        impl AsyncBlobStoreAcquire for Supply {
            type AcquireError = std::io::Error;

            async fn acquire(
                &mut self,
                handle: Inline<Handle<UnknownBlob>>,
            ) -> Result<Option<Bytes>, Self::AcquireError> {
                self.requested.push(handle);
                let stored = self
                    .pile
                    .put::<UnknownBlob, _>(self.payload.clone())
                    .unwrap();
                assert_eq!(stored, handle);
                Ok(Some(self.payload.clone()))
            }
        }

        let files = TestFiles::new();
        let payload: Bytes = Vec::from("selected body").into();
        let mut source = MemoryRepo::default();
        let handle = source.put::<UnknownBlob, _>(payload.clone()).unwrap();
        let mut store = Supply {
            pile: open_pile_strict(&files.pile).unwrap(),
            payload,
            requested: Vec::new(),
        };
        let before = store.snapshot().unwrap();
        let value = pollster::block_on(read(&mut store, &before, |reader| {
            let bytes = reader.get::<Bytes, UnknownBlob>(handle)?;
            Ok(bytes)
        }))
        .unwrap();
        assert_eq!(&*value, b"selected body");
        assert_eq!(store.requested, [handle]);
        assert!(!before.contains_blob(handle).unwrap());
        assert!(store.snapshot().unwrap().wants().unwrap().next().is_none());
        let resident = store.snapshot().unwrap();
        let result = pollster::block_on(read(&mut store, &resident, |_| {
            // Accidentally capturing the old reader cannot spin forever.
            Ok(before.get::<Bytes, UnknownBlob>(handle)?)
        }));
        assert!(result.is_err());
        assert_eq!(store.requested, [handle]);
        drop(resident);
        drop(before);
        store.pile.close().unwrap();
    }

    #[test]
    fn strict_open_reports_evidence_without_prescribing_data_loss() {
        let files = TestFiles::new();
        fs::write(&files.pile, [0xFF; 8]).unwrap();
        let before = fs::read(&files.pile).unwrap();

        let error = open_pile_strict(&files.pile)
            .err()
            .expect("malformed pile must fail strict open");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("malformed or incomplete known record at byte 0"));
        assert!(rendered.contains("cannot prove"));
        assert!(rendered.contains("matching current source cohort"));
        assert!(rendered.contains("pile diagnose record-at"));
        assert!(!rendered.contains("pile amputate"));
        assert_eq!(fs::read(&files.pile).unwrap(), before);

        let mut unsupported = [0u8; 256];
        unsupported[..16].fill(0xA5);
        fs::write(&files.pile, unsupported).unwrap();
        let error = open_pile_strict(&files.pile)
            .err()
            .expect("unsupported marker must fail strict open");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("unsupported by this binary"));
        assert!(rendered.contains("likely version skew"));
        assert!(!rendered.contains("pile amputate"));
        assert_eq!(fs::read(&files.pile).unwrap(), unsupported);
    }

    #[test]
    fn target_discovery_registers_descriptor_without_definition_record() {
        // Two REAL scopes rather than two arbitrary ids: a root is anchored by
        // a name now, and an id this build has never named is one it cannot
        // open at all. Any two distinct faculties prove the same thing.
        let signer = SigningKey::from_bytes(&[7; 32]);
        let team = signer.verifying_key();
        let target_scope = crate::schemas::wiki::DEFAULT_SCOPE_ID;
        let other_scope = crate::schemas::compass::DEFAULT_SCOPE_ID;
        let mut store = MemoryRepo::default();
        let target = crate::collection_names::open(&mut store, target_scope, team)
            .unwrap()
            .handle();
        let other = crate::collection_names::open(&mut store, other_scope, team)
            .unwrap()
            .handle();

        let target_commit = CollectionCommit::sign(
            &signer,
            target,
            Inline::new([1; 32]),
            empty_metadata_handle(),
        );
        let other_commit = CollectionCommit::sign(
            &signer,
            other,
            Inline::new([2; 32]),
            empty_metadata_handle(),
        );
        let target_merge = CollectionMerge::sign(
            &signer,
            target,
            [target_commit.data(), Inline::new([4; 32])],
            Inline::new([5; 32]),
        )
        .unwrap();
        let other_merge = CollectionMerge::sign(
            &signer,
            other,
            [other_commit.data(), Inline::new([7; 32])],
            Inline::new([8; 32]),
        )
        .unwrap();
        let derive_to_target = CollectionDerive::sign(
            &signer,
            target,
            triblespace::core::collection::SourceLocator::of(other_commit.data().raw),
            Inline::new([10; 32]),
        );
        let derive_from_target = CollectionDerive::sign(
            &signer,
            other,
            triblespace::core::collection::SourceLocator::of(target_commit.data().raw),
            Inline::new([12; 32]),
        );

        for record in [
            CollectionRecord::Commit(target_commit),
            CollectionRecord::Commit(other_commit),
            CollectionRecord::Merge(target_merge),
            CollectionRecord::Merge(other_merge),
            CollectionRecord::Derive(derive_to_target),
            CollectionRecord::Derive(derive_from_target),
        ] {
            store.insert(record).unwrap();
        }

        let discovered = discover_target(&mut store, target_scope, team).unwrap();
        assert_eq!(discovered.commits(), &[target_commit]);
        assert_eq!(discovered.merges(), &[target_merge]);
        assert_eq!(discovered.derives(), &[derive_to_target]);
        assert!(
            !store.blobs.is_empty(),
            "registration retains the descriptor attachment closure"
        );
    }

    #[test]
    fn the_attached_pair_maintains_a_shard_preserving_rank9_view() {
        let signer = SigningKey::from_bytes(&[7; 32]);
        let mut store = MemoryRepo::for_host(signer.verifying_key());
        let source = crate::collection_names::open(
            &mut store,
            crate::schemas::wiki::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
        )
        .unwrap();
        let (_succinct, rank9) = fact_pair(&mut store, source).unwrap();
        let fragment = entity! {
            metadata::tag: &id(9),
            metadata::name: "maintained facts",
        };
        let expected = fragment.facts().clone();
        store.commit(source, &signer, fragment).unwrap();

        let after = pollster::block_on(async {
            drop(store.ensure(source, &signer).await.unwrap());
            maintain_downstream(&mut store, source, &signer)
                .await
                .unwrap();
            store.snapshot().unwrap()
        });
        let observed = after.attached(rank9).unwrap();
        let view = observed.view::<FactArchive>().unwrap();
        let actual: TribleSet = view.iter().collect();

        assert_eq!(actual, expected);
        assert_eq!(observed.support().len(), 1);
        assert!(observed.residual().is_empty());
        assert_eq!(view.segment_count(), 1);
        assert_eq!(discovered_records(&after).unwrap().maps().len(), 2);
    }

    /// A write's commit is readable through the fact pair at once, whoever
    /// wrote it and whether or not its attachment exists yet: the host's
    /// write attaches the source's frontier, and a read takes what no
    /// attachment reaches from its own bytes. Another key's commit is
    /// attached by the host's next write, not left to its writer.
    #[test]
    fn every_commit_is_readable_through_the_pair_whoever_wrote_it() {
        use triblespace::core::collection::grant_collection_write;

        let owner = SigningKey::from_bytes(&[11; 32]);
        let writer = SigningKey::from_bytes(&[12; 32]);
        let mut store = MemoryRepo::for_host(owner.verifying_key());
        let source = crate::collection_names::open(
            &mut store,
            crate::schemas::wiki::DEFAULT_SCOPE_ID,
            owner.verifying_key(),
        )
        .unwrap();
        let (succinct, rank9) = fact_pair(&mut store, source).unwrap();
        let lag = |store: &mut MemoryRepo| {
            FactLag::of(&store.snapshot().unwrap(), source, succinct, rank9).unwrap()
        };
        let facts = |store: &mut MemoryRepo| -> TribleSet {
            store
                .snapshot()
                .unwrap()
                .read_facts(rank9)
                .unwrap()
                .iter()
                .collect()
        };

        grant_collection_write(&mut store, source.handle(), &owner, writer.verifying_key())
            .unwrap();
        let written = entity! { metadata::name: "another key's write" };
        store.commit(source, &writer, written.clone()).unwrap();
        // Nothing is attached yet, and the commit is read all the same.
        assert_eq!(
            lag(&mut store),
            FactLag {
                succinct: 1,
                rank9: 1
            }
        );
        assert_eq!(facts(&mut store), written.facts().clone());

        let own = entity! { metadata::name: "owner" };
        store.commit(source, &owner, own.clone()).unwrap();
        drop(pollster::block_on(ensure_downstream(&mut store, source, &owner)).unwrap());
        assert!(lag(&mut store).is_current(), "{:?}", lag(&mut store));
        assert_eq!(
            facts(&mut store),
            (written.clone() + own.clone()).facts().clone()
        );

        // A key that is not the store's host attaches nothing -- its MAPs
        // would be believed nowhere -- and its write is read all the same.
        let later = entity! { metadata::name: "later" };
        store.commit(source, &writer, later.clone()).unwrap();
        let before = store.snapshot().unwrap().records().unwrap().count();
        drop(pollster::block_on(ensure_downstream(&mut store, source, &writer)).unwrap());
        assert_eq!(store.snapshot().unwrap().records().unwrap().count(), before);
        assert_eq!(
            facts(&mut store),
            (written.clone() + own.clone() + later).facts().clone()
        );
    }

    /// A write is done once its commit is, whatever else is attached to its
    /// source: upkeep passes over a descriptor no attached read here serves
    /// (one naming a second parent), and goes on past an attached index
    /// whose mapping fails on a commit, so the fact pair still attaches
    /// every commit.
    #[test]
    fn a_write_is_done_whatever_an_unrelated_attached_collection_does() {
        use anybytes::Bytes;
        use triblespace::core::blob::encodings::succinctarchive::SuccinctArchiveBlob;
        use triblespace::core::blob::encodings::UnknownBlob;
        use triblespace::core::blob::{Blob, IntoBlob};
        use triblespace::core::collection::records::{
            collection_mapping, collection_parent, collection_representation,
            KIND_COLLECTION_DESCRIPTOR,
        };
        use triblespace::core::collection::{CollectionAttachment, CollectionCommit};
        use triblespace::core::metadata::MetaDescribe;
        use triblespace::core::repo::BlobStorePut;
        use triblespace::core::trible::Fragment;
        use triblespace_search::portable_bm25::PortableBM25Blob;
        use triblespace_search::text_bm25::{Bm25Tokenizer, TextAttributeToBm25};

        let owner = SigningKey::from_bytes(&[13; 32]);
        let mut store = MemoryRepo::for_host(owner.verifying_key());
        let source = crate::collection_names::open(
            &mut store,
            crate::schemas::wiki::DEFAULT_SCOPE_ID,
            owner.verifying_key(),
        )
        .unwrap();
        let (succinct, rank9) = fact_pair(&mut store, source).unwrap();
        let described = |store: &mut MemoryRepo, text: Blob<UTF8String>| {
            let text = store.put::<UTF8String, _>(text).unwrap();
            let mut fragment = Fragment::empty();
            fragment += entity! { metadata::description: text };
            store.commit(source, &owner, fragment).unwrap();
        };
        let write = |store: &mut MemoryRepo, name: &'static str| {
            store
                .commit(source, &owner, entity! { metadata::name: name })
                .unwrap();
            pollster::block_on(ensure_downstream(store, source, &owner))
        };

        // A descriptor naming the source and a second parent, listed by a
        // record naming it.
        let second = crate::collection_names::open(
            &mut store,
            crate::schemas::compass::DEFAULT_SCOPE_ID,
            owner.verifying_key(),
        )
        .unwrap();
        let (facts, mut blobs) = entity! {
            metadata::tag: KIND_COLLECTION_DESCRIPTOR,
            collection_parent*: [source.handle(), second.handle()],
            collection_representation*: <SuccinctArchiveBlob as MetaDescribe>::describe(),
            collection_mapping*: <SuccinctArchiveBlob as CollectionAttachment>::fragment(&()),
        }
        .into_facts_and_blobs();
        for (_, blob) in blobs.snapshot().unwrap() {
            store.put::<UnknownBlob, _>(blob).unwrap();
        }
        let both = store.put::<SimpleArchive, _>(facts).unwrap();
        let stray = store
            .put::<SimpleArchive, _>(TribleSet::new().to_blob())
            .unwrap();
        store
            .insert(CollectionRecord::Commit(CollectionCommit::sign(
                &owner,
                both,
                Handle::<SimpleArchive>::to_hash(stray),
                empty_metadata_handle(),
            )))
            .unwrap();
        assert!(store
            .snapshot()
            .unwrap()
            .collections()
            .unwrap()
            .contains(&both));
        let report = write(&mut store, "beside a second parent").unwrap();
        assert!(!report.realized.contains(&both));
        assert!(report.failed_attached.is_empty());
        assert!(
            FactLag::of(&store.snapshot().unwrap(), source, succinct, rank9)
                .unwrap()
                .is_current()
        );

        // A text index over descriptions, seeded while every text is well
        // formed; then a commit whose description is not UTF-8, which its
        // mapping refuses.
        let index = store
            .attach::<PortableBM25Blob>(
                source,
                TextAttributeToBm25 {
                    attribute: metadata::description.id(),
                    tokenizer: Bm25Tokenizer::Word,
                },
            )
            .unwrap();
        described(&mut store, "well formed".to_blob());
        pollster::block_on(seed_attached(&mut store, index, &owner)).unwrap();
        described(&mut store, Blob::new(Bytes::from(vec![0xFF, 0xFE])));
        let report = write(&mut store, "beside a failing index").unwrap();
        assert_eq!(
            report
                .failed_attached
                .iter()
                .map(|(attached, _)| attached.handle)
                .collect::<Vec<_>>(),
            [index.handle()]
        );
        assert!(report.realized.contains(&rank9.handle()));
        assert!(
            FactLag::of(&store.snapshot().unwrap(), source, succinct, rank9)
                .unwrap()
                .is_current()
        );
    }

    #[test]
    fn fact_read_stands_for_nothing_beneath_unadmitted_commits_and_reads_the_rollup_once_they_are_admitted(
    ) {
        use triblespace::core::blob::IntoBlob;
        use triblespace::core::repo::BlobStorePut;

        let owner = SigningKey::from_bytes(&[7; 32]);
        let author = SigningKey::from_bytes(&[8; 32]);
        // Opened as the owner, whose MERGE below the fold therefore believes.
        let mut store = MemoryRepo::for_host(owner.verifying_key());
        let collection = crate::collection_names::open(
            &mut store,
            crate::schemas::wiki::DEFAULT_SCOPE_ID,
            owner.verifying_key(),
        )
        .unwrap();
        let left = entity! { metadata::name: "left" };
        let right = entity! { metadata::name: "right" };
        let expected = (left.clone() + right.clone()).facts().clone();
        let commits: Vec<_> = [left, right]
            .into_iter()
            .map(|fragment| {
                let blob = IntoBlob::<SimpleArchive>::to_blob(fragment.facts().clone());
                CollectionCommit::sign(
                    &author,
                    collection.handle(),
                    Handle::<SimpleArchive>::to_hash(blob.get_handle()),
                    empty_metadata_handle(),
                )
            })
            .collect();
        for commit in &commits {
            store.insert(CollectionRecord::Commit(*commit)).unwrap();
        }
        let joined = store.put::<SimpleArchive, _>(expected.clone()).unwrap();
        store
            .insert(CollectionRecord::Merge(
                CollectionMerge::sign(
                    &owner,
                    collection.handle(),
                    [commits[0].data(), commits[1].data()],
                    Handle::<SimpleArchive>::to_hash(joined),
                )
                .unwrap(),
            ))
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        assert!(collection.admitted(&snapshot).unwrap().is_empty());
        for commit in &commits {
            assert!(!snapshot
                .contains_blob(Handle::<SimpleArchive>::from_hash(commit.data()))
                .unwrap());
        }
        // The owner's MERGE is believed, but the commits it joins are signed
        // by a key nothing here admits, so the node it produces has no row to
        // stand on: the collection stands for nothing, and the read says so.
        let (facts, support) = read_fact_collection(collection, &snapshot).unwrap();
        assert_eq!(facts.len(), 0);
        assert!(support.is_empty());

        // Admitted commits for the same payloads make the rollup stand. The
        // input payloads never become resident: the read is the joined blob,
        // and its support is what the commits stand for.
        for commit in &commits {
            store
                .insert(CollectionRecord::Commit(CollectionCommit::sign(
                    &owner,
                    collection.handle(),
                    commit.data(),
                    empty_metadata_handle(),
                )))
                .unwrap();
        }
        let snapshot = store.snapshot().unwrap();
        assert_eq!(collection.admitted(&snapshot).unwrap().len(), 2);
        for commit in &commits {
            assert!(!snapshot
                .contains_blob(Handle::<SimpleArchive>::from_hash(commit.data()))
                .unwrap());
        }
        let (facts, support) = read_fact_collection(collection, &snapshot).unwrap();
        assert_eq!(facts, expected);
        assert_eq!(support.len(), 2);
    }

    #[test]
    fn storage_opens_as_its_signer_so_its_own_merges_are_read_and_a_keyless_open_reads_wider() {
        use triblespace::core::collection::MERGE_FAN_IN;

        let files = TestFiles::new();
        let signer = initialize_signer(&files.pile, Some(&files.key)).unwrap();
        let scope = crate::schemas::wiki::DEFAULT_SCOPE_ID;
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        assert_eq!(names.len(), MERGE_FAN_IN);
        publish_fragments(
            &files.pile,
            Some(&files.key),
            scope,
            names.map(|name| entity! { _ @ metadata::name: name }),
        )
        .unwrap();
        // The maintenance pass carries the root as the signer: one merge of
        // the whole tier.
        let mut pile = open_pile_strict_as(&files.pile, signer.verifying_key()).unwrap();
        let collection =
            crate::collection_names::open_configured(&mut pile, scope, signer.verifying_key())
                .unwrap();
        drop(pollster::block_on(pile.maintain(collection, &signer)).unwrap());
        pile.close().unwrap();

        let observe = |pile: &mut Pile| -> Result<(usize, TribleSet)> {
            let snapshot = pile.snapshot()?;
            let observed = snapshot.collection(collection)?;
            Ok((observed.cover().len(), observed.view::<TribleSet>()?))
        };
        // Every faculty opens through Storage, as the key it signs with, so
        // the fold believes that key's merge and a read attaches it alone.
        let (width, facts) = Storage::new(files.pile.clone(), Some(files.key.clone()))
            .with_pile(|pile, _| observe(pile))
            .unwrap();
        assert_eq!(width, 1, "a one-shot open reads the signer's merge");
        let shared = Storage::shared(files.pile.clone(), Some(files.key.clone()));
        let (shared_width, shared_facts) = shared.with_pile(|pile, _| observe(pile)).unwrap();
        shared.close().unwrap();
        assert_eq!(
            shared_width, 1,
            "the shared session store is opened as the signer"
        );
        assert_eq!(shared_facts, facts);

        // With no host the fold believes no MERGE: every commit is read on
        // its own, and the facts are the same.
        let mut keyless = open_pile_strict(&files.pile).unwrap();
        let (wide, wide_facts) = observe(&mut keyless).unwrap();
        keyless.close().unwrap();
        assert_eq!(wide, MERGE_FAN_IN);
        assert_eq!(wide_facts, facts);
        assert!(!facts.is_empty());
    }

    #[test]
    fn carry_scope_opens_as_the_signer_so_the_fact_pair_attaches_its_root_merge() {
        use triblespace::core::collection::{CoverageRead, MERGE_FAN_IN};

        let files = TestFiles::new();
        let signer = initialize_signer(&files.pile, Some(&files.key)).unwrap();
        let scope = crate::schemas::wiki::DEFAULT_SCOPE_ID;
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        assert_eq!(names.len(), MERGE_FAN_IN);
        publish_fragments(
            &files.pile,
            Some(&files.key),
            scope,
            names.map(|name| entity! { _ @ metadata::name: name }),
        )
        .unwrap();
        // A signed open folds as the signer it hands back.
        let (mut pile, loaded) = open_pile_signed(&files.pile, Some(&files.key)).unwrap();
        assert_eq!(loaded.verifying_key(), signer.verifying_key());
        let host = pile
            .snapshot()
            .unwrap()
            .index(&BTreeSet::new())
            .unwrap()
            .host();
        assert_eq!(
            host.map(|host| host.raw),
            Some(signer.verifying_key().to_bytes())
        );
        let collection =
            crate::collection_names::open_configured(&mut pile, scope, signer.verifying_key())
                .unwrap();
        drop(pollster::block_on(pile.maintain(collection, &signer)).unwrap());
        pile.close().unwrap();

        // The daemon's stand-in attaches the fact pair to the node the
        // signer's root merge produced. It can only do so opened as the
        // signer: a store that believes no merge and no MAP has neither the
        // merged node to attach nor the right to publish a MAP.
        carry_scope(&files.pile, Some(&files.key), scope).unwrap();

        let rank9_cover = |pile: &mut Pile| -> (usize, usize) {
            let (_, rank9) = fact_pair(pile, collection).unwrap();
            let snapshot = pile.snapshot().unwrap();
            let attached = snapshot.attached(rank9).unwrap();
            (attached.cover().len(), attached.residual().len())
        };
        let mut pile = open_pile_strict_as(&files.pile, signer.verifying_key()).unwrap();
        assert_eq!(
            rank9_cover(&mut pile),
            (1, 0),
            "one attachment of the merged node covers the tier"
        );
        pile.close().unwrap();
        // With no host nothing is believed: no merge, no MAP, so every
        // commit is residual and read from its bytes.
        let mut keyless = open_pile_strict(&files.pile).unwrap();
        assert_eq!(rank9_cover(&mut keyless), (0, MERGE_FAN_IN));
        keyless.close().unwrap();
    }

    #[test]
    fn a_shared_store_refuses_a_signing_key_replaced_under_it() {
        let files = TestFiles::new();
        initialize_signer(&files.pile, Some(&files.key)).unwrap();
        let shared = Storage::shared(files.pile.clone(), Some(files.key.clone()));
        shared.with_pile(|_, _| Ok(())).unwrap();

        // Another key file replaces the one the session opened as.
        let other = files.directory.join("other.key");
        initialize_signer(&files.pile, Some(&other)).unwrap();
        fs::copy(&other, &files.key).unwrap();
        let error = shared.with_pile(|_, _| Ok(())).unwrap_err().to_string();
        assert!(
            error.contains("signing key changed while the shared faculty store was open"),
            "{error}"
        );
        let error = shared.with_store(|_, _, _| Ok(())).unwrap_err().to_string();
        assert!(error.contains("restart the process"), "{error}");
        shared.close().unwrap();

        // A fresh owner opens as the key the file now holds.
        Storage::shared(files.pile.clone(), Some(files.key.clone()))
            .with_pile(|_, _| Ok(()))
            .unwrap();
    }

    #[test]
    fn publication_conserves_both_fact_channels_and_attachments_and_replays_idempotently() {
        let files = TestFiles::new();
        initialize_signer(&files.pile, Some(&files.key)).unwrap();

        let mut fragment = entity! { _ @ metadata::name: "content attachment" };
        let content_root = fragment.root().unwrap();
        let description = entity! { _ @ metadata::name: "metadata attachment" };
        let metadata_root = description.root().unwrap();
        fragment.describe_with(description);
        let expected_facts = fragment.facts().clone();
        let expected_metafacts = fragment.metafacts().clone();
        assert!(!expected_facts.is_empty());
        assert!(!expected_metafacts.is_empty());

        let team = load_signer(&files.pile, Some(&files.key))
            .unwrap()
            .verifying_key();
        let target_scope = crate::schemas::wiki::DEFAULT_SCOPE_ID;
        let other_scope = crate::schemas::compass::DEFAULT_SCOPE_ID;
        let first = publish_fragment(
            &files.pile,
            Some(&files.key),
            target_scope,
            fragment.clone(),
        )
        .unwrap();
        let after_first = fs::metadata(&files.pile).unwrap().len();

        let unrelated = entity! { _ @ metadata::tag: &id(9) };
        publish_fragment(&files.pile, Some(&files.key), other_scope, unrelated).unwrap();
        let before_replay = fs::metadata(&files.pile).unwrap().len();
        let repeated =
            publish_fragment(&files.pile, Some(&files.key), target_scope, fragment).unwrap();
        let after_replay = fs::metadata(&files.pile).unwrap().len();

        assert_eq!(repeated, first);
        assert!(before_replay > after_first);
        assert_eq!(after_replay, before_replay);

        let mut pile = open_pile_strict(&files.pile).unwrap();
        let target_collection =
            crate::collection_names::open(&mut pile, target_scope, team).unwrap();
        let target = discover_target(&mut pile, target_scope, team).unwrap();
        assert_eq!(target.commits(), &[first]);
        assert_eq!(target.commits()[0].collection(), target_collection.handle());
        assert!(target.merges().is_empty());
        assert!(target.derives().is_empty());

        let unrelated_target = discover_target(&mut pile, other_scope, team).unwrap();
        assert_eq!(unrelated_target.commits().len(), 1);

        let reader = pile.snapshot().unwrap();
        let data_handle = Handle::<SimpleArchive>::from_hash(first.data());
        let actual_facts: TribleSet = reader.get(data_handle).unwrap();
        let actual_metafacts: TribleSet = reader.get(first.metadata()).unwrap();
        assert_eq!(actual_facts, expected_facts);
        assert_eq!(actual_metafacts, expected_metafacts);

        let content_handle = actual_facts
            .iter()
            .find(|fact| fact.e() == &content_root && fact.a() == &metadata::name.id())
            .map(|fact| *fact.v::<Handle<UTF8String>>())
            .expect("content attachment handle");
        let content: View<str> = reader.get(content_handle).unwrap();
        assert_eq!(&*content, "content attachment");
        let metadata_handle = actual_metafacts
            .iter()
            .find(|fact| fact.e() == &metadata_root && fact.a() == &metadata::name.id())
            .map(|fact| *fact.v::<Handle<UTF8String>>())
            .expect("metadata attachment handle");
        let metadata_text: View<str> = reader.get(metadata_handle).unwrap();
        assert_eq!(&*metadata_text, "metadata attachment");
        pile.close().unwrap();
    }

    #[test]
    fn missing_signer_fails_before_the_pile_is_touched() {
        let files = TestFiles::new();
        let missing = files.directory.join("missing.key");
        let before = fs::metadata(&files.pile).unwrap().len();

        let error = publish_fragment(
            &files.pile,
            Some(&missing),
            id(1),
            entity! { _ @ metadata::tag: &id(2) },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("load durable signing key"));
        assert!(!missing.exists());
        assert_eq!(fs::metadata(&files.pile).unwrap().len(), before);
    }

    /// Review finding, 2026-09-28: the lag count took any leaf for a
    /// foundation as done, so a file whose only row had not arrived here was
    /// not counted although the index could not answer for it. A leaf counts
    /// only when its output is here.
    #[cfg(feature = "local-embed")]
    #[test]
    fn a_leaf_whose_output_is_not_here_leaves_its_foundation_underived() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        use triblespace::core::repo::BlobStorePut;
        use triblespace::core::trible::Trible;
        use triblespace_search::nvfp4::{NvFp4CosineSet, NvFp4EmbeddingAttribute};
        use triblespace_search::schemas::Embedding;

        let key = SigningKey::from_bytes(&[0x78; 32]);
        let policy = CollectionPolicy::new(
            AdmissionPolicy::direct(key.verifying_key()),
            AdmissionPolicy::direct(key.verifying_key()),
        );
        let attribute = Id::new([0xA7; 16]).unwrap();
        let mut store = MemoryRepo::default();
        let source = store.collection("vectors", policy.clone()).unwrap();
        let view = store
            .derive::<NvFp4CosineSet<Embedding>>(
                source,
                NvFp4EmbeddingAttribute::new(attribute, 3).unwrap(),
                policy,
            )
            .unwrap();
        let commit = |store: &mut MemoryRepo, entity: u8, vector: Vec<f32>| {
            let embedding = store.put::<Embedding, _>(vector).unwrap();
            let mut facts = TribleSet::new();
            facts.insert(&Trible::force(
                &Id::new([entity; 16]).unwrap(),
                &attribute,
                &embedding,
            ));
            store
                .commit(source, &key, Fragment::from(facts))
                .unwrap()
                .data()
        };
        commit(&mut store, 1, vec![1.0, 0.0, 0.0]);
        commit(&mut store, 2, vec![0.0, 1.0, 0.0]);
        drop(pollster::block_on(store.ensure(view, &key)).unwrap());
        let count =
            |store: &mut MemoryRepo| underived(&store.snapshot().unwrap(), source, view).unwrap();
        assert_eq!(count(&mut store), 0);

        // A third file whose row record arrived before its row.
        let waiting = commit(&mut store, 3, vec![0.0, 0.0, 1.0]);
        store
            .insert(CollectionRecord::Derive(CollectionDerive::sign(
                &key,
                view.handle(),
                SourceLocator::of(waiting.raw),
                Inline::new([0x77; 32]),
            )))
            .unwrap();
        assert_eq!(count(&mut store), 1);
        // And a fourth with no row at all.
        commit(&mut store, 4, vec![0.6, 0.8, 0.0]);
        assert_eq!(count(&mut store), 2);
    }
}

/// The records of one snapshot by kind, for tests that count what a write
/// published. Tests may walk records; nothing on a read or maintenance path
/// does.
#[cfg(test)]
pub(crate) struct DiscoveredRecords {
    commits: Vec<triblespace::core::collection::CollectionCommit>,
    merges: Vec<triblespace::core::collection::CollectionMerge>,
    derives: Vec<triblespace::core::collection::CollectionDerive>,
    maps: Vec<triblespace::core::collection::CollectionMap>,
}

#[cfg(test)]
impl DiscoveredRecords {
    pub(crate) fn maps(&self) -> &[triblespace::core::collection::CollectionMap] {
        &self.maps
    }

    pub(crate) fn commits(&self) -> &[triblespace::core::collection::CollectionCommit] {
        &self.commits
    }

    pub(crate) fn merges(&self) -> &[triblespace::core::collection::CollectionMerge] {
        &self.merges
    }

    pub(crate) fn derives(&self) -> &[triblespace::core::collection::CollectionDerive] {
        &self.derives
    }
}

#[cfg(test)]
pub(crate) fn discovered_records<S: triblespace::core::collection::CollectionRead>(
    snapshot: &S,
) -> anyhow::Result<DiscoveredRecords> {
    use triblespace::core::collection::CollectionRecord;
    let mut discovered = DiscoveredRecords {
        commits: Vec::new(),
        merges: Vec::new(),
        derives: Vec::new(),
        maps: Vec::new(),
    };
    for record in snapshot
        .records()
        .map_err(|error| anyhow::anyhow!("{error}"))?
    {
        match record.map_err(|error| anyhow::anyhow!("{error}"))? {
            CollectionRecord::Commit(commit) => discovered.commits.push(commit),
            CollectionRecord::Merge(merge) => discovered.merges.push(merge),
            CollectionRecord::Derive(derive) => discovered.derives.push(derive),
            CollectionRecord::Map(map) => discovered.maps.push(map),
        }
    }
    Ok(discovered)
}

/// What the maintenance worker does between a write and a read: carry the
/// source and attach what the carry leaves into every collection attached to
/// it, and derive the signer's leaves into every collection derived from it,
/// as the daemon would. Reads attach what was maintained and never maintain,
/// so a test or a tool that writes and then reads calls this to stand in for
/// the worker.
pub fn carry_facts<S>(pile: &mut S, source: Collection<SimpleArchive>, signer: &SigningKey)
where
    S: Store + AsyncBlobStoreAcquire + Send,
{
    drop(pollster::block_on(maintain_downstream(pile, source, signer)).unwrap());
}

/// The realizer for every encoding a faculty binary carries: the core's own
/// attachments and the search crate's BM25 index, through their canonical
/// mappings. A collection whose descriptor names a mapping this binary does
/// not know, such as the Files semantic index without its model or the
/// Archive's own BM25 mapping, is named in the report and left to whoever
/// registered it; nothing is guessed at.
///
/// A derived collection that could derive everything of the signer's except
/// some own foundations the mapping cannot represent counts as realized:
/// those foundations are its lag ([`tolerate_own_lag`]), kept in
/// [`Self::lagging`], and the pass goes on. An attached collection has no
/// such lag: what its mapping cannot represent is left to finer nodes, and a
/// reader reads what no attachment reaches from its own bytes.
#[derive(Clone, Debug, Default)]
pub struct FacultiesRealizer {
    /// Each derived target that left own foundations without a leaf, with
    /// each foundation and the reason, in the order the targets were visited.
    pub lagging: Vec<(CollectionHandle, Vec<(CollectionData, String)>)>,
}

impl<S> RealizeDerived<S> for FacultiesRealizer
where
    S: Store + AsyncBlobStoreAcquire + Send,
{
    async fn realize(
        &mut self,
        store: &mut S,
        derived: &Derived,
        signer: &SigningKey,
        upkeep: Upkeep,
    ) -> Result<Realized, CollectionRealizationError> {
        match CoreRealizer.realize(store, derived, signer, upkeep).await {
            Err(CollectionRealizationError::Unmappable { blocked }) => {
                self.lagging.push((derived.handle, blocked));
                Ok(Realized::Done)
            }
            realized => realized,
        }
    }

    async fn realize_attached(
        &mut self,
        store: &mut S,
        attached: &Attached,
        signer: &SigningKey,
        upkeep: Upkeep,
    ) -> Result<Realized, CollectionRealizationError> {
        if attached.representation == <PortableBM25Blob as MetaDescribe>::id() {
            realize_attached_as::<S, PortableBM25Blob>(store, attached, signer, upkeep).await
        } else {
            CoreRealizer
                .realize_attached(store, attached, signer, upkeep)
                .await
        }
    }
}

/// Attach the Succinct and Rank9 pair every faculty source is read through.
///
/// An attached descriptor is the source's handle and the mapping, with no
/// policy, so this registers the same pair on every host and whatever key
/// asks. Rank9 names the Succinct collection it reads.
pub fn fact_pair<S>(
    pile: &mut S,
    source: Collection<SimpleArchive>,
) -> Result<(
    Collection<SuccinctArchiveBlob>,
    Collection<Rank9AcceleratedSuccinctArchiveBlob>,
)>
where
    S: Store + AsyncBlobStoreAcquire + Send,
{
    let succinct = pile
        .attach::<SuccinctArchiveBlob>(source, ())
        .map_err(|error| anyhow!("attach the Succinct collection: {error}"))?;
    let rank9 = pile
        .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
        .map_err(|error| anyhow!("attach the Rank9 collection: {error}"))?;
    Ok((succinct, rank9))
}

/// Attach the parent's current frontier into an attached collection once if
/// nothing lists it yet.
///
/// An attached collection joins a store's listing with its first MAP, and
/// until then no pass over what is attached to its parent can find it. So
/// whoever registers one attaches the parent's frontier right then; from
/// then on every write's [`ensure_downstream`] finds it. Costs one listing
/// when the collection is already listed, which is every time but the first.
/// The store must be opened as `signer`, whose MAPs are the only ones
/// believed.
pub async fn seed_attached<S, T>(
    pile: &mut S,
    target: Collection<T>,
    signer: &SigningKey,
) -> Result<()>
where
    S: Store + AsyncBlobStoreAcquire + Send,
    T: CollectionAttachment,
    Handle<T>: InlineEncoding,
{
    if listed(pile, target.handle())? {
        return Ok(());
    }
    match pile.ensure_attached(target, signer).await {
        // A key that is not the store's host attaches nothing; the host's
        // next pass seeds it.
        Ok(_) | Err(CollectionRealizationError::HostMismatch { .. }) => Ok(()),
        Err(error) => Err(anyhow!("seed a newly attached collection: {error}")),
    }
}

/// Whether the store lists `collection` yet: it has a record of its own.
fn listed<S>(pile: &mut S, collection: CollectionHandle) -> Result<bool>
where
    S: Store,
{
    Ok(pile
        .snapshot()
        .context("freeze the store for its collection listing")?
        .collections()
        .map_err(|error| anyhow!("list the store's collections: {error}"))?
        .contains(&collection))
}

/// [`seed_attached`] for the fact pair, the Succinct attachments first.
async fn seed_fact_pair<S>(
    pile: &mut S,
    succinct: Collection<SuccinctArchiveBlob>,
    rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
    signer: &SigningKey,
) -> Result<()>
where
    S: Store + AsyncBlobStoreAcquire + Send,
{
    seed_attached(pile, succinct, signer).await?;
    seed_attached(pile, rank9, signer).await
}

/// What a write does after its commit into `source`: attach the Succinct and
/// Rank9 pair to it, seed them if the store does not list them yet, then
/// attach the source's current frontier into every collection attached to
/// it and derive the signer's own missing leaves into every collection
/// derived from it, so what the signer wrote is readable through each of
/// them. No merge is published; the maintenance daemon carries the source
/// later. The store must be opened as `signer`: only its MAPs are believed.
///
/// A commit whose attachment could not be built -- its mapping refused it,
/// or a dependency is not here -- is still readable: [`FactRead::read_facts`]
/// reads it from its own bytes. So is every commit of a writer that is not
/// the store's host: such a key attaches nothing, and the write is done
/// once its commit is. An attached collection whose upkeep fails is named
/// in the report's `failed_attached`, and the others are still attached.
pub async fn ensure_downstream<S>(
    pile: &mut S,
    source: Collection<SimpleArchive>,
    signer: &SigningKey,
) -> Result<UpkeepReport>
where
    S: Store + AsyncBlobStoreAcquire + Send,
{
    let (succinct, rank9) = fact_pair(pile, source)?;
    seed_fact_pair(pile, succinct, rank9, signer).await?;
    let mut realizer = FacultiesRealizer::default();
    match core_ensure_downstream(pile, source.handle(), signer, &mut realizer).await {
        Err(CollectionRealizationError::HostMismatch { .. }) => Ok(UpkeepReport::default()),
        result => result
            .map_err(|error| anyhow!("ensure the collections downstream of the source: {error}")),
    }
}

/// Carry `source`, attach its new frontier into every collection attached to
/// it, and derive every foundation left without a usable leaf into every
/// collection derived from it, carrying each one's own leaves, as the
/// maintenance daemon does.
/// The pair is attached and seeded first, like [`ensure_downstream`]. An own
/// foundation a derived view cannot derive is that view's lag and the pass
/// goes on, as in [`FacultiesRealizer`].
pub async fn maintain_downstream<S>(
    pile: &mut S,
    source: Collection<SimpleArchive>,
    signer: &SigningKey,
) -> Result<UpkeepReport>
where
    S: Store + AsyncBlobStoreAcquire + Send,
{
    let (succinct, rank9) = fact_pair(pile, source)?;
    seed_fact_pair(pile, succinct, rank9, signer).await?;
    let mut realizer = FacultiesRealizer::default();
    core_maintain_downstream(pile, source.handle(), signer, &mut realizer)
        .await
        .map_err(|error| anyhow!("maintain the collections downstream of the source: {error}"))
}
