//! Collection-native Archive runtime over the V4 descriptor-handle calculus.
//!
//! Archive authorship has one durable Ed25519 signer and one fixed canonical
//! SimpleArchive-union descriptor. Imports stage independently derivable source
//! fragments which contribute new evidence and cross exactly one signed COMMIT
//! visibility edge per publication. Reads snapshot that same collection;
//! there is no Repository branch, CAS head, sidecar registry, or fallback
//! identity.

#[cfg(test)]
use crate::storage::FactView;
use std::sync::Arc;
use std::collections::{BTreeMap, BTreeSet};
use triblespace::core::collection::AttachedSnapshot;

use anybytes::Bytes;
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::succinctarchive::{
    OrderedUniverse, Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchive, SuccinctArchiveBlob,
};
use triblespace::core::blob::encodings::{simplearchive::SimpleArchive, UnknownBlob};
use triblespace::core::blob::Blob;
use triblespace::core::collection::{
    Collection, CollectionCommit, CollectionData, CollectionSnapshotExt, CollectionStoreExt,
};
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::{
    BlobStoreGet, BlobStorePut, SnapshotSource, StorageClose, Store, StoreRead,
};
use triblespace::core::repo::async_store::{AsyncBlobStoreAcquire, AsyncBlobStoreGet};
use triblespace::prelude::blobencodings::RawBytes;
use triblespace::prelude::inlineencodings::Handle;
use triblespace::prelude::*;
use triblespace_search::portable_bm25::PortableBM25Blob;

use crate::archive_bm25;
use crate::blockdag;
use crate::schemas::blockdag as schema;
#[cfg(test)]
use crate::storage::{load_signer, open_pile_strict, open_pile_strict_as};
use crate::storage::{FactArchive, FactLag, FacultyStore};

#[cfg(test)]
use crate::collection_names::open_configured;
#[cfg(test)]
use triblespace::core::collection::{
    CollectionAttachment, CollectionMap, CollectionRecord, CollectionStore,
};
#[cfg(test)]
use triblespace::core::repo::BlobStoreMeta;

type RawHandle = Inline<Handle<RawBytes>>;

/// Stage Archive fragments for commit-last publication.
///
/// Supplied facts remain open-world relations, including opaque ids and further
/// annotations. Publication does not require a closed-world catalog decode.
pub struct ArchiveImportWriter<P = FacultyStore> {
    pile: P,
    collection: Collection<SimpleArchive>,
    signer: SigningKey,
    current: FactArchive,
    delta: Fragment,
    runtime: Arc<tokio::runtime::Runtime>,
    /// Commits published since this writer last carried its source.
    uncarried: usize,
}

/// Commits a writer publishes between two carries of its own source.
///
/// Every commit is a frontier node of its own until a carry joins it, and
/// each commit's attachment pass, and each read of the facts back, costs
/// more the wider the frontier, so a writer that never carried paid more for
/// every commit than for the one before and left the whole carry to the next
/// process to open the pile. The carry is the one
/// [`ArchiveImportWriter::prepare`] runs at open.
///
/// Sixteen is measured, not derived (sky, shared GB10; one process importing
/// hard-linked Codex rollouts into a fresh pile; mean wall time per
/// rollout). Over 1,000 raw-only rollouts: 8.7 ms at 4, 8.6-9.3 at 8,
/// 8.8-9.3 at 16, 10.2-10.3 at 32, 12.0 at 64, against 38-39 ms for a writer
/// that never carries. Over 400 rollouts of 269 projections each: about 150
/// ms at 8 (outside two stretches of interference), 148.5 at 16, 153.0 at
/// 32, against 278-330 ms. Below the optimum a carry pass costs more than the
/// frontier it removes; above it the frontier costs more. The optimum is
/// flat from 4 to 16; sixteen is its upper edge, which leaves room for a
/// pass that costs more on a larger pile. The sweep ran on fresh piles that
/// never held more than 1,000 commits, with the facts read back whole after
/// every commit, which [`ArchiveImportWriter::commit_unit`] now does once
/// per carry.
///
/// The sweep weighed wall time only; the cadence also costs pile bytes.
/// Each carry attaches every node it leaves on the frontier, merged nodes
/// included, and a later carry of the same writer joins most tier-one merged
/// nodes into a tier-two one. Their Succinct and Rank9 attachments are not
/// read again, and they stay in the pile until a compaction. A writer that
/// never carried left its merging to the next open, which attaches only the
/// frontier it ends with. Measured in review (sky, shared GB10): one writer
/// importing 3,000 synthetic Claude Code sessions into a fresh pile left 311
/// attached merged nodes that a later carry consumed, whose attachments hold
/// 102.8 MB of the pile's 1,171.5 MB of blob bytes (8.8%), against 74.3 MB
/// attached to the 18 nodes of the final frontier. Two writers over 1,982
/// real Claude Code sessions: 48.6 MB of 573.8 MB (8.5%).
const CARRY_EVERY: usize = 16;

#[cfg(test)]
thread_local! {
    /// How often [`ArchiveImportWriter::open`] ran on this thread, so a test
    /// can pin how many times one command opens the pile.
    pub(crate) static OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How often a writer on this thread read its known facts back whole.
    static READ_BACKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl ArchiveImportWriter {
    /// Open a synchronous import session. Async callers run the complete
    /// open/stage/finish lifetime on a blocking worker, not inside their runtime.
    pub fn open(
        pile_path: &std::path::Path,
        key_path: Option<&std::path::Path>,
    ) -> Result<Self> {
        #[cfg(test)]
        OPENS.with(|opens| opens.set(opens.get() + 1));
        let signer = crate::storage::load_signer(pile_path, key_path)?;
        let runtime = Arc::new(crate::storage::runtime()?);
        let mut pile = crate::storage::open_store_as(pile_path, signer.verifying_key())?;
        let result = Self::prepare(&mut pile, &signer, &runtime);
        match result {
            Ok((collection, current)) => {
                let mut writer = Self {
                    pile,
                    collection,
                    signer,
                    current,
                    delta: Fragment::empty(),
                    runtime,
                    uncarried: 0,
                };
                if let Err(error) = writer.stage_fragment(blockdag::vocabulary_fragment()) {
                    return close_pile(
                        writer.pile,
                        Err(error),
                        "closing Archive pile after vocabulary staging failed",
                    );
                }
                Ok(writer)
            }
            Err(error) => close_pile(
                pile,
                Err(error),
                "closing Archive pile after failed open also failed",
            ),
        }
    }
}

impl<P> ArchiveImportWriter<P>
where
    P: Store + AsyncBlobStoreAcquire + Send,
    P::Snapshot: AsyncBlobStoreGet,
{
    /// Borrow a caller-owned store without taking its local-backend guard.
    /// The caller keeps the transport alive until the last commit completes.
    pub fn from_store(
        mut pile: P,
        signer: &SigningKey,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self> {
        let (source, current) = Self::prepare(&mut pile, signer, &runtime)?;
        let mut writer = Self {
            pile,
            collection: source,
            signer: signer.clone(),
            current,
            delta: Fragment::empty(),
            runtime,
            uncarried: 0,
        };
        writer.stage_fragment(blockdag::vocabulary_fragment())?;
        Ok(writer)
    }

    fn prepare(
        pile: &mut P,
        signer: &SigningKey,
        runtime: &Arc<tokio::runtime::Runtime>,
    ) -> Result<(Collection<SimpleArchive>, FactArchive)> {
        let source = crate::collection_names::open_configured_acquiring(
            pile,
            schema::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            runtime,
        )?;
        let (succinct, rank9) = crate::storage::fact_pair(pile, source)?;
        runtime.block_on(async {
            crate::storage::tolerate_own_lag(pile.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(pile.maintain_attached(rank9, signer).await)
        }).context("maintain Archive import facts")?;
        let reader = crate::storage::AcquiringReader::new(pile.snapshot()?, runtime.clone());
        let current = crate::storage::acquire_facts(&reader, rank9)
            .context("read Archive import facts")?;
        Ok((source, current))
    }

    pub fn stage_fragment(&mut self, fragment: Fragment) -> Result<()> {
        // A Fragment is the independently derivable source unit. A wholly
        // known candidate is an idempotent replay and can be skipped. Once it
        // contributes even one new fact, retain its complete closure in this
        // COMMIT element—including facts already present in older elements.
        // Set union makes that duplication semantically free, and a mapping
        // that reads a whole block (the Archive block BM25) then finds it in
        // one commit, without depending on an implicit merge with historical
        // commits. The fragment is kept as given: nothing here checks that it
        // holds whole blocks; the importers hand it whole ones.
        let (_, facts, metafacts, blobs) = fragment.into_parts();
        if facts.iter().all(|fact| {
            self.delta.facts().contains(fact) || fact_archive_contains(&self.current, fact)
        }) {
            return Ok(());
        }

        // Embedded payloads can dominate an import's resident memory. Append
        // each already-constructed content-addressed dependency immediately,
        // keeping payload bytes out of the long-lived logical delta. They
        // remain semantically unreachable until a signed COMMIT names the
        // facts which reference them.
        let embedded = embedded_blobs(blobs);
        stage_embedded_blobs(&mut self.pile, embedded)?;

        // Only the lightweight logical delta remains resident between source
        // fragments. Data and metadata archives are constructed once at the
        // final publication boundary.
        self.delta += Fragment::from_parts(facts, metafacts, Default::default());
        Ok(())
    }

    pub fn delta_len(&self) -> usize {
        self.delta.facts().len()
    }

    /// Publish ONE rollout's staged delta and keep the pile open.
    ///
    /// Atomicity is per rollout; it was never per PROCESS. Opening the pile
    /// costs ~9.3 s on the live 44 GB pile against ~1 s of actual projection
    /// for a 478 KB rollout (measured 2026-09-01), so paying it once per file
    /// put a full Codex backfill — 3,161 refused rollouts — at about 8.2 hours
    /// of pure pile-opening before any work. JP: "we can commit multiple times
    /// in the same process right xD?" Yes. Same one-signed-COMMIT-per-rollout
    /// guarantee, one open.
    ///
    /// `current` MUST absorb what was just published, or the next rollout's
    /// idempotence check would re-stage facts this commit already carries —
    /// which is the whole reason resumed Codex rollouts (they replay large
    /// parent prefixes) are cheap to ingest in sequence rather than expensive.
    ///
    /// Between carries the commit's own Rank9 attachment, which its
    /// attachment pass has just built, joins `current` as one more zero-copy
    /// segment, so `current` gains at most [`CARRY_EVERY`] segments before a
    /// carry. A carry reads `current` back whole from the frontier's Rank9
    /// attachments, as the next open would read it: one segment per frontier
    /// node. That read selects a cover over the frontier, and its cost grows
    /// with every commit the collection holds, not with the frontier, so it
    /// runs once per carry rather than once per commit. A commit left without
    /// a usable attachment here (a store whose host is another key attaches
    /// nothing) is read back whole the same way. A foundation whose bytes are
    /// not here is left out, never fetched; that can only make a later
    /// fragment stage facts again, which the union absorbs.
    pub fn commit_unit(&mut self) -> Result<Option<CollectionCommit>> {
        let Some(commit) = self.publish()? else {
            return Ok(None);
        };
        let carry = self.uncarried >= CARRY_EVERY;
        if carry {
            self.carry()?;
        } else {
            self.ensure_downstream()?;
        }
        let (_, rank9) = crate::storage::fact_pair(&mut self.pile, self.collection)?;
        let snapshot = self.pile.snapshot()?;
        if !carry {
            if let Some(segment) = attached_node_facts(&snapshot, rank9, commit.data())? {
                self.current = self.current.with_segments([segment]);
                return Ok(Some(commit));
            }
        }
        #[cfg(test)]
        READ_BACKS.with(|reads| reads.set(reads.get() + 1));
        self.current = crate::storage::FactRead::read_facts(&snapshot, rank9)
            .context("read back the published Archive facts")?;
        Ok(Some(commit))
    }

    /// Commit the staged delta, if any, as one signed COMMIT.
    fn publish(&mut self) -> Result<Option<CollectionCommit>> {
        if self.delta.facts().is_empty() {
            return Ok(None);
        }
        let fragment = std::mem::replace(&mut self.delta, Fragment::empty());
        let commit = self
            .pile
            .commit(self.collection, &self.signer, fragment)
            .context("commit authored Archive projection unit")?;
        self.uncarried += 1;
        Ok(Some(commit))
    }

    /// Attach the source's current frontier into every collection attached
    /// to it, so what was just committed is readable through them.
    fn ensure_downstream(&mut self) -> Result<()> {
        drop(
            self.runtime.block_on(crate::storage::ensure_downstream(
                &mut self.pile,
                self.collection,
                &self.signer,
            ))
            .context(
                "Archive projection unit was committed, but ensuring its derived views failed",
            )?,
        );
        Ok(())
    }

    /// Carry the source to its fixed point -- every tier holding eight nodes
    /// joined by one MERGE -- and attach the frontier the carry leaves, so
    /// neither this writer's next commit nor the next opener inherits the
    /// commits made since the last carry. A store whose host is not this
    /// writer's key believes none of its merges and is left uncarried, as
    /// at open.
    fn carry(&mut self) -> Result<()> {
        self.runtime
            .block_on(async {
                crate::storage::tolerate_own_lag(
                    self.pile.maintain(self.collection, &self.signer).await,
                )
            })
            .context("Archive projection units were committed, but carrying them failed")?;
        self.uncarried = 0;
        self.ensure_downstream()
    }

    /// Publish what is still staged and carry what this writer committed,
    /// before it closes: the carry's attachment pass also attaches the last
    /// commit, and nothing is read back.
    fn settle(&mut self) -> Result<Option<CollectionCommit>> {
        let commit = self.publish()?;
        if self.uncarried > 0 {
            self.carry()?;
        }
        Ok(commit)
    }
}

impl ArchiveImportWriter {
    /// Close the pile, publishing any still-staged delta and carrying what
    /// this writer committed first.
    pub fn close<T>(mut self, surrounding: Result<T>) -> Result<T> {
        let result = surrounding.and_then(|value| {
            self.settle()?;
            Ok(value)
        });
        close_pile(
            self.pile,
            result,
            "closing Archive pile after failure also failed",
        )
    }

    pub fn finish<T>(mut self, surrounding: Result<T>) -> Result<(T, Option<CollectionCommit>)> {
        let result = surrounding.and_then(|value| {
            let commit = self.settle()?;
            Ok((value, commit))
        });
        close_pile(
            self.pile,
            result,
            "closing Archive pile after failure also failed",
        )
    }
}

/// Write one constructed streaming payload batch into content-addressed storage.
///
/// A failed put abandons only this batch. Replaying the fragment repeats the
/// same idempotent content-addressed writes; the later signed collection commit
/// is the sole semantic publication edge.
fn stage_embedded_blobs<S>(store: &mut S, embedded: Vec<Blob<UnknownBlob>>) -> Result<()>
where
    S: BlobStorePut,
{
    for blob in embedded {
        store
            .put::<UnknownBlob, _>(blob)
            .context("stage Archive embedded blob")?;
    }
    Ok(())
}

/// Extract the content-addressed attachments already constructed by a Fragment.
///
/// `MemoryBlobStore` and `Blob` uphold their cached-handle invariants. Rehashing
/// bytes produced in this process would only repeat work at the publication
/// boundary; untrusted bytes are validated according to their encoding when
/// they are interpreted.
fn embedded_blobs(mut blobs: triblespace::core::blob::MemoryBlobStore) -> Vec<Blob<UnknownBlob>> {
    let reader = blobs
        .snapshot()
        .expect("MemoryBlobStore reader creation is infallible");
    let mut embedded: Vec<_> = reader.iter().collect();
    embedded.sort_unstable_by_key(|(store_key, _)| store_key.raw);

    embedded.into_iter().map(|(_, blob)| blob).collect()
}

/// One node's facts through the Rank9 attachment a believed MAP gives it in
/// `snapshot`: one zero-copy segment, or `None` when no attachment of it is
/// here together with the Succinct archive it accelerates.
fn attached_node_facts<R: StoreRead>(
    snapshot: &R,
    rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
    node: CollectionData,
) -> Result<Option<SuccinctArchive<OrderedUniverse>>> {
    let coverage = snapshot
        .coverage(&BTreeSet::from([rank9.handle()]))
        .map_err(|error| anyhow!("read the Rank9 attachments: {error}"))?;
    for attachment in coverage.attachments(rank9.handle(), node) {
        let root = Handle::<Rank9AcceleratedSuccinctArchiveBlob>::from_hash(attachment);
        if !resident(snapshot, root)? {
            continue;
        }
        let root: Blob<Rank9AcceleratedSuccinctArchiveBlob> = snapshot
            .get(root)
            .map_err(|error| anyhow!("read a Rank9 attachment: {error}"))?;
        let source = Rank9AcceleratedSuccinctArchiveBlob::source_handle(&root)
            .map_err(|error| anyhow!("read a Rank9 attachment's source: {error}"))?;
        if !resident(snapshot, source)? {
            continue;
        }
        let raw: Blob<SuccinctArchiveBlob> = snapshot
            .get(source)
            .map_err(|error| anyhow!("read a Rank9 attachment's Succinct archive: {error}"))?;
        return SuccinctArchive::from_accelerated_parts(raw, root)
            .map(Some)
            .map_err(|error| anyhow!("attach a Rank9 attachment: {error}"));
    }
    Ok(None)
}

fn resident<R: StoreRead, T: BlobEncoding + 'static>(
    snapshot: &R,
    handle: Inline<Handle<T>>,
) -> Result<bool>
where
    Handle<T>: InlineEncoding,
{
    Ok(snapshot
        .metadata(handle)
        .map_err(|error| anyhow!("inspect blob residency: {error}"))?
        .is_some())
}

/// Exact membership without rebuilding an in-memory `TribleSet` over the
/// maintained shard union.
fn fact_archive_contains(facts: &FactArchive, fact: &Trible) -> bool {
    exists!(facts.pattern(
        inlineencodings::GenId::inline_from(*fact.e()),
        inlineencodings::GenId::inline_from(*fact.a()),
        *fact.v::<UnknownInline>(),
    ))
}

fn close_pile<T>(pile: impl StorageClose, result: Result<T>, failure_context: &str) -> Result<T> {
    match (result, pile.close()) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(close_error)) => Err(anyhow!("close Archive pile: {close_error}")),
        (Err(error), Err(close_error)) => {
            Err(error.context(format!("{failure_context} also failed: {close_error}")))
        }
    }
}

/// Ensure the Archive's maintained fact representation and return the ordinary
/// collection observation. This boundary performs storage work, not domain
/// decoding: consumers choose their own typed queries over `view::<FactArchive>()`.
pub async fn ensure_local(
    pile_path: &std::path::Path,
    key_path: Option<&std::path::Path>,
) -> Result<AttachedSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>> {
    ensure_local_with_storage(&crate::storage::Storage::new(
        pile_path.to_owned(),
        key_path.map(std::path::Path::to_owned),
    ))
}

pub fn ensure_local_with_storage(
    storage: &crate::storage::Storage,
) -> Result<AttachedSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>> {
    storage.with_store(|store, signer, runtime| {
        let source = crate::collection_names::open_configured_acquiring(
            store, schema::DEFAULT_SCOPE_ID, signer.verifying_key(), runtime,
        )?;
        let mut local = store.store();
        let pile = &mut *local;
        pollster::block_on(ensure_facts(pile, source, signer))
    })
}

/// Foreground observation. The caller retains its `Storage::scope` through
/// its last payload read; the observation itself does not extend store life.
pub(crate) fn ensure_acquiring_with_storage(
    storage: &crate::storage::Storage,
) -> Result<AttachedSnapshot<
    crate::storage::AcquiringReader<crate::storage::FacultySnapshot>,
    Rank9AcceleratedSuccinctArchiveBlob,
>> {
    storage.with_store(|store, signer, runtime| {
        let source = crate::collection_names::open_configured_acquiring(
            store, schema::DEFAULT_SCOPE_ID, signer.verifying_key(), runtime,
        )?;
        let (succinct, rank9) = crate::storage::fact_pair(store, source)?;
        runtime.block_on(async {
            crate::storage::tolerate_own_lag(store.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(store.maintain_attached(rank9, signer).await)?;
            Ok::<_, anyhow::Error>(())
        })?;
        crate::storage::AcquiringReader::new(store.snapshot()?, runtime.clone())
            .attached_acquiring(rank9).context("attach frozen Archive facts")
    })
}

/// The Archive's Succinct and Rank9 fact pair over `source`.
fn fact_views(
    pile: &mut Pile,
    source: Collection<SimpleArchive>,
) -> Result<(
    Collection<SuccinctArchiveBlob>,
    Collection<Rank9AcceleratedSuccinctArchiveBlob>,
)> {
    crate::storage::fact_pair(pile, source).context("register Archive fact collections")
}

/// Carry the Archive source and attach what the carry leaves into the fact
/// pair, then read Rank9. A commit whose bytes are not here is not fetched;
/// it is the pair's residual and is counted as its lag.
async fn ensure_facts(
    pile: &mut Pile,
    source: Collection<SimpleArchive>,
    signer: &SigningKey,
) -> Result<AttachedSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>> {
    let (succinct, rank9) = fact_views(pile, source)?;
    crate::storage::tolerate_own_lag(pile.maintain_attached(succinct, signer).await)
        .context("maintain Succinct Archive fact collection")?;
    crate::storage::tolerate_own_lag(pile.maintain_attached(rank9, signer).await)
        .context("maintain Rank9 Archive fact collection")?;
    pile.snapshot()
        .context("freeze maintained Archive facts")?
        .attached(rank9)
        .context("attach Archive fact collection")
}

/// Accelerated-Succinct index summary. Source membership is measured in
/// distinct commit payloads the snapshot can read, never in the number of
/// attestations over them; the lag says how many source foundations no
/// attachment of each collection of the fact pair reaches yet, a commit whose
/// payload is not here included.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SuccinctIndexReport {
    pub source_elements: usize,
    pub lag: FactLag,
    pub source_collection: Inline<Handle<SimpleArchive>>,
    pub target_collection: Inline<Handle<SimpleArchive>>,
}

pub fn ensure_succinct_index(
    pile_path: &std::path::Path,
    key_path: Option<&std::path::Path>,
) -> Result<SuccinctIndexReport> {
    ensure_succinct_index_with_storage(&crate::storage::Storage::new(
        pile_path.to_owned(),
        key_path.map(std::path::Path::to_owned),
    ))
}

pub fn ensure_succinct_index_with_storage(
    storage: &crate::storage::Storage,
) -> Result<SuccinctIndexReport> {
    storage.with_store(|store, signer, runtime| {
        let source = crate::collection_names::open_configured_acquiring(
            store, schema::DEFAULT_SCOPE_ID, signer.verifying_key(), runtime,
        )?;
        let (succinct, rank9) = crate::storage::fact_pair(store, source)?;
        runtime.block_on(async {
            crate::storage::tolerate_own_lag(store.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(store.maintain_attached(rank9, signer).await)?;
            Ok::<_, anyhow::Error>(())
        })?;
        let snapshot = crate::storage::AcquiringReader::new(store.snapshot()?, runtime.clone());
        crate::storage::acquire_facts(&snapshot, rank9)?;
        let source_elements = snapshot
            .collection(source)
            .context("attach Archive source")?
            .support()
            .context("resolve Archive source support")?
            .len();
        Ok(SuccinctIndexReport {
            source_elements,
            lag: FactLag::of(&snapshot, source, succinct, rank9)?,
            source_collection: source.handle(),
            target_collection: rank9.handle(),
        })
    })
}

/// Archive BM25 index summary: the source commits the snapshot can read, how
/// many source foundations no BM25 attachment reaches yet, and how many
/// segments its attached cover has.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bm25IndexReport {
    pub source_elements: usize,
    pub lagging: usize,
    pub cover_segments: usize,
    pub source_collection: Inline<Handle<SimpleArchive>>,
    pub target_collection: Inline<Handle<SimpleArchive>>,
}

struct EnsuredBm25 {
    report: Bm25IndexReport,
    index: archive_bm25::ArchiveBM25View,
}

/// Attach the BM25 index to the Archive source.
fn bm25_target(
    pile: &mut Pile,
    source: Collection<SimpleArchive>,
) -> Result<Collection<PortableBM25Blob>> {
    pile.attach_with(source, archive_bm25::ArchiveBlockTextBm25Mapping)
        .context("attach the Archive BM25 index")
}

/// Carry the Archive source and attach what the carry leaves into the BM25
/// index, then read it back from the maintained snapshot together with how
/// many source foundations no attachment reaches there.
/// Provenance records are neither part of this value nor required to replay it.
async fn ensure_bm25(
    pile: &mut Pile,
    source: Collection<SimpleArchive>,
    signer: &SigningKey,
) -> Result<EnsuredBm25> {
    let target = bm25_target(pile, source)?;
    crate::storage::tolerate_own_lag(
        pile.maintain_attached_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(target, signer)
            .await,
    )
    .context("maintain Archive BM25 cover")?;
    let maintained = pile
        .snapshot()
        .context("freeze maintained Archive BM25 cover")?;
    let attached = maintained
        .attached(target)
        .context("attach Archive BM25 cover")?;
    let source_view = maintained
        .collection(source)
        .context("attach Archive source")?;
    let lagging = attached.residual().len();
    let index = attached
        .view::<archive_bm25::ArchiveBM25View>()
        .context("read Archive BM25 cover")?;
    Ok(EnsuredBm25 {
        report: Bm25IndexReport {
            source_elements: source_view
                .support()
                .context("resolve Archive source support")?
                .len(),
            lagging,
            cover_segments: attached.cover().len(),
            source_collection: source.handle(),
            target_collection: target.handle(),
        },
        index,
    })
}

pub fn ensure_bm25_index(
    pile_path: &std::path::Path,
    key_path: Option<&std::path::Path>,
) -> Result<Bm25IndexReport> {
    ensure_bm25_index_with_storage(&crate::storage::Storage::new(
        pile_path.to_owned(),
        key_path.map(std::path::Path::to_owned),
    ))
}

pub fn ensure_bm25_index_with_storage(
    storage: &crate::storage::Storage,
) -> Result<Bm25IndexReport> {
    storage.with_store(|store, signer, runtime| {
        let source = crate::collection_names::open_configured_acquiring(
            store, schema::DEFAULT_SCOPE_ID, signer.verifying_key(), runtime,
        )?;
        let target = store.attach_with(source, archive_bm25::ArchiveBlockTextBm25Mapping)?;
        let (succinct, rank9) = crate::storage::fact_pair(store, source)?;
        runtime.block_on(async {
            crate::storage::tolerate_own_lag(store.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(store.maintain_attached(rank9, signer).await)?;
            crate::storage::tolerate_own_lag(
                store.maintain_attached_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(
                    target, signer,
                ).await,
            )?;
            Ok::<_, anyhow::Error>(())
        })?;
        let snapshot = crate::storage::AcquiringReader::new(store.snapshot()?, runtime.clone());
        let attached = snapshot.attached_acquiring(target)?;
        let read = attached.read_with_acquiring::<
            archive_bm25::ArchiveBlockTextBm25Mapping, archive_bm25::ArchiveBM25View,
        >()?;
        Ok(Bm25IndexReport {
            source_elements: snapshot.collection(source)?.support()?.len(),
            lagging: read.unread().len(),
            cover_segments: attached.cover().len(),
            source_collection: source.handle(),
            target_collection: target.handle(),
        })
    })
}

/// How far the two Archive search views lag the source in the one snapshot
/// both were attached from. Each view is attached on its own, so the two may
/// lag differently; a search reads what is present.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveSearchLag {
    /// The fact pair the results are joined against.
    pub facts: FactLag,
    /// Source foundations no BM25 attachment reaches yet.
    pub index: usize,
}

impl ArchiveSearchLag {
    /// Whether both views have caught up with every source commit the
    /// snapshot admits.
    pub const fn is_current(self) -> bool {
        self.facts.is_current() && self.index == 0
    }
}

/// Prepare the fact and search views from one snapshot. Both are attached
/// to the one Archive source and both are maintained here, then read from
/// one snapshot. They need not stand for the same source foundations: a node
/// one mapping cannot represent is left to finer nodes there and not in the
/// other, and a commit landing between the two passes reaches one view
/// first. The returned lag says how far each is behind; nothing waits for
/// them to agree. The returned collection snapshot exposes the usual fact
/// view and blob reader; callers explicitly prepare a BM25 query and join
/// document ids to whatever facts their operation needs, and a document
/// whose facts have not arrived yet simply joins to nothing. Attaching the
/// search cover does not serialize its union.
pub async fn ensure_search_local(
    pile_path: &std::path::Path,
    key_path: Option<&std::path::Path>,
) -> Result<(
    AttachedSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>,
    archive_bm25::ArchiveBM25View,
    ArchiveSearchLag,
)> {
    ensure_search_local_with_storage(&crate::storage::Storage::new(
        pile_path.to_owned(),
        key_path.map(std::path::Path::to_owned),
    ))
}

pub fn ensure_search_local_with_storage(
    storage: &crate::storage::Storage,
) -> Result<(
    AttachedSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>,
    archive_bm25::ArchiveBM25View,
    ArchiveSearchLag,
)> {
    storage.with_store(|store, signer, runtime| {
        let source = crate::collection_names::open_configured_acquiring(
            store, schema::DEFAULT_SCOPE_ID, signer.verifying_key(), runtime,
        )?;
        let mut local = store.store();
        let pile = &mut *local;
        pollster::block_on(async {
            let target = bm25_target(pile, source)?;
            let (succinct, rank9) = fact_views(pile, source)?;
            drop(ensure_facts(pile, source, signer).await?);
            crate::storage::tolerate_own_lag(
                pile.maintain_attached_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(
                    target, signer,
                )
                .await,
            )
            .context("maintain Archive BM25 cover")?;
            // One snapshot for both views: search maintenance may have
            // acquired referenced text payloads, and the facts are read
            // through that same reader.
            let after = pile
                .snapshot()
                .context("freeze prepared Archive search snapshot")?;
            let facts = after
                .attached(rank9)
                .context("attach Archive search facts")?;
            let search = after
                .attached(target)
                .context("attach Archive BM25 cover")?;
            let lag = ArchiveSearchLag {
                facts: FactLag::of(&after, source, succinct, rank9)?,
                index: search.residual().len(),
            };
            let index = search
                .view::<archive_bm25::ArchiveBM25View>()
                .context("read Archive BM25 cover")?;
            Ok((facts, index, lag))
        })
    })
}

/// Acquire a query's fixed Archive fact and BM25 observations. Both views
/// select their support at the same watermark; exact-byte arrival below does
/// not reselect either of them. The surrounding operation owns the store.
pub(crate) fn ensure_search_acquiring_with_storage(
    storage: &crate::storage::Storage,
) -> Result<(
    AttachedSnapshot<
        crate::storage::AcquiringReader<crate::storage::FacultySnapshot>,
        Rank9AcceleratedSuccinctArchiveBlob,
    >,
    archive_bm25::ArchiveBM25View,
    ArchiveSearchLag,
)> {
    storage.with_store(|store, signer, runtime| {
        let source = crate::collection_names::open_configured_acquiring(
            store, schema::DEFAULT_SCOPE_ID, signer.verifying_key(), runtime,
        )?;
        let target = store.attach_with(source, archive_bm25::ArchiveBlockTextBm25Mapping)?;
        let (succinct, rank9) = crate::storage::fact_pair(store, source)?;
        runtime.block_on(async {
            crate::storage::tolerate_own_lag(store.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(store.maintain_attached(rank9, signer).await)?;
            crate::storage::tolerate_own_lag(
                store.maintain_attached_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(
                    target, signer,
                ).await,
            )?;
            Ok::<_, anyhow::Error>(())
        })?;
        let after = crate::storage::AcquiringReader::new(store.snapshot()?, runtime.clone());
        let facts = after.attached_acquiring(rank9)?;
        let search = after.attached_acquiring(target)?;
        let lag = ArchiveSearchLag {
            facts: FactLag::of(&after, source, succinct, rank9)?,
            index: search.residual().len(),
        };
        let index = crate::storage::require_complete_attached_read(
            search.read_with_acquiring::<
                archive_bm25::ArchiveBlockTextBm25Mapping, archive_bm25::ArchiveBM25View,
            >()?,
        )?;
        Ok((facts, index, lag))
    })
}

/// Stream the byte geometry selected by one source snapshot. Only lightweight
/// chunk coordinates are sorted; each payload is fetched and hash-checked on
/// demand. Geometry errors affect this export, never ordinary archive reads.
///
/// The queried entity ids are opaque. Equal offset/handle rows deduplicate even
/// when different chunk ids witness them, and unrelated annotations are inert.
pub fn write_source_snapshot<P, R, W>(
    facts: &P,
    reader: &R,
    id: Id,
    destination: &mut W,
) -> Result<u128>
where
    P: TriblePattern,
    R: BlobStoreGet,
    W: std::io::Write,
{
    let lengths: BTreeSet<u128> = find!(
        length: u128,
        pattern!(facts, [{
            id @ metadata::tag: &schema::source_snapshot::KIND,
            schema::source_snapshot::byte_length: ?length
        }])
    )
    .collect();
    if lengths.is_empty() {
        bail!("Archive source snapshot {id:X} has no readable byte length");
    }
    let chunks: BTreeSet<(u128, RawHandle)> = find!(
        (offset: u128, bytes: RawHandle),
        pattern!(facts, [
            { id @ schema::source_snapshot::contains: _?chunk },
            { _?chunk @ schema::source_chunk::offset: ?offset,
                schema::source_chunk::bytes: ?bytes },
        ])
    )
    .collect();
    let mut written = 0u128;
    for (offset, handle) in chunks {
        if offset != written {
            bail!("Archive source snapshot {id:X} has a chunk at {offset}, expected {written}");
        }
        let bytes: Bytes = reader.get(handle).context("read Archive source chunk")?;
        destination
            .write_all(bytes.as_ref())
            .with_context(|| format!("write Archive source snapshot {id:X}"))?;
        written = written
            .checked_add(bytes.len() as u128)
            .ok_or_else(|| anyhow!("Archive source snapshot {id:X} length overflows u128"))?;
    }
    if !lengths.contains(&written) {
        bail!("Archive source snapshot {id:X} yielded {written} bytes, outside its stated lengths");
    }
    Ok(written)
}

/// Exact continuation point for the causal-temporal view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveTimelineCursor {
    AfterTime(i128),
    AfterBlock(Id),
}

/// One positioned identity, not a decoded domain object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveTimelineBlock {
    pub position: i128,
    pub block: Id,
}

impl ArchiveTimelineBlock {
    pub const fn cursor(&self) -> ArchiveTimelineCursor {
        ArchiveTimelineCursor::AfterBlock(self.block)
    }
}

/// Position canonical blocks in deterministic causal order. Timestamp
/// annotations may have several values: the earliest canonical timestamp wins,
/// falling back to the earliest source timestamp. Untimed nodes carry causal
/// position but emit nothing. Inclusion policy belongs to the caller after
/// positioning, so filtering cannot change cursor or predecessor semantics.
pub fn timeline_after<P>(
    facts: &P,
    cursor: ArchiveTimelineCursor,
) -> Result<Vec<ArchiveTimelineBlock>>
where
    P: TriblePattern,
{
    let blocks: BTreeSet<Id> = find!(
        block: Id,
        pattern!(facts, [{ ?block @ metadata::tag: &schema::block::KIND }])
    )
    .collect();
    let mut canonical_timestamps = BTreeMap::<Id, i128>::new();
    for (block, (lower, _)) in find!(
        (block: Id, timestamp: (i128, i128)),
        pattern!(facts, [{ ?block @ schema::block::timestamp: ?timestamp }])
    ) {
        canonical_timestamps
            .entry(block)
            .and_modify(|current| *current = (*current).min(lower))
            .or_insert(lower);
    }
    let mut receipt_timestamps = BTreeMap::<Id, i128>::new();
    for (block, (lower, _)) in find!(
        (block: Id, timestamp: (i128, i128)),
        pattern!(facts, [{
            _?projection @ schema::source_projection::projects_to: ?block,
            schema::source_projection::source_timestamp: ?timestamp
        }])
    ) {
        receipt_timestamps
            .entry(block)
            .and_modify(|current| *current = (*current).min(lower))
            .or_insert(lower);
    }
    let mut timestamps = BTreeMap::new();
    let mut predecessors = BTreeMap::<Id, BTreeSet<Id>>::new();
    let mut successors = BTreeMap::<Id, BTreeSet<Id>>::new();
    for block in &blocks {
        timestamps.insert(
            *block,
            canonical_timestamps
                .get(block)
                .or_else(|| receipt_timestamps.get(block))
                .copied(),
        );
        let previous: BTreeSet<_> = find!(
            predecessor: Id,
            pattern!(facts, [{ block @ schema::block::previous: ?predecessor }])
        )
        .collect();
        for predecessor in &previous {
            successors.entry(*predecessor).or_default().insert(*block);
        }
        predecessors.insert(*block, previous);
    }
    let mut remaining: BTreeMap<Id, usize> = predecessors
        .iter()
        .map(|(block, previous)| (*block, previous.len()))
        .collect();
    let mut inherited = BTreeMap::<Id, Option<i128>>::new();
    let mut ready_untimed = BTreeSet::new();
    let mut ready_timed = BTreeSet::new();
    for block in &blocks {
        inherited.insert(*block, None);
        if remaining[block] == 0 {
            match timestamps[block] {
                Some(position) => {
                    ready_timed.insert((position, *block));
                }
                None => {
                    ready_untimed.insert(*block);
                }
            }
        }
    }
    let mut positioned = Vec::new();
    let mut visited = 0usize;
    while visited < blocks.len() {
        let (block, position, emits) = if let Some(block) = ready_untimed.pop_first() {
            (block, inherited[&block], false)
        } else if let Some((position, block)) = ready_timed.pop_first() {
            (block, Some(position), true)
        } else {
            bail!("Archive timeline has an incomplete or cyclic predecessor graph");
        };
        visited += 1;
        if let Some(position) = position.filter(|_| emits) {
            positioned.push(ArchiveTimelineBlock { position, block });
        }
        for successor in successors.get(&block).into_iter().flatten() {
            if let Some(position) = position {
                let inherited = inherited
                    .get_mut(successor)
                    .expect("every successor belongs to the canonical block set");
                *inherited = Some(inherited.map_or(position, |current| current.max(position)));
            }
            let count = remaining
                .get_mut(successor)
                .expect("every successor has a dependency count");
            *count -= 1;
            if *count == 0 {
                let inherited = inherited[successor];
                match timestamps[successor] {
                    Some(timestamp) => {
                        ready_timed.insert((
                            inherited.map_or(timestamp, |value| value.max(timestamp)),
                            *successor,
                        ));
                    }
                    None => {
                        ready_untimed.insert(*successor);
                    }
                }
            }
        }
    }
    let start = match cursor {
        ArchiveTimelineCursor::AfterTime(position) => {
            positioned.partition_point(|candidate| candidate.position <= position)
        }
        ArchiveTimelineCursor::AfterBlock(anchor) => positioned
            .iter()
            .position(|candidate| candidate.block == anchor)
            .map(|index| index + 1)
            .ok_or_else(|| {
                anyhow!("Archive timeline cursor block {anchor:X} is absent or has no timestamp")
            })?,
    };
    Ok(positioned.into_iter().skip(start).collect())
}

#[cfg(test)]
mod tests {
    use crate::storage::FactRead;
    use std::convert::Infallible;

    /// Descriptor-local authority of the Archive fixture.
    fn test_authority(
        pile: &std::path::Path,
        key: &std::path::Path,
    ) -> ed25519_dalek::VerifyingKey {
        load_signer(pile, Some(key)).unwrap().verifying_key()
    }

    /// The Archive root these fixtures commit into.
    fn test_source(
        store: &mut Pile,
        pile: &std::path::Path,
        key: &std::path::Path,
    ) -> Collection<SimpleArchive> {
        crate::collection_names::open(store, schema::DEFAULT_SCOPE_ID, test_authority(pile, key))
            .unwrap()
    }

    /// The BM25 collection attached to that root.
    fn test_target(
        store: &mut Pile,
        source: Collection<SimpleArchive>,
    ) -> Collection<PortableBM25Blob> {
        store
            .attach_with(source, archive_bm25::ArchiveBlockTextBm25Mapping)
            .unwrap()
    }

    /// Initialize the durable signer used by the Archive fixture.
    fn initialize_archive_fixture(pile: &std::path::Path, key: &std::path::Path) -> SigningKey {
        initialize_signer(pile, Some(key)).unwrap()
    }

    use super::*;
    use crate::schemas::files as files_schema;
    use anybytes::View;
    use triblespace::prelude::blobencodings::UTF8String;
    use triblespace::prelude::inlineencodings::NsTAIInterval;
    use triblespace_search::tokens::hash_tokens;

    fn projection_ids(facts: &FactArchive) -> Vec<Id> {
        find!(
            projection: Id,
            pattern!(facts, [{ ?projection @ metadata::tag: &schema::source_projection::KIND }])
        )
        .collect()
    }
    use crate::storage::discovered_records;
    use crate::storage::initialize_signer;
    use ed25519_dalek::SigningKey;
    use hifitime::Epoch;
    use tempfile::TempDir;
    use triblespace::core::blob::IntoBlob;

    #[test]
    fn synchronous_import_sessions_have_an_explicit_tokio_blocking_boundary() {
        let runtime = crate::storage::runtime().unwrap();
        runtime.block_on(async {
            tokio::task::spawn_blocking(|| {
                let directory = TempDir::new().unwrap();
                let pile = directory.path().join("imports.pile");
                let key = directory.path().join("imports.key");
                std::fs::File::create(&pile).unwrap();
                initialize_signer(&pile, Some(&key)).unwrap();
                let mut archive = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
                archive.stage_fragment(projection("async-worker", "body")).unwrap();
                assert!(archive.finish(Ok(())).unwrap().1.is_some());

                let mut code = crate::code::ingest::CodeImportWriter::open(&pile, Some(&key)).unwrap();
                code.stage_fragment(entity! { metadata::description: "code import worker" }).unwrap();
                assert!(code.commit_unit().unwrap().is_some());
                code.close(Ok(())).unwrap();

                // Failed construction drops its owned runtime on the same
                // blocking worker, never on the async executor.
                let absent = directory.path().join("absent").join("pile");
                assert!(ArchiveImportWriter::open(&absent, Some(&key)).is_err());
            }).await.unwrap();
        });
    }

    #[test]
    fn import_writer_retains_exact_byte_acquisition_until_close() {
        use triblespace::core::repo::BlobStoreList;

        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        let key = directory.path().join("archive.key");
        std::fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        let snapshot = writer.pile.snapshot().unwrap();
        let reader = crate::storage::AcquiringReader::new(snapshot.clone(), writer.runtime.clone());
        let body = "an import body staged after the captured observation";
        let fragment = entity! { metadata::description: body };
        let handle = find!(
            handle: Inline<Handle<UTF8String>>,
            pattern!(fragment.facts(), [{ metadata::description: ?handle }])
        ).next().unwrap();
        assert!(!snapshot.contains_blob(handle).unwrap());

        writer.stage_fragment(fragment).unwrap();
        // A later exact byte is available through the still-owned Leech even
        // though the reader's facts, residency and proof observation are old.
        let fetched: View<str> = BlobStoreGet::get(&reader, handle).unwrap();
        assert_eq!(fetched.as_ref(), body);
        assert!(!reader.contains_blob(handle).unwrap());
        assert!(writer.commit_unit().unwrap().is_some());
        writer.close(Ok(())).unwrap();

        // Closing, not the captured reader, owns acquisition lifetime. It
        // cannot silently reopen a pile or endpoint after the writer ends.
        assert!(BlobStoreGet::get::<View<str>, UTF8String>(&reader, handle).is_err());
        let mut reopened = open_pile_strict(&pile).unwrap();
        let bytes: View<str> = BlobStoreGet::get(&reopened.snapshot().unwrap(), handle).unwrap();
        assert_eq!(bytes.as_ref(), body);
        reopened.close().unwrap();
    }

    fn projection(locator: &str, text: &str) -> Fragment {
        let fact = blockdag::text_fact(
            schema::content_fact::modality::TEXT,
            schema::content_fact::direction::IN,
            text,
        )
        .unwrap();
        let part = blockdag::content_part(0, fact, None).unwrap();
        let block = blockdag::block([], None, part).unwrap();
        blockdag::source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            locator,
            format!("{{\\\"text\\\":{text:?}}}").into_bytes(),
            block,
        )
        .unwrap()
    }

    fn projection_at(locator: &str, text: &str, unix_seconds: f64) -> Fragment {
        projection_at_modality(
            locator,
            schema::content_fact::modality::TEXT,
            text,
            unix_seconds,
        )
    }

    fn projection_at_modality(
        locator: &str,
        modality: Id,
        text: &str,
        unix_seconds: f64,
    ) -> Fragment {
        let fact =
            blockdag::text_fact(modality, schema::content_fact::direction::IN, text).unwrap();
        let part = blockdag::content_part(0, fact, None).unwrap();
        let epoch = Epoch::from_unix_seconds(unix_seconds);
        let timestamp: Inline<NsTAIInterval> =
            (epoch, epoch).try_to_inline().expect("valid test interval");
        let block = blockdag::block([], Some(timestamp), part).unwrap();
        blockdag::source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            locator,
            format!("{{\"text\":{text:?}}}").into_bytes(),
            block,
        )
        .unwrap()
    }

    fn projection_after_at(
        locator: &str,
        text: &str,
        unix_seconds: Option<f64>,
        predecessors: &[Id],
    ) -> (Fragment, Id) {
        let fact = blockdag::text_fact(
            schema::content_fact::modality::TEXT,
            schema::content_fact::direction::IN,
            text,
        )
        .unwrap();
        let part = blockdag::content_part(0, fact, None).unwrap();
        let timestamp = unix_seconds.map(|seconds| {
            let epoch = Epoch::from_unix_seconds(seconds);
            (epoch, epoch).try_to_inline().expect("valid test interval")
        });
        let block = blockdag::block(predecessors.iter().copied(), timestamp, part).unwrap();
        let block_id = block.root().unwrap();
        let projection = blockdag::source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            locator,
            format!("{{\"text\":{text:?}}}").into_bytes(),
            block,
        )
        .unwrap();
        (projection, block_id)
    }

    fn projection_split_across_source_elements(locator: &str, text: &str) -> (Fragment, Fragment) {
        let fact = blockdag::text_fact(
            schema::content_fact::modality::TEXT,
            schema::content_fact::direction::IN,
            text,
        )
        .unwrap();
        let part = blockdag::content_part(0, fact, None).unwrap();
        let block = blockdag::block([], None, part).unwrap();
        let block_id = block.root().unwrap();
        let projection = blockdag::source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            locator,
            format!("{{\"text\":{text:?}}}").into_bytes(),
            block,
        )
        .unwrap();
        let (_, facts, metafacts, blobs) = projection.into_parts();
        let mut block_facts = TribleSet::new();
        let mut remaining_facts = TribleSet::new();
        for fact in facts.iter() {
            if fact.e() == &block_id {
                block_facts.insert(fact);
            } else {
                remaining_facts.insert(fact);
            }
        }
        (
            Fragment::from(block_facts),
            Fragment::from_parts(remaining_facts, metafacts, blobs),
        )
    }

    fn commit_projection(
        pile: &std::path::Path,
        key: &std::path::Path,
        locator: &str,
        text: &str,
    ) -> CollectionCommit {
        let mut writer = ArchiveImportWriter::open(pile, Some(key)).unwrap();
        writer.stage_fragment(projection(locator, text)).unwrap();
        writer.finish(Ok(())).unwrap().1.unwrap()
    }

    fn first_embedded_handle(fragment: &Fragment) -> Inline<Handle<UnknownBlob>> {
        let mut blobs = fragment.blobs().clone();
        blobs
            .snapshot()
            .unwrap()
            .iter()
            .next()
            .expect("fixture carries embedded blobs")
            .0
    }

    /// Whether the observed fact view stands on `commit`: the source reads it
    /// and neither hop of the fact pair still lacks a leaf for anything the
    /// source reads. The observation's own store snapshot answers both.
    /// Whether the attached read stands on `commit` with nothing residual.
    fn facts_stand_on(
        observed: &AttachedSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>,
        commit: &CollectionCommit,
    ) -> bool {
        observed
            .support()
            .contains(Handle::<SimpleArchive>::from_hash(commit.data()))
            && observed.residual().is_empty()
    }

    #[test]
    fn staged_blobs_leave_the_delta_and_remain_semantically_invisible_until_finish() {
        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile, &key);

        let fragment = projection("session:staged", "resident only after commit");
        let embedded = first_embedded_handle(&fragment);
        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        writer.stage_fragment(fragment).unwrap();

        assert!(writer.delta_len() > 0);
        assert!(
            writer.delta.blobs().is_empty(),
            "the long-lived logical delta must not retain embedded bytes"
        );

        // The payload is already durable enough to satisfy a fresh reader, but
        // no signed collection root makes its facts visible yet.
        let mut physical = open_pile_strict(&pile).unwrap();
        let reader = physical.snapshot().unwrap();
        let _: Blob<UnknownBlob> = reader.get(embedded).unwrap();
        drop(reader);
        physical.close().unwrap();
        let before = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert!(before.support().is_empty());
        assert!(before.facts().unwrap().iter().next().is_none());
        drop(before);

        let commit = writer.finish(Ok(())).unwrap().1.unwrap();
        let after = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(after.support().len(), 1);
        assert!(facts_stand_on(&after, &commit));
        assert_eq!(projection_ids(&after.facts().unwrap()).len(), 1);
    }

    #[test]
    fn source_failure_closes_without_publishing_a_collection_commit() {
        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile, &key);

        let fragment = projection("session:aborted", "unreachable after source failure");
        let embedded = first_embedded_handle(&fragment);

        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        writer.stage_fragment(fragment).unwrap();
        let error = writer
            .finish::<()>(Err(anyhow!("source projection failed")))
            .unwrap_err();
        assert_eq!(error.to_string(), "source projection failed");

        // `finish` closed the writer even on source failure. Reopening is
        // sound, no semantic edge escaped, and the dependency is merely an
        // unreachable content-addressed record available for later GC.
        let snapshot = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert!(snapshot.support().is_empty());
        assert!(snapshot.facts().unwrap().iter().next().is_none());
        drop(snapshot);
        let mut physical = open_pile_strict(&pile).unwrap();
        let reader = physical.snapshot().unwrap();
        let _: Blob<UnknownBlob> = reader.get(embedded).unwrap();
        drop(reader);
        physical.close().unwrap();
    }

    #[test]
    fn writer_publishes_opaque_annotations_and_multiple_bodies_idempotently() {
        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile, &key);

        let fact = fucid();
        let annotation_kind = fucid();
        let fragment = entity! { &fact @
            metadata::tag*: [&schema::content_fact::KIND, &annotation_kind.id],
            metadata::name*: ["first annotation", "second annotation"],
            schema::content_fact::modality: &schema::content_fact::modality::TEXT,
            schema::content_fact::direction: &schema::content_fact::direction::IN,
            schema::content_fact::payload*: ["first body", "second body"],
        };
        let annotation = entity! { &fact @
            metadata::name: "later annotation",
        };

        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        writer.stage_fragment(fragment.clone()).unwrap();
        assert!(writer.commit_unit().unwrap().is_some());
        // The reader prepares the projection; the writer only commits.
        let succinct = writer
            .pile
            .attach::<SuccinctArchiveBlob>(writer.collection, ())
            .unwrap();
        let rank9 = writer
            .pile
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(writer.collection, succinct)
            .unwrap();
        let prepared = {
            let signer = writer.signer.clone();
            let pile = &mut writer.pile;
            pollster::block_on(async {
                drop(pile.maintain_attached(succinct, &signer).await.unwrap());
                pile.maintain_attached(rank9, &signer).await
            })
            .unwrap()
        };
        let prepared_facts = prepared.read_facts(rank9).unwrap();
        assert!(fragment
            .facts()
            .iter()
            .all(|fact| fact_archive_contains(&prepared_facts, fact)));
        writer.stage_fragment(fragment.clone()).unwrap();
        assert_eq!(writer.delta_len(), 0);
        assert!(writer.commit_unit().unwrap().is_none());
        writer.stage_fragment(annotation.clone()).unwrap();
        assert!(writer.finish(Ok(())).unwrap().1.is_some());

        let snapshot = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(snapshot.support().len(), 2);
        let facts = snapshot.facts().unwrap();
        let tags: BTreeSet<_> = find!(
            tag: Id,
            pattern!(&facts, [{ fact.id @ metadata::tag: ?tag }])
        )
        .collect();
        assert_eq!(
            tags,
            BTreeSet::from([schema::content_fact::KIND, annotation_kind.id])
        );
        let names: BTreeSet<_> = find!(
            name: Inline<Handle<UTF8String>>,
            pattern!(&facts, [{ fact.id @ metadata::name: ?name }])
        )
        .map(|handle| {
            let name: View<str> = snapshot.snapshot().get(handle).unwrap();
            name.to_string()
        })
        .collect();
        assert_eq!(
            names,
            BTreeSet::from([
                "first annotation".to_owned(),
                "second annotation".to_owned(),
                "later annotation".to_owned(),
            ])
        );
        let bodies: BTreeSet<_> = find!(
            payload: Inline<Handle<UTF8String>>,
            pattern!(&facts, [{ fact.id @ schema::content_fact::payload: ?payload }])
        )
        .map(|handle| {
            let body: View<str> = snapshot.snapshot().get(handle).unwrap();
            body.to_string()
        })
        .collect();
        assert_eq!(
            bodies,
            BTreeSet::from(["first body".to_owned(), "second body".to_owned()])
        );
        drop(facts);
        drop(snapshot);

        let mut retry = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        retry.stage_fragment(fragment).unwrap();
        retry.stage_fragment(annotation).unwrap();
        assert_eq!(retry.delta_len(), 0);
        assert!(retry.finish(Ok(())).unwrap().1.is_none());
    }

    /// A writer that commits many times in one process carries its own
    /// source as it goes. Its frontier stays within the commits since its
    /// last carry beside fewer than eight nodes per tier, what it reads back
    /// after a carry still knows every commit, it closes at the carry's
    /// fixed point, and the next open finds nothing to merge or attach.
    #[test]
    fn a_long_writer_carries_its_own_commits() {
        use triblespace::core::collection::MERGE_FAN_IN;

        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile, &key);

        // Fills the second tier once and leaves three commits over.
        let commits = MERGE_FAN_IN * MERGE_FAN_IN + 3;
        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        let source = writer.collection;
        let mut widest = 0;
        for index in 0..commits {
            writer
                .stage_fragment(projection(
                    &format!("session:{index}"),
                    &format!("body {index}"),
                ))
                .unwrap();
            assert!(writer.commit_unit().unwrap().is_some());
            let snapshot = writer.pile.snapshot().unwrap();
            widest = widest.max(snapshot.collection(source).unwrap().cover().len());
        }
        let tiers = 3;
        assert!(
            widest < CARRY_EVERY + (MERGE_FAN_IN - 1) * tiers && widest < commits,
            "the frontier reached {widest} nodes over {commits} commits"
        );
        writer
            .stage_fragment(projection("session:0", "body 0"))
            .unwrap();
        assert_eq!(
            writer.delta_len(),
            0,
            "the read-back facts know the first commit"
        );
        writer.close(Ok(())).unwrap();

        let mut reopened = open_pile_strict_as(&pile, signer.verifying_key()).unwrap();
        let snapshot = reopened.snapshot().unwrap();
        let merges = discovered_records(&snapshot)
            .unwrap()
            .merges()
            .iter()
            .filter(|merge| merge.collection() == source.handle())
            .count();
        assert_eq!(
            merges,
            commits / MERGE_FAN_IN + commits / (MERGE_FAN_IN * MERGE_FAN_IN),
            "every full tier was joined by the writer"
        );
        assert_eq!(
            snapshot.collection(source).unwrap().cover().len(),
            1 + commits % MERGE_FAN_IN,
            "one node for the full second tier and the commits left over"
        );
        drop(snapshot);
        reopened.close().unwrap();

        let length = std::fs::metadata(&pile).unwrap().len();
        let observed = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(observed.support().len(), commits);
        assert!(observed.residual().is_empty());
        drop(observed);
        assert_eq!(
            std::fs::metadata(&pile).unwrap().len(),
            length,
            "the next open publishes no MERGE and no MAP"
        );
    }

    /// Between carries each commit's own Rank9 attachment joins what the
    /// writer knows. The read-back that selects a cover over the whole
    /// frontier, whose cost grows with every commit the collection holds,
    /// runs once per carry and not once per commit. After every commit the
    /// writer still knows every commit it made.
    #[test]
    fn a_writer_reads_its_facts_back_once_per_carry() {
        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile, &key);

        let unit = |index: usize| projection(&format!("session:{index}"), &format!("body {index}"));
        let commits = 2 * CARRY_EVERY + 3;
        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        let read_backs = || READ_BACKS.with(std::cell::Cell::get);
        let before = read_backs();
        for index in 0..commits {
            writer.stage_fragment(unit(index)).unwrap();
            assert!(writer.commit_unit().unwrap().is_some());
            for known in 0..=index {
                writer.stage_fragment(unit(known)).unwrap();
            }
            assert_eq!(
                writer.delta_len(),
                0,
                "after commit {index} the writer knows every commit it made"
            );
        }
        assert_eq!(
            read_backs() - before,
            commits / CARRY_EVERY,
            "one read-back per carry over {commits} commits"
        );
        writer.close(Ok(())).unwrap();
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum StageEvent {
        Put(Inline<Handle<UnknownBlob>>),
    }

    #[derive(Default)]
    struct StageProbe {
        events: Vec<StageEvent>,
    }

    impl BlobStorePut for StageProbe {
        type PutError = Infallible;

        fn put<S, T>(&mut self, item: T) -> std::result::Result<Inline<Handle<S>>, Self::PutError>
        where
            S: triblespace::core::blob::BlobEncoding + 'static,
            T: IntoBlob<S>,
            Handle<S>: triblespace::core::inline::InlineEncoding,
        {
            let handle = item.to_blob().get_handle();
            self.events.push(StageEvent::Put(handle.transmute()));
            Ok(handle)
        }
    }

    #[test]
    fn streamed_blob_batch_writes_every_content_addressed_member() {
        let first = Blob::<UnknownBlob>::new(Bytes::from_source(b"first".to_vec()));
        let second = Blob::<UnknownBlob>::new(Bytes::from_source(b"second".to_vec()));
        let first_handle = first.get_handle();
        let second_handle = second.get_handle();
        let mut probe = StageProbe::default();
        stage_embedded_blobs(&mut probe, vec![second, first]).unwrap();
        assert_eq!(
            probe.events,
            vec![
                StageEvent::Put(second_handle),
                StageEvent::Put(first_handle),
            ]
        );
    }

    #[test]
    fn import_crosses_one_v4_visibility_edge_and_retries_idempotently() {
        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile, &key);

        let fragment = projection("session:one", "one");
        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        writer.stage_fragment(fragment.clone()).unwrap();
        let (_, first) = writer.finish(Ok(())).unwrap();
        let first = first.unwrap();
        let mut retry = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        let length = std::fs::metadata(&pile).unwrap().len();
        retry.stage_fragment(fragment).unwrap();
        let (_, repeated) = retry.finish(Ok(())).unwrap();
        assert_eq!(repeated, None);
        assert_eq!(std::fs::metadata(&pile).unwrap().len(), length);

        let snapshot = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(snapshot.support().len(), 1);
        assert!(facts_stand_on(&snapshot, &first));
        assert_eq!(projection_ids(&snapshot.facts().unwrap()).len(), 1);
    }

    #[test]
    fn unauthorized_duplicate_claim_is_not_an_admitted_archive_root() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key_path = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile_path, &key_path);
        let fragment = projection("session:duplicate-author", "one payload");

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let collection =
            open_configured(&mut pile, schema::DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        let admitted = pile.commit(collection, &signer, fragment.clone()).unwrap();
        let foreign = SigningKey::from_bytes(&[0xA7; 32]);
        let duplicate = pile.commit(collection, &foreign, fragment).unwrap();
        assert_eq!(duplicate.data(), admitted.data());
        pile.close().unwrap();

        let snapshot = pollster::block_on(ensure_local(&pile_path, Some(&key_path))).unwrap();
        assert_eq!(snapshot.support().len(), 1);
        assert!(facts_stand_on(&snapshot, &admitted));
        assert_eq!(projection_ids(&snapshot.facts().unwrap()).len(), 1);
    }

    #[test]
    fn exact_source_snapshots_are_listed_lazily_and_stream_reconstructible() {
        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile, &key);

        let source = b"opaque telemetry and encrypted reasoning\n";
        let chunks = blockdag::source_chunk(0, Bytes::from_source(source.to_vec())).unwrap();
        let fragment = blockdag::source_snapshot(
            schema::source_projection::SOURCE_CODEX,
            "snapshot/v1/session:exact",
            source.len() as u128,
            chunks,
            Some("/moved/rollout.jsonl".to_owned()),
        )
        .unwrap();
        let expected = fragment.root().unwrap();
        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        writer.stage_fragment(fragment).unwrap();
        writer.finish(Ok(())).unwrap();

        let archive = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        let facts = archive.facts().unwrap();
        let snapshots: BTreeSet<_> = find!(
            snapshot: Id,
            pattern!(&facts, [{ ?snapshot @ metadata::tag: &schema::source_snapshot::KIND }])
        )
        .collect();
        assert_eq!(snapshots, BTreeSet::from([expected]));
        let (namespace, locator, length, path) = find!(
            (namespace: Id, locator: Inline<Handle<UTF8String>>, length: u128,
                path: Inline<Handle<UTF8String>>),
            pattern!(&facts, [{
                expected @ schema::source_projection::source_namespace: ?namespace,
                schema::source_projection::source_locator: ?locator,
                schema::source_snapshot::byte_length: ?length,
                files_schema::file::source_path: ?path
            }])
        )
        .next()
        .unwrap();
        assert_eq!(namespace, schema::source_projection::SOURCE_CODEX);
        assert_eq!(length, source.len() as u128);
        let locator: View<str> = archive.snapshot().get(locator).unwrap();
        let path: View<str> = archive.snapshot().get(path).unwrap();
        assert_eq!(locator.as_ref(), "snapshot/v1/session:exact");
        assert_eq!(path.as_ref(), "/moved/rollout.jsonl");
        let chunks: Vec<_> = find!(
            (offset: u128, bytes: RawHandle),
            pattern!(&facts, [
                { expected @ schema::source_snapshot::contains: _?chunk },
                { _?chunk @ schema::source_chunk::offset: ?offset,
                    schema::source_chunk::bytes: ?bytes },
            ])
        )
        .collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, 0);
        let mut reconstructed = Vec::new();
        assert_eq!(
            write_source_snapshot(&facts, archive.snapshot(), expected, &mut reconstructed)
                .unwrap(),
            source.len() as u128
        );
        assert_eq!(reconstructed, source);
    }

    #[test]
    fn growing_source_skips_known_fragments_and_keeps_new_leaf_closure() {
        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile, &key);

        let first_fragment = projection("session:one", "shared");
        let first_len = first_fragment.facts().len();
        let mut first_writer =
            ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        first_writer.stage_fragment(first_fragment.clone()).unwrap();
        let first = first_writer.finish(Ok(())).unwrap().1.unwrap();

        let second_fragment = projection("session:two", "shared");
        let mut second_writer =
            ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        second_writer.stage_fragment(first_fragment).unwrap();
        assert_eq!(second_writer.delta_len(), 0, "known fragment is a replay");
        second_writer
            .stage_fragment(second_fragment.clone())
            .unwrap();
        assert_eq!(
            second_writer.delta_len(),
            second_fragment.facts().len(),
            "a novel source unit retains its reused fact/part/block closure"
        );
        let second = second_writer.finish(Ok(())).unwrap().1.unwrap();
        assert_ne!(first.data(), second.data());

        let snapshot = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(snapshot.support().len(), 2);
        assert_eq!(projection_ids(&snapshot.facts().unwrap()).len(), 2);
        assert!(snapshot.facts().unwrap().iter().count() > first_len);
    }

    #[test]
    fn zero_commit_search_uses_the_canonical_empty_resident_index() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let report = ensure_bm25_index(&pile_path, Some(&key)).unwrap();
        assert_eq!((report.source_elements, report.cover_segments), (0, 0));

        let search = pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert!(search.0.support().is_empty());
        assert!(search
            .1
            .query()
            .unwrap()
            .query_multi(&hash_tokens("anything"))
            .is_empty());
    }

    #[test]
    fn empty_commit_receives_exact_empty_attachments_and_keeps_reads_empty() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let signer = load_signer(&pile_path, Some(&key)).unwrap();
        let mut pile = open_pile_strict(&pile_path).unwrap();
        let collection =
            open_configured(&mut pile, schema::DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        let commit = pile.commit(collection, &signer, Fragment::empty()).unwrap();
        pile.close().unwrap();

        let succinct = ensure_succinct_index(&pile_path, Some(&key)).unwrap();
        let bm25 = ensure_bm25_index(&pile_path, Some(&key)).unwrap();
        assert_eq!(succinct.source_elements, 1);
        assert_eq!((bm25.source_elements, bm25.cover_segments), (1, 1));

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let records = {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot).unwrap()
        };
        let maps = records
            .maps()
            .iter()
            .filter(|map| map.node() == commit.data())
            .count();
        assert_eq!(
            maps, 3,
            "one empty attachment per collection attached to the source"
        );
        pile.close().unwrap();

        let snapshot = pollster::block_on(ensure_local(&pile_path, Some(&key))).unwrap();
        assert_eq!(snapshot.support().len(), 1);
        assert!(facts_stand_on(&snapshot, &commit));
        assert!(projection_ids(&snapshot.facts().unwrap()).is_empty());
        drop(snapshot);
        let search = pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert!(search
            .1
            .query()
            .unwrap()
            .query_multi(&hash_tokens("anything"))
            .is_empty());
    }

    #[test]
    fn timeline_is_pure_and_leaves_inclusion_policy_to_the_caller() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let mut writer =
            ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        writer
            .stage_fragment(projection_at_modality(
                "session:text",
                schema::content_fact::modality::TEXT,
                "spoken",
                1.0,
            ))
            .unwrap();
        writer
            .stage_fragment(projection_at_modality(
                "session:tool",
                schema::content_fact::modality::TOOL_CALL,
                "memory context",
                2.0,
            ))
            .unwrap();
        writer.finish(Ok(())).unwrap();

        let snapshot = pollster::block_on(ensure_local(&pile_path, Some(&key))).unwrap();
        let facts = snapshot.facts().unwrap();
        let complete = timeline_after(&facts, ArchiveTimelineCursor::AfterTime(i128::MIN)).unwrap();
        assert_eq!(complete.len(), 2);
        assert!(complete[0].position < complete[1].position);
        let dialogue: Vec<_> = complete
            .iter()
            .filter(|item| {
                exists!(pattern!(&facts, [
                    { item.block @ schema::block::contains: _?part },
                    { _?part @ schema::content_part::fact: _?fact },
                    { _?fact @ schema::content_fact::modality:
                        &schema::content_fact::modality::TEXT },
                ]))
            })
            .collect();
        assert_eq!(dialogue.len(), 1);
        assert_eq!(dialogue[0].block, complete[0].block);
        let after_first = timeline_after(&facts, complete[0].cursor()).unwrap();
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].block, complete[1].block);
    }

    #[test]
    fn timeline_cursor_preserves_equal_time_blocks_and_causal_order() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let (parent, parent_id) = projection_after_at("thread/parent", "parent", Some(10.0), &[]);
        let (untimed, untimed_id) =
            projection_after_at("thread/untimed", "untimed", None, &[parent_id]);
        let (regressed_child, child_id) =
            projection_after_at("thread/child", "regressed child", Some(5.0), &[untimed_id]);
        let (independent, independent_id) =
            projection_after_at("other/root", "independent", Some(7.0), &[]);

        let mut writer =
            ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        // Deliberately stage out of causal and temporal order. The collection
        // is a set; replay order must come solely from canonical semantics.
        writer.stage_fragment(regressed_child).unwrap();
        writer.stage_fragment(independent).unwrap();
        writer.stage_fragment(untimed).unwrap();
        writer.stage_fragment(parent).unwrap();
        writer.finish(Ok(())).unwrap();

        let snapshot = pollster::block_on(ensure_local(&pile_path, Some(&key))).unwrap();
        let facts = snapshot.facts().unwrap();
        let timeline = timeline_after(&facts, ArchiveTimelineCursor::AfterTime(i128::MIN)).unwrap();
        assert_eq!(timeline.len(), 3, "the untimed conduit stays invisible");
        assert_eq!(timeline[0].block, independent_id);
        assert_eq!(timeline[1].block, parent_id);
        assert_eq!(timeline[2].block, child_id);
        assert!(timeline[0].position < timeline[1].position);
        assert_eq!(
            timeline[1].position, timeline[2].position,
            "the regressed child is lifted to its predecessor's position"
        );

        let after_parent = timeline_after(&facts, timeline[1].cursor()).unwrap();
        assert_eq!(after_parent.len(), 1);
        assert_eq!(after_parent[0].block, child_id);
        assert!(
            timeline_after(&facts, ArchiveTimelineCursor::AfterBlock(untimed_id))
                .unwrap_err()
                .to_string()
                .contains("absent or has no timestamp")
        );
    }

    #[test]
    fn succinct_index_persists_an_exact_validated_v4_derive() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let mut writer =
            ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        writer
            .stage_fragment(projection("session:index", "exact succinct"))
            .unwrap();
        writer.finish(Ok(())).unwrap();

        let report = ensure_succinct_index(&pile_path, Some(&key)).unwrap();

        assert_eq!(report.source_elements, 1);
        let length = std::fs::metadata(&pile_path).unwrap().len();
        assert_eq!(
            ensure_succinct_index(&pile_path, Some(&key)).unwrap(),
            report
        );
        assert_eq!(std::fs::metadata(&pile_path).unwrap().len(), length);

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let raw_target = pile.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
        let records = {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot).unwrap()
        };
        let map = records
            .maps()
            .iter()
            .find(|map| map.attached() == raw_target.handle())
            .copied()
            .expect("stored Archive raw-Succinct MAP");
        // The MAP names its parent node by handle: the commit it stands for.
        let commit = records
            .commits()
            .iter()
            .find(|commit| commit.data() == map.node())
            .copied()
            .expect("the MAP names an Archive commit");
        let reader = pile.snapshot().unwrap();
        let input: Blob<SimpleArchive> = reader
            .get(Handle::<SimpleArchive>::from_hash(commit.data()))
            .unwrap();
        let output: Blob<SuccinctArchiveBlob> = reader
            .get(Handle::<SuccinctArchiveBlob>::from_hash(map.attachment()))
            .unwrap();
        let expected =
            <SuccinctArchiveBlob as CollectionAttachment>::map(&(), &input, &[], &reader).unwrap();
        assert_eq!(expected.get_handle(), output.get_handle());
        pile.close().unwrap();
    }

    /// Each commit gets one BM25 attachment. With no source merge, the index
    /// reads its two attachments as two segments, stands for both commits,
    /// and a repeat publishes nothing.
    #[test]
    fn bm25_uses_per_commit_leaves_and_reads_them_unmerged() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        for (locator, text) in [("session:alpha", "alpha"), ("session:beta", "beta")] {
            let mut writer =
                ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
            writer.stage_fragment(projection(locator, text)).unwrap();
            writer.finish(Ok(())).unwrap();
        }

        let report = ensure_bm25_index(&pile_path, Some(&key)).unwrap();

        assert_eq!(report.source_elements, 2);
        assert_eq!(report.lagging, 0);
        assert_eq!(report.cover_segments, 2);
        let length = std::fs::metadata(&pile_path).unwrap().len();
        assert_eq!(
            ensure_bm25_index(&pile_path, Some(&key)).unwrap(),
            report
        );
        assert_eq!(std::fs::metadata(&pile_path).unwrap().len(), length);

        let signer = load_signer(&pile_path, Some(&key)).unwrap();
        let mut pile = open_pile_strict_as(&pile_path, signer.verifying_key()).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let target = test_target(&mut pile, source);
        let records = {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot).unwrap()
        };
        let maps = records
            .maps()
            .iter()
            .filter(|map| map.attached() == target.handle())
            .count();
        assert_eq!(maps, 2);
        assert!(records.derives().is_empty());
        let store_snapshot = pile.snapshot().unwrap();
        let attached = store_snapshot.attached(target).unwrap();
        assert!(attached.residual().is_empty());
        assert_eq!(attached.cover().len(), 2);
        drop(attached);
        pile.close().unwrap();

        let search = pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        let query = search.1.query().unwrap();
        assert_eq!(query.query_multi(&hash_tokens("alpha")).len(), 1);
        assert_eq!(query.query_multi(&hash_tokens("beta")).len(), 1);
    }

    #[test]
    fn bm25_collapses_repeated_content_to_its_canonical_block() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        for (locator, seconds) in [("session:first", 1.0), ("session:second", 2.0)] {
            let mut writer =
                ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
            writer
                .stage_fragment(projection_at(locator, "shared closure needle", seconds))
                .unwrap();
            writer.finish(Ok(())).unwrap();
        }

        let report = ensure_bm25_index(&pile_path, Some(&key)).unwrap();
        assert_eq!(report.source_elements, 2);
        let search = pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        let hits = search
            .1
            .query()
            .unwrap()
            .query_multi(&hash_tokens("shared closure needle"));
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn lazy_bm25_maintenance_extends_after_a_new_commit() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let first_fragment = projection("session:first", "alpha");
        let mut writer =
            ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        writer.stage_fragment(first_fragment).unwrap();
        writer.finish(Ok(())).unwrap();
        let first = pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert_eq!(
            first
                .1
                .query()
                .unwrap()
                .query_multi(&hash_tokens("alpha"))
                .len(),
            1
        );
        drop(first);

        let second_fragment = projection("session:second", "beta βeta 🛰️");
        let mut writer =
            ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        writer.stage_fragment(second_fragment.clone()).unwrap();
        writer.finish(Ok(())).unwrap();

        let extended = pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        {
            let query = extended.1.query().unwrap();
            assert_eq!(query.query_multi(&hash_tokens("alpha")).len(), 1);
            assert_eq!(query.query_multi(&hash_tokens("beta")).len(), 1);
            assert_eq!(query.query_multi(&hash_tokens("🛰️")).len(), 1);
        }
        drop(extended);

        let before = std::fs::metadata(&pile_path).unwrap().len();
        let mut retry =
            ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        retry.stage_fragment(second_fragment).unwrap();
        retry.finish(Ok(())).unwrap();
        let after_retry = pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert_eq!(
            after_retry
                .1
                .query()
                .unwrap()
                .query_multi(&hash_tokens("beta"))
                .len(),
            1
        );
        assert_eq!(std::fs::metadata(&pile_path).unwrap().len(), before);
    }

    /// The collection union is a valid Archive, but the tagged block and its
    /// part/fact closure live in separate signed elements. Each commit is a
    /// node of its own, and the BM25 mapping refuses the node that holds the
    /// block without its closure: maintenance leaves that node as the
    /// residual, attaches every other one, and a repeat publishes nothing.
    /// Once the carry joins the two halves into one merged node, that node
    /// holds the whole closure, and its attachment represents the block.
    #[test]
    fn bm25_leaves_a_split_block_residual_until_a_merged_node_holds_it() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let (block_element, remainder_element) =
            projection_split_across_source_elements("session:split", "closure needle");
        let signer = load_signer(&pile_path, Some(&key)).unwrap();
        let mut pile = open_pile_strict_as(&pile_path, signer.verifying_key()).unwrap();
        let collection =
            open_configured(&mut pile, schema::DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        let block_commit = pile.commit(collection, &signer, block_element).unwrap();
        let remainder_commit = pile.commit(collection, &signer, remainder_element).unwrap();
        let target = test_target(&mut pile, collection);
        pile.close().unwrap();
        // A whole commit after the split one: attached like any other.
        commit_projection(&pile_path, &key, "session:whole", "whole haystack");

        let archive = pollster::block_on(ensure_local(&pile_path, Some(&key))).unwrap();
        assert_eq!(archive.support().len(), 3);
        assert_eq!(projection_ids(&archive.facts().unwrap()).len(), 2);
        drop(archive);

        let report = ensure_bm25_index(&pile_path, Some(&key)).unwrap();
        assert_eq!(report.source_elements, 3);
        assert_eq!(report.lagging, 1, "only the split block's node is residual");
        let length = std::fs::metadata(&pile_path).unwrap().len();
        assert_eq!(
            ensure_bm25_index(&pile_path, Some(&key)).unwrap(),
            report
        );
        assert_eq!(
            std::fs::metadata(&pile_path).unwrap().len(),
            length,
            "a refused node publishes nothing on a later pass either"
        );

        let mut pile = open_pile_strict_as(&pile_path, signer.verifying_key()).unwrap();
        let records = discovered_records(&pile.snapshot().unwrap()).unwrap();
        let attached_nodes: BTreeSet<_> = records
            .maps()
            .iter()
            .filter(|map| map.attached() == target.handle())
            .map(|map| map.node())
            .collect();
        assert!(!attached_nodes.contains(&block_commit.data()));
        assert!(attached_nodes.contains(&remainder_commit.data()));
        assert_eq!(attached_nodes.len(), 2);
        pile.close().unwrap();

        let (_, index, lag) =
            pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert_eq!(lag.index, 1);
        assert!(lag.facts.is_current());
        let query = index.query().unwrap();
        assert_eq!(query.query_multi(&hash_tokens("whole haystack")).len(), 1);
        assert!(query.query_multi(&hash_tokens("closure needle")).is_empty());
        drop(query);
        drop(index);

        // Five more commits fill the lowest tier; the carry joins all eight
        // into one node, which holds the block with its closure.
        for word in ["one", "two", "three", "four", "five"] {
            commit_projection(&pile_path, &key, &format!("session:{word}"), word);
        }
        let report = ensure_bm25_index(&pile_path, Some(&key)).unwrap();
        assert_eq!(report.lagging, 0);
        assert_eq!(report.cover_segments, 1);
        let (_, index, lag) =
            pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert!(lag.is_current(), "{lag:?}");
        let query = index.query().unwrap();
        assert_eq!(query.query_multi(&hash_tokens("closure needle")).len(), 1);
    }

    /// Eight commits fill the root's lowest tier, so the carry joins them
    /// with one 8-input MERGE, and BM25 maintenance attaches the merged node
    /// once, mapped from the merged node's own bytes; nothing is attached to
    /// the eight nodes the carry consumed. The index then reads as one
    /// segment that finds every commit, and a repeat publishes nothing.
    #[test]
    fn bm25_attaches_the_merged_node_once_and_reads_it_as_one_segment() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile_path, &key);
        let words = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        ];
        assert_eq!(words.len(), triblespace::core::collection::MERGE_FAN_IN);
        for word in words {
            commit_projection(&pile_path, &key, &format!("session:{word}"), word);
        }

        // Opened as the maintaining key, as every faculty opens: the carry's
        // merge is then believed here and by the reopen below.
        let mut pile = open_pile_strict_as(&pile_path, signer.verifying_key()).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let target = test_target(&mut pile, source);
        let bm25_maps = |pile: &mut Pile| {
            let records = discovered_records(&pile.snapshot().unwrap()).unwrap();
            records
                .maps()
                .iter()
                .filter(|map| map.attached() == target.handle())
                .copied()
                .collect::<Vec<_>>()
        };
        drop(
            pollster::block_on(
                pile.maintain_attached_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(
                    target, &signer,
                ),
            )
            .unwrap(),
        );
        let records = discovered_records(&pile.snapshot().unwrap()).unwrap();
        let root_merges: Vec<_> = records
            .merges()
            .iter()
            .filter(|merge| merge.collection() == source.handle())
            .copied()
            .collect();
        assert_eq!(root_merges.len(), 1, "one carry of the full tier");
        assert_eq!(root_merges[0].inputs().len(), words.len());
        let maps = bm25_maps(&mut pile);
        assert_eq!(maps.len(), 1, "only the merged node is attached");
        assert_eq!(maps[0].node(), root_merges[0].result());
        let snapshot = pile.snapshot().unwrap();
        let merged: Blob<SimpleArchive> = snapshot
            .get(Handle::<SimpleArchive>::from_hash(root_merges[0].result()))
            .unwrap();
        let expected = archive_bm25::derive_element(&snapshot, merged).unwrap();
        assert_eq!(
            Handle::<PortableBM25Blob>::to_hash(expected.get_handle()),
            maps[0].attachment(),
            "the attachment is the merged node's own image"
        );
        let attached = snapshot.attached(target).unwrap();
        assert_eq!(attached.cover().len(), 1);
        assert!(attached.residual().is_empty());
        drop((attached, snapshot));

        let length = std::fs::metadata(&pile_path).unwrap().len();
        drop(
            pollster::block_on(
                pile.maintain_attached_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(
                    target, &signer,
                ),
            )
            .unwrap(),
        );
        assert_eq!(bm25_maps(&mut pile), maps);
        pile.close().unwrap();
        assert_eq!(
            std::fs::metadata(&pile_path).unwrap().len(),
            length,
            "a repeat pass publishes no record"
        );

        let (_, index, lag) =
            pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert!(lag.is_current(), "{lag:?}");
        assert_eq!(index.segments().len(), 1);
        let query = index.query().unwrap();
        for word in words {
            assert_eq!(query.query_multi(&hash_tokens(word)).len(), 1, "{word}");
        }
    }

    /// A new commit costs the index exactly one new attachment, the index
    /// stands for every commit after each pass, and a complete repeat
    /// publishes no record. With no source merge, each attachment is its own
    /// segment.
    #[test]
    fn bm25_maintenance_attaches_only_the_new_node_and_repeats_without_work() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile_path, &key);
        let bm25_maps = |pile: &mut Pile, target: Collection<PortableBM25Blob>| {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot)
                .unwrap()
                .maps()
                .iter()
                .filter(|map| map.attached() == target.handle())
                .count()
        };
        let maintain_fresh = |pile: &mut Pile, target, segments: usize| {
            let maintained = pollster::block_on(
                pile.maintain_attached_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(
                    target, &signer,
                ),
            )
            .unwrap();
            let index = maintained.attached(target).unwrap();
            assert!(index.residual().is_empty());
            assert_eq!(index.cover().len(), segments);
        };

        commit_projection(&pile_path, &key, "session:first", "first residual");
        let mut pile = open_pile_strict_as(&pile_path, signer.verifying_key()).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let target = test_target(&mut pile, source);
        maintain_fresh(&mut pile, target, 1);
        assert_eq!(bm25_maps(&mut pile, target), 1);
        pile.close().unwrap();

        commit_projection(&pile_path, &key, "session:second", "second residual");
        let mut pile = open_pile_strict_as(&pile_path, signer.verifying_key()).unwrap();
        maintain_fresh(&mut pile, target, 2);
        assert_eq!(
            bm25_maps(&mut pile, target),
            2,
            "only the new commit is attached"
        );
        let count = |pile: &mut Pile| {
            let store_snapshot = pile.snapshot().unwrap();
            store_snapshot.records().unwrap().count()
        };
        let before = count(&mut pile);
        maintain_fresh(&mut pile, target, 2);
        assert_eq!(
            count(&mut pile),
            before,
            "a complete retry publishes no collection records"
        );
        pile.close().unwrap();
    }

    /// A believed MAP whose attachment bytes are not here is not usable: the
    /// read descends past it, and maintenance builds the same attachment
    /// again, after which the read takes it.
    #[test]
    fn maintenance_rebuilds_an_attachment_whose_bytes_are_missing() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile_path, &key);
        let commit = commit_projection(&pile_path, &key, "session:pending", "recover output");

        let mut pile = open_pile_strict_as(&pile_path, signer.verifying_key()).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let target = test_target(&mut pile, source);
        let store_snapshot = pile.snapshot().unwrap();
        let input: Blob<SimpleArchive> = store_snapshot
            .get(Handle::<SimpleArchive>::from_hash(commit.data()))
            .unwrap();
        let output = archive_bm25::derive_element(&store_snapshot, input).unwrap();
        let output_data = Handle::<PortableBM25Blob>::to_hash(output.get_handle());
        let pending = CollectionMap::sign(&signer, target.handle(), commit.data(), output_data);
        drop(output);
        drop(store_snapshot);
        CollectionStore::insert(&mut pile, CollectionRecord::Map(pending)).unwrap();
        let before = pile.snapshot().unwrap();
        assert!(before
            .metadata(Handle::<PortableBM25Blob>::from_hash(output_data))
            .unwrap()
            .is_none());
        let unusable = before.attached(target).unwrap();
        assert!(unusable.cover().is_empty());
        assert_eq!(unusable.residual().len(), 1);
        drop((unusable, before));

        let ready_snapshot = pollster::block_on(
            pile.maintain_attached_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(
                target, &signer,
            ),
        )
        .unwrap();
        let ready = ready_snapshot.attached(target).unwrap();
        assert!(ready.residual().is_empty());
        assert_eq!(
            ready
                .cover()
                .members()
                .map(Handle::<PortableBM25Blob>::to_hash)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([output_data])
        );
        drop(ready);
        let records = {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot).unwrap()
        };
        assert!(
            records
                .maps()
                .iter()
                .filter(|map| map.attached() == target.handle())
                .all(|map| *map == pending),
            "the rebuilt attachment is the same deterministic MAP"
        );
        pile.close().unwrap();
    }

    #[test]
    fn exact_fact_cover_and_raw_export_need_no_outer_commits() {
        use triblespace::core::collection::CollectionAttachment;

        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        // Chunk and snapshot ids are deliberately extrinsic. Two different
        // witnesses of identical byte geometry must not duplicate the output.
        let chunk_a = fucid();
        let chunk_b = fucid();
        let snapshot_id = fucid();
        let block = fucid();
        let timestamp = crate::clock::point(hifitime::Epoch::from_tai_duration(
            hifitime::Duration::from_total_nanoseconds(1),
        ))
        .unwrap();
        let mut fragment = entity! { &chunk_a @
            schema::source_chunk::offset: 0u128,
            schema::source_chunk::bytes: b"raw".to_vec(),
        };
        fragment += entity! { &chunk_b @
            schema::source_chunk::offset: 0u128,
            schema::source_chunk::bytes: b"raw".to_vec(),
        };
        fragment += entity! { &snapshot_id @
            metadata::tag: &schema::source_snapshot::KIND,
            metadata::name*: ["first annotation", "another annotation"],
            schema::source_snapshot::byte_length*: [3u128, 9u128],
            schema::source_snapshot::contains*: [&chunk_a, &chunk_b],
        };
        fragment += entity! { &block @
            metadata::tag: &schema::block::KIND,
            schema::block::timestamp: timestamp,
        };

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let succinct = pile.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
        let rank9 = pile
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
            .unwrap();
        let (_, facts, _metadata, blobs) = fragment.into_parts();
        stage_embedded_blobs(&mut pile, embedded_blobs(blobs)).unwrap();
        let data = pile.put::<SimpleArchive, _>(facts).unwrap();
        // Raw value construction needs no COMMIT authority. It does not,
        // however, manufacture admitted support or signed collection records.
        let before = pile.snapshot().unwrap();
        let raw: Blob<SimpleArchive> = before.get(data).unwrap();
        let compact = SuccinctArchiveBlob::map(&(), &raw, &[], &before).unwrap();
        let compact = Handle::<SuccinctArchiveBlob>::to_hash(
            pile.put::<SuccinctArchiveBlob, _>(compact).unwrap(),
        );
        let accelerated = Rank9AcceleratedSuccinctArchiveBlob::map(
            &succinct,
            &raw,
            &[compact],
            &pile.snapshot().unwrap(),
        )
        .unwrap();
        let member = pile
            .put::<Rank9AcceleratedSuccinctArchiveBlob, _>(accelerated)
            .unwrap();
        let after = pile.snapshot().unwrap();
        assert!(after.attached(rank9).unwrap().cover().is_empty());
        assert!(source.admitted(&after).unwrap().is_empty());
        // The view is built straight from the resident member through the
        // collection's own descriptor, as an attached snapshot would build it.
        let descriptor = Fragment::from(after.get::<TribleSet, _>(rank9.handle()).unwrap());
        let facts =
            <FactArchive as triblespace::core::collection::TryFromCover<_>>::try_from_cover(
                &rank9.cover([member]),
                &descriptor,
                &after,
            )
            .unwrap();
        let mut output = Vec::new();
        assert_eq!(
            write_source_snapshot(&facts, &after, snapshot_id.id, &mut output).unwrap(),
            3,
        );
        assert_eq!(output, b"raw");
        let timeline = timeline_after(&facts, ArchiveTimelineCursor::AfterTime(i128::MIN)).unwrap();
        assert_eq!(
            timeline,
            [ArchiveTimelineBlock {
                position: 1,
                block: block.id
            }]
        );
        pile.close().unwrap();
    }

    #[test]
    fn export_geometry_failure_does_not_reject_other_fact_queries() {
        let source = fucid();
        let chunk = fucid();
        let mut fragment = entity! { &chunk @
            schema::source_chunk::offset: 1u128,
            schema::source_chunk::bytes: b"gap".to_vec(),
        };
        fragment += entity! { &source @
            metadata::tag: &schema::source_snapshot::KIND,
            schema::source_snapshot::contains: &chunk,
            schema::source_snapshot::byte_length: 3u128,
            metadata::name: "still queryable",
        };
        let reader = fragment.blobs().clone().snapshot().unwrap();
        let names: Vec<_> = find!(
            name: Inline<Handle<UTF8String>>,
            pattern!(fragment.facts(), [{ source.id @ metadata::name: ?name }])
        )
        .collect();
        assert_eq!(names.len(), 1);
        let error = write_source_snapshot(fragment.facts(), &reader, source.id, &mut Vec::new())
            .unwrap_err();
        assert!(error.to_string().contains("expected 0"));
    }

    #[test]
    fn timeline_uses_all_timestamp_annotations_without_scalar_validation() {
        let block = fucid();
        let later = crate::clock::point(hifitime::Epoch::from_tai_duration(
            hifitime::Duration::from_total_nanoseconds(9),
        ))
        .unwrap();
        let earlier = crate::clock::point(hifitime::Epoch::from_tai_duration(
            hifitime::Duration::from_total_nanoseconds(4),
        ))
        .unwrap();
        let fragment = entity! { &block @
            metadata::tag: &schema::block::KIND,
            schema::block::timestamp*: [later, earlier],
            metadata::name*: ["one", "two"],
        };
        let timeline = timeline_after(
            fragment.facts(),
            ArchiveTimelineCursor::AfterTime(i128::MIN),
        )
        .unwrap();
        assert_eq!(
            timeline,
            [ArchiveTimelineBlock {
                position: 4,
                block: block.id
            }]
        );
    }

    /// The Archive BM25 mapping answers like the union only for covers whose
    /// nodes hold whole source units (see `archive_bm25`). This pins that
    /// faculties' own importer commits whole units, which is that premise
    /// for data we write; it says nothing of another writer's commits or of
    /// historical input, and the writer itself checks nothing. Every commit
    /// that holds a block's tag holds every part the collection says the
    /// block contains, with each part's fields and its content fact's, and
    /// every part a commit holds is contained by a block that commit tags.
    /// The second transcript repeats the first's opening message and answers
    /// with a reply that begins with the first reply's parts, so a writer
    /// that committed only the facts the pile lacked would split those
    /// blocks across the two commits, and this test would fail.
    #[test]
    fn every_import_commit_holds_whole_blocks() {
        const FIRST: &str = r#"{"type":"user","sessionId":"whole-1","uuid":"u1","parentUuid":null,"timestamp":"2026-03-01T15:34:01.542Z","message":{"role":"user","content":"hello there"}}
{"type":"assistant","sessionId":"whole-1","uuid":"a1","parentUuid":"u1","timestamp":"2026-03-01T15:34:02.000Z","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"thinking","thinking":"consider it","signature":"opaque"},{"type":"text","text":"hi!"},{"type":"tool_use","id":"toolu_1","name":"Screenshot","input":{"display":1}}]}}
{"type":"user","sessionId":"whole-1","uuid":"u2","parentUuid":"a1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"screenshot below"},{"type":"image","source":{"type":"base64","media_type":"IMAGE/PNG; charset=binary","data":"iVBORw=="}}]}]}}"#;
        const SECOND: &str = r#"{"type":"user","sessionId":"whole-2","uuid":"v1","parentUuid":null,"message":{"role":"user","content":"hello there"}}
{"type":"assistant","sessionId":"whole-2","uuid":"b1","parentUuid":"v1","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"thinking","thinking":"consider it","signature":"opaque"},{"type":"text","text":"hi!"},{"type":"text","text":"and something more"}]}}"#;

        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile_path, &key);
        for (name, transcript) in [("first.jsonl", FIRST), ("second.jsonl", SECOND)] {
            let mut writer =
                ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
            let projection = crate::archive_claude_code::project_bytes(
                name,
                Bytes::from_source(transcript.as_bytes().to_vec()),
                |projected| writer.stage_fragment(projected.fragment),
            );
            let (_, commit) = writer.finish(projection).unwrap();
            assert!(commit.is_some(), "{name} commits");
        }

        let mut pile = open_pile_strict_as(&pile_path, signer.verifying_key()).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let snapshot = pile.snapshot().unwrap();
        let commits: Vec<TribleSet> = discovered_records(&snapshot)
            .unwrap()
            .commits()
            .iter()
            .filter(|commit| commit.collection() == source.handle())
            .map(|commit| {
                snapshot
                    .get::<TribleSet, SimpleArchive>(Handle::<SimpleArchive>::from_hash(
                        commit.data(),
                    ))
                    .unwrap()
            })
            .collect();
        drop(snapshot);
        pile.close().unwrap();
        assert_eq!(commits.len(), 2);
        let mut union = TribleSet::new();
        for facts in &commits {
            union += facts.clone();
        }

        let tagged = |facts: &TribleSet, kind: Id| -> BTreeSet<Id> {
            find!(
                entity: Id,
                pattern!(facts, [{ ?entity @ metadata::tag: &kind }])
            )
            .collect()
        };
        let parts_of = |block: Id| -> BTreeSet<Id> {
            find!(
                part: Id,
                pattern!(&union, [{ block @ schema::block::contains: ?part }])
            )
            .collect()
        };
        // What the BM25 mapping reads of one block, as the whole collection
        // states it: the block's tag and `contains` facts, each part's tag,
        // ordinal and fact, and each content fact's tag, modality, direction
        // and payload.
        let read = [
            metadata::tag.id(),
            schema::block::contains.id(),
            schema::content_part::ordinal.id(),
            schema::content_part::fact.id(),
            schema::content_fact::modality.id(),
            schema::content_fact::direction.id(),
            schema::content_fact::payload.id(),
            schema::content_fact::blob.id(),
            schema::content_fact::asset_pointer.id(),
        ];
        let unit = |block: Id| -> TribleSet {
            let parts = parts_of(block);
            let mut facts = BTreeSet::new();
            for part in &parts {
                let part = *part;
                facts.extend(find!(
                    fact: Id,
                    pattern!(&union, [{ part @ schema::content_part::fact: ?fact }])
                ));
            }
            let mut unit = TribleSet::new();
            for trible in union.iter() {
                let entity = trible.e();
                if (*entity == block || parts.contains(entity) || facts.contains(entity))
                    && read.contains(trible.a())
                {
                    unit.insert(trible);
                }
            }
            unit
        };

        let mut multi_part_blocks = 0;
        for (index, facts) in commits.iter().enumerate() {
            for block in tagged(facts, schema::block::KIND) {
                let missing = unit(block)
                    .iter()
                    .filter(|trible| !facts.contains(trible))
                    .count();
                assert_eq!(
                    missing, 0,
                    "commit {index} holds block {block:X} without all of its parts"
                );
                if parts_of(block).len() > 1 {
                    multi_part_blocks += 1;
                }
            }
            for part in tagged(facts, schema::content_part::KIND) {
                let blocks: BTreeSet<Id> = find!(
                    block: Id,
                    pattern!(facts, [{
                        ?block @
                            metadata::tag: &schema::block::KIND,
                            schema::block::contains: &part,
                    }])
                )
                .collect();
                assert!(
                    !blocks.is_empty(),
                    "commit {index} holds part {part:X} without a block it tags"
                );
            }
        }
        assert!(multi_part_blocks >= 3, "{multi_part_blocks}");
        // The case a splitting writer gets wrong is present: the two commits
        // share parts.
        assert!(!tagged(&commits[0], schema::content_part::KIND)
            .is_disjoint(&tagged(&commits[1], schema::content_part::KIND)));
    }
}
