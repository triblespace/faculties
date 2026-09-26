//! Collection-native Archive runtime over the V4 descriptor-handle calculus.
//!
//! Archive authorship has one durable Ed25519 signer and one fixed canonical
//! SimpleArchive-union descriptor. Imports stage independently derivable source
//! fragments which contribute new evidence and cross exactly one signed COMMIT
//! visibility edge per publication. Reads snapshot that same collection;
//! there is no Repository branch, CAS head, sidecar registry, or fallback
//! identity.

use std::borrow::BorrowMut;
use std::collections::{BTreeMap, BTreeSet};

use anybytes::Bytes;
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::blob::encodings::{simplearchive::SimpleArchive, UnknownBlob};
use triblespace::core::blob::Blob;
use triblespace::core::collection::{
    Collection, CollectionCommit, CollectionSnapshot, CollectionSnapshotExt, CollectionStoreExt,
};
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::{BlobStoreGet, BlobStorePut, SnapshotSource};
use triblespace::prelude::blobencodings::RawBytes;
use triblespace::prelude::inlineencodings::Handle;
use triblespace::prelude::*;
use triblespace_search::portable_bm25::PortableBM25Blob;

use crate::archive_bm25;
use crate::blockdag;
use crate::schemas::blockdag as schema;
use crate::storage::{load_signer, open_pile_strict, FactArchive, FactLag};

use crate::collection_names::open_configured;
#[cfg(test)]
use triblespace::core::collection::{
    CollectionDerivation, CollectionDerive, CollectionRealizationError, CollectionRecord,
    CollectionStore,
};
#[cfg(test)]
use triblespace::core::repo::BlobStoreMeta;

type RawHandle = Inline<Handle<RawBytes>>;

/// Stage Archive fragments for commit-last publication.
///
/// Supplied facts remain open-world relations, including opaque ids and further
/// annotations. Publication does not require a closed-world catalog decode.
pub struct ArchiveImportWriter<P = Pile> {
    pile: P,
    collection: Collection<SimpleArchive>,
    signer: SigningKey,
    current: FactArchive,
    delta: Fragment,
}

impl ArchiveImportWriter {
    pub async fn open(
        pile_path: &std::path::Path,
        key_path: Option<&std::path::Path>,
    ) -> Result<Self> {
        let signer = load_signer(pile_path, key_path)?;
        let mut pile = open_pile_strict(pile_path)?;
        let result = async {
            let source =
                open_configured(&mut pile, schema::DEFAULT_SCOPE_ID, signer.verifying_key())?;
            let observed = ensure_facts(&mut pile, source, &signer).await?;
            let current = observed
                .view::<FactArchive>()
                .context("read Archive facts")?;
            Ok((source, current))
        }
        .await;
        match result {
            Ok((collection, current)) => {
                let mut writer = Self {
                    pile,
                    collection,
                    signer,
                    current,
                    delta: Fragment::empty(),
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

impl<P: BorrowMut<Pile>> ArchiveImportWriter<P> {
    /// Stage against a caller-owned pile. The caller controls its lifetime and close.
    pub async fn from_pile(mut pile: P, signer: &SigningKey) -> Result<Self> {
        let source = open_configured(
            pile.borrow_mut(),
            schema::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
        )?;
        let observed = ensure_facts(pile.borrow_mut(), source, signer).await?;
        let current = observed
            .view::<FactArchive>()
            .context("read Archive facts")?;
        let mut writer = Self {
            pile,
            collection: source,
            signer: signer.clone(),
            current,
            delta: Fragment::empty(),
        };
        writer.stage_fragment(blockdag::vocabulary_fragment())?;
        Ok(writer)
    }

    pub fn stage_fragment(&mut self, fragment: Fragment) -> Result<()> {
        // A Fragment is the independently derivable source unit. A wholly
        // known candidate is an idempotent replay and can be skipped. Once it
        // contributes even one new fact, retain its complete closure in this
        // COMMIT element—including facts already present in older elements.
        // Set union makes that duplication semantically free, while exact
        // homomorphisms (BM25 and future derivatives) can derive every leaf
        // without depending on an implicit merge with historical commits.
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
        stage_embedded_blobs(self.pile.borrow_mut(), embedded)?;

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
    pub fn commit_unit(&mut self) -> Result<Option<CollectionCommit>> {
        if self.delta.facts().is_empty() {
            return Ok(None);
        }
        let fragment = std::mem::replace(&mut self.delta, Fragment::empty());
        let published = fragment.facts().clone();
        let commit = self
            .pile
            .borrow_mut()
            .commit(self.collection, &self.signer, fragment)
            .context("commit authored Archive projection unit")?;
        self.current = extend_archive(&self.current, &published);
        drop(
            pollster::block_on(crate::storage::ensure_downstream(
                self.pile.borrow_mut(),
                self.collection,
                &self.signer,
            ))
            .context(
                "Archive projection unit was committed, but ensuring its derived views failed",
            )?,
        );
        Ok(Some(commit))
    }
}

impl ArchiveImportWriter {
    /// Close the pile, publishing any still-staged delta first.
    pub fn close<T>(mut self, surrounding: Result<T>) -> Result<T> {
        let result = surrounding.and_then(|value| {
            self.commit_unit()?;
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
            let commit = self.commit_unit()?;
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

/// Exact membership without rebuilding an in-memory `TribleSet` over the
/// maintained shard union.
fn fact_archive_contains(facts: &FactArchive, fact: &Trible) -> bool {
    exists!(facts.pattern(
        inlineencodings::GenId::inline_from(*fact.e()),
        inlineencodings::GenId::inline_from(*fact.a()),
        *fact.v::<UnknownInline>(),
    ))
}

fn extend_archive(current: &FactArchive, additions: &TribleSet) -> FactArchive {
    if additions.is_empty() {
        return current.clone();
    }
    current.with_segments([
        triblespace::core::blob::encodings::succinctarchive::SuccinctArchive::from(additions),
    ])
}

fn close_pile<T>(pile: Pile, result: Result<T>, failure_context: &str) -> Result<T> {
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
) -> Result<CollectionSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>> {
    ensure_local_with_storage(&crate::storage::Storage::new(
        pile_path.to_owned(),
        key_path.map(std::path::Path::to_owned),
    ))
}

pub fn ensure_local_with_storage(
    storage: &crate::storage::Storage,
) -> Result<CollectionSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>> {
    storage.with_pile(|pile, signer| {
        let source = open_configured(pile, schema::DEFAULT_SCOPE_ID, signer.verifying_key())?;
        pollster::block_on(ensure_facts(pile, source, signer))
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

/// Derive the signer's own Archive commits into the fact pair and mirror its
/// own merges there, then attach Rank9. Another writer's commit that has no
/// leaf yet is derived too when its payload is already here; nothing is
/// fetched for it, and what cannot be derived here is that writer's lag, as
/// is an own commit neither view can derive.
async fn ensure_facts(
    pile: &mut Pile,
    source: Collection<SimpleArchive>,
    signer: &SigningKey,
) -> Result<CollectionSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>> {
    let (succinct, rank9) = fact_views(pile, source)?;
    crate::storage::tolerate_own_lag(pile.maintain(succinct, signer).await)
        .context("maintain Succinct Archive fact collection")?;
    crate::storage::tolerate_own_lag(pile.maintain(rank9, signer).await)
        .context("maintain Rank9 Archive fact collection")?;
    pile.snapshot()
        .context("freeze maintained Archive facts")?
        .collection(rank9)
        .context("attach Archive fact collection")
}

/// Accelerated-Succinct derivation summary. Source membership is measured in
/// distinct commit payloads the snapshot can read, never in the number of
/// attestations over them; the lag says how many admitted commits each hop of
/// the fact pair has not derived yet, a commit whose payload is not here
/// included.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SuccinctIndexReport {
    pub source_elements: usize,
    pub lag: FactLag,
    pub source_collection: Inline<Handle<SimpleArchive>>,
    pub target_collection: Inline<Handle<SimpleArchive>>,
}

pub async fn ensure_succinct_index(
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
    storage.with_pile(|pile, signer| {
        let source = open_configured(pile, schema::DEFAULT_SCOPE_ID, signer.verifying_key())?;
        let observed = pollster::block_on(ensure_facts(pile, source, signer))?;
        let (succinct, rank9) = fact_views(pile, source)?;
        let snapshot = observed.snapshot();
        let source_elements = snapshot
            .collection(source)
            .context("attach Archive source")?
            .support()
            .context("resolve Archive source support")?
            .len();
        Ok(SuccinctIndexReport {
            source_elements,
            lag: FactLag::of(snapshot, source, succinct, rank9)?,
            source_collection: source.handle(),
            target_collection: rank9.handle(),
        })
    })
}

/// Archive BM25 derivation summary: the source commits the snapshot can
/// read, how many admitted source commits the index has no leaf for yet, and
/// how many segments its resident cover has.
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

/// Register the BM25 derivation over the Archive source.
fn bm25_target(
    pile: &mut Pile,
    source: Collection<SimpleArchive>,
    signer: &SigningKey,
) -> Result<Collection<PortableBM25Blob>> {
    pile.derive_with(
        source,
        archive_bm25::ArchiveBlockTextBm25Mapping,
        crate::collection_names::private_policy(signer.verifying_key()),
    )
    .context("register Archive BM25 derivation")
}

/// Derive the signer's own Archive commits into the BM25 index and mirror
/// its own merges there, then read it back from the maintained snapshot
/// together with how far it lags the source there.
/// Provenance records are neither part of this value nor required to replay it.
async fn ensure_bm25(
    pile: &mut Pile,
    source: Collection<SimpleArchive>,
    signer: &SigningKey,
) -> Result<EnsuredBm25> {
    let target = bm25_target(pile, source, signer)?;
    crate::storage::tolerate_own_lag(
        pile.maintain_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(target, signer)
            .await,
    )
    .context("maintain Archive BM25 cover")?;
    let maintained = pile
        .snapshot()
        .context("freeze maintained Archive BM25 cover")?;
    let attached = maintained
        .collection(target)
        .context("attach Archive BM25 cover")?;
    let source_view = maintained
        .collection(source)
        .context("attach Archive source")?;
    let lagging = crate::storage::underived(&maintained, source, target)
        .context("count Archive commits without a BM25 leaf")?;
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

pub async fn ensure_bm25_index(
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
    storage.with_pile(|pile, signer| {
        pollster::block_on(async {
            let source = open_configured(pile, schema::DEFAULT_SCOPE_ID, signer.verifying_key())?;
            drop(ensure_facts(pile, source, signer).await?);
            Ok(ensure_bm25(pile, source, signer).await?.report)
        })
    })
}

/// How far the two Archive search views lag the source in the one snapshot
/// both were attached from. Each view is derived by each writer for its own
/// commits, so the two may lag differently; a search reads what is present.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveSearchLag {
    /// The fact pair the results are joined against.
    pub facts: FactLag,
    /// Source commits the BM25 index has no leaf for yet.
    pub index: usize,
}

impl ArchiveSearchLag {
    /// Whether both views have caught up with every source commit the
    /// snapshot admits.
    pub const fn is_current(self) -> bool {
        self.facts.is_current() && self.index == 0
    }
}

/// Prepare the fact and search views from one snapshot. Both are derived
/// from the one Archive source and both are maintained here for the signer,
/// then attached from one snapshot. They need not stand for the same source
/// commits: another writer's commit reaches each view when that writer
/// derives it, and a commit landing between the two passes reaches one view
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
    CollectionSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>,
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
    CollectionSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>,
    archive_bm25::ArchiveBM25View,
    ArchiveSearchLag,
)> {
    storage.with_pile(|pile, signer| {
        pollster::block_on(async {
            let source = open_configured(pile, schema::DEFAULT_SCOPE_ID, signer.verifying_key())?;
            let target = bm25_target(pile, source, signer)?;
            let (succinct, rank9) = fact_views(pile, source)?;
            drop(ensure_facts(pile, source, signer).await?);
            crate::storage::tolerate_own_lag(
                pile.maintain_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(target, signer)
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
                .collection(rank9)
                .context("attach Archive search facts")?;
            let search = after
                .collection(target)
                .context("attach Archive BM25 cover")?;
            let lag = ArchiveSearchLag {
                facts: FactLag::of(&after, source, succinct, rank9)?,
                index: crate::storage::underived(&after, source, target)
                    .context("count Archive commits without a BM25 leaf")?,
            };
            let index = search
                .view::<archive_bm25::ArchiveBM25View>()
                .context("read Archive BM25 cover")?;
            Ok((facts, index, lag))
        })
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

    /// The derived BM25 collection over that root.
    fn test_target(
        store: &mut Pile,
        source: Collection<SimpleArchive>,
        pile: &std::path::Path,
        key: &std::path::Path,
    ) -> Collection<PortableBM25Blob> {
        store
            .derive_with(
                source,
                archive_bm25::ArchiveBlockTextBm25Mapping,
                crate::collection_names::private_policy(test_authority(pile, key)),
            )
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
        let mut writer = pollster::block_on(ArchiveImportWriter::open(pile, Some(key))).unwrap();
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
    fn facts_stand_on(
        observed: &CollectionSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>,
        commit: &CollectionCommit,
    ) -> bool {
        let snapshot = observed.snapshot();
        let source = Collection::<SimpleArchive>::open(snapshot, commit.collection()).unwrap();
        let rank9 = observed.cover().collection();
        // Rank9's descriptor names its source, the pair's Succinct view.
        let succinct = triblespace::core::collection::derived_from(snapshot, source.handle())
            .unwrap()
            .into_iter()
            .find(|derived| derived.handle == rank9.handle())
            .map(|derived| Collection::<SuccinctArchiveBlob>::open(snapshot, derived.source))
            .expect("the fact pair is listed")
            .unwrap();
        snapshot
            .collection(source)
            .unwrap()
            .support()
            .unwrap()
            .contains(Handle::<SimpleArchive>::from_hash(commit.data()))
            && FactLag::of(snapshot, source, succinct, rank9)
                .unwrap()
                .is_current()
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
        let mut writer = pollster::block_on(ArchiveImportWriter::open(&pile, Some(&key))).unwrap();
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
        assert!(before.support().unwrap().is_empty());
        assert!(before
            .view::<FactArchive>()
            .unwrap()
            .iter()
            .next()
            .is_none());
        drop(before);

        let commit = writer.finish(Ok(())).unwrap().1.unwrap();
        let after = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(after.support().unwrap().len(), 1);
        assert!(facts_stand_on(&after, &commit));
        assert_eq!(
            projection_ids(&after.view::<FactArchive>().unwrap()).len(),
            1
        );
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

        let mut writer = pollster::block_on(ArchiveImportWriter::open(&pile, Some(&key))).unwrap();
        writer.stage_fragment(fragment).unwrap();
        let error = writer
            .finish::<()>(Err(anyhow!("source projection failed")))
            .unwrap_err();
        assert_eq!(error.to_string(), "source projection failed");

        // `finish` closed the writer even on source failure. Reopening is
        // sound, no semantic edge escaped, and the dependency is merely an
        // unreachable content-addressed record available for later GC.
        let snapshot = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert!(snapshot.support().unwrap().is_empty());
        assert!(snapshot
            .view::<FactArchive>()
            .unwrap()
            .iter()
            .next()
            .is_none());
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

        let mut writer = pollster::block_on(ArchiveImportWriter::open(&pile, Some(&key))).unwrap();
        writer.stage_fragment(fragment.clone()).unwrap();
        assert!(writer.commit_unit().unwrap().is_some());
        // The reader prepares the projection; the writer only commits.
        let policy = writer
            .collection
            .policy(&writer.pile.snapshot().unwrap())
            .unwrap();
        let succinct = writer
            .pile
            .derive::<SuccinctArchiveBlob>(writer.collection, (), policy.clone())
            .unwrap();
        let rank9 = writer
            .pile
            .derive::<Rank9AcceleratedSuccinctArchiveBlob>(succinct, (), policy)
            .unwrap();
        let prepared = {
            let signer = writer.signer.clone();
            let pile = &mut writer.pile;
            pollster::block_on(async {
                drop(pile.maintain(succinct, &signer).await.unwrap());
                pile.maintain(rank9, &signer).await
            })
            .unwrap()
        };
        let prepared_facts = prepared
            .collection(rank9)
            .unwrap()
            .view::<FactArchive>()
            .unwrap();
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
        assert_eq!(snapshot.support().unwrap().len(), 2);
        let facts = snapshot.view::<FactArchive>().unwrap();
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

        let mut retry = pollster::block_on(ArchiveImportWriter::open(&pile, Some(&key))).unwrap();
        retry.stage_fragment(fragment).unwrap();
        retry.stage_fragment(annotation).unwrap();
        assert_eq!(retry.delta_len(), 0);
        assert!(retry.finish(Ok(())).unwrap().1.is_none());
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
        let mut writer = pollster::block_on(ArchiveImportWriter::open(&pile, Some(&key))).unwrap();
        writer.stage_fragment(fragment.clone()).unwrap();
        let (_, first) = writer.finish(Ok(())).unwrap();
        let first = first.unwrap();
        let mut retry = pollster::block_on(ArchiveImportWriter::open(&pile, Some(&key))).unwrap();
        let length = std::fs::metadata(&pile).unwrap().len();
        retry.stage_fragment(fragment).unwrap();
        let (_, repeated) = retry.finish(Ok(())).unwrap();
        assert_eq!(repeated, None);
        assert_eq!(std::fs::metadata(&pile).unwrap().len(), length);

        let snapshot = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(snapshot.support().unwrap().len(), 1);
        assert!(facts_stand_on(&snapshot, &first));
        assert_eq!(
            projection_ids(&snapshot.view::<FactArchive>().unwrap()).len(),
            1
        );
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
        assert_eq!(snapshot.support().unwrap().len(), 1);
        assert!(facts_stand_on(&snapshot, &admitted));
        assert_eq!(
            projection_ids(&snapshot.view::<FactArchive>().unwrap()).len(),
            1
        );
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
        let mut writer = pollster::block_on(ArchiveImportWriter::open(&pile, Some(&key))).unwrap();
        writer.stage_fragment(fragment).unwrap();
        writer.finish(Ok(())).unwrap();

        let archive = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        let facts = archive.view::<FactArchive>().unwrap();
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
            pollster::block_on(ArchiveImportWriter::open(&pile, Some(&key))).unwrap();
        first_writer.stage_fragment(first_fragment.clone()).unwrap();
        let first = first_writer.finish(Ok(())).unwrap().1.unwrap();

        let second_fragment = projection("session:two", "shared");
        let mut second_writer =
            pollster::block_on(ArchiveImportWriter::open(&pile, Some(&key))).unwrap();
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
        assert_eq!(snapshot.support().unwrap().len(), 2);
        assert_eq!(
            projection_ids(&snapshot.view::<FactArchive>().unwrap()).len(),
            2
        );
        assert!(snapshot.view::<FactArchive>().unwrap().iter().count() > first_len);
    }

    #[test]
    fn zero_commit_search_uses_the_canonical_empty_resident_index() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let report = pollster::block_on(ensure_bm25_index(&pile_path, Some(&key))).unwrap();
        assert_eq!((report.source_elements, report.cover_segments), (0, 0));

        let search = pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert!(search.0.support().unwrap().is_empty());
        assert!(search
            .1
            .query()
            .unwrap()
            .query_multi(&hash_tokens("anything"))
            .is_empty());
    }

    #[test]
    fn empty_commit_receives_exact_empty_derives_and_keeps_reads_empty() {
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

        let succinct = pollster::block_on(ensure_succinct_index(&pile_path, Some(&key))).unwrap();
        let bm25 = pollster::block_on(ensure_bm25_index(&pile_path, Some(&key))).unwrap();
        assert_eq!(succinct.source_elements, 1);
        assert_eq!((bm25.source_elements, bm25.cover_segments), (1, 1));

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let records = {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot).unwrap()
        };
        let derives: Vec<_> = records
            .derives()
            .iter()
            .filter(|claim| {
                claim.input() == triblespace::core::collection::SourceLocator::of(commit.data().raw)
            })
            .collect();
        assert_eq!(
            derives.len(),
            2,
            "one empty leaf per mapping over the source"
        );
        pile.close().unwrap();

        let snapshot = pollster::block_on(ensure_local(&pile_path, Some(&key))).unwrap();
        assert_eq!(snapshot.support().unwrap().len(), 1);
        assert!(facts_stand_on(&snapshot, &commit));
        assert!(projection_ids(&snapshot.view::<FactArchive>().unwrap()).is_empty());
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
            pollster::block_on(ArchiveImportWriter::open(&pile_path, Some(&key))).unwrap();
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
        let facts = snapshot.view::<FactArchive>().unwrap();
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
            pollster::block_on(ArchiveImportWriter::open(&pile_path, Some(&key))).unwrap();
        // Deliberately stage out of causal and temporal order. The collection
        // is a set; replay order must come solely from canonical semantics.
        writer.stage_fragment(regressed_child).unwrap();
        writer.stage_fragment(independent).unwrap();
        writer.stage_fragment(untimed).unwrap();
        writer.stage_fragment(parent).unwrap();
        writer.finish(Ok(())).unwrap();

        let snapshot = pollster::block_on(ensure_local(&pile_path, Some(&key))).unwrap();
        let facts = snapshot.view::<FactArchive>().unwrap();
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
            pollster::block_on(ArchiveImportWriter::open(&pile_path, Some(&key))).unwrap();
        writer
            .stage_fragment(projection("session:index", "exact succinct"))
            .unwrap();
        writer.finish(Ok(())).unwrap();

        let report = pollster::block_on(ensure_succinct_index(&pile_path, Some(&key))).unwrap();

        assert_eq!(report.source_elements, 1);
        let length = std::fs::metadata(&pile_path).unwrap().len();
        assert_eq!(
            pollster::block_on(ensure_succinct_index(&pile_path, Some(&key))).unwrap(),
            report
        );
        assert_eq!(std::fs::metadata(&pile_path).unwrap().len(), length);

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let authority = load_signer(&pile_path, Some(&key)).unwrap().verifying_key();
        let source = test_source(&mut pile, &pile_path, &key);
        let raw_target = pile
            .derive::<SuccinctArchiveBlob>(
                source,
                (),
                crate::collection_names::private_policy(authority),
            )
            .unwrap();
        let records = {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot).unwrap()
        };
        let derive = records
            .derives()
            .iter()
            .find(|derive| derive.collection() == raw_target.handle())
            .copied()
            .expect("stored Archive raw-Succinct DERIVE");
        // The leaf names its source commit by locator, which is not a
        // fetchable handle: the commit it stands for is the one whose
        // payload has that locator.
        let commit = records
            .commits()
            .iter()
            .find(|commit| {
                triblespace::core::collection::SourceLocator::of(commit.data().raw)
                    == derive.input()
            })
            .copied()
            .expect("the leaf names an Archive commit");
        let reader = pile.snapshot().unwrap();
        let output = derive.output();
        let input: Blob<SimpleArchive> = reader
            .get(Handle::<SimpleArchive>::from_hash(commit.data()))
            .unwrap();
        let output: Blob<SuccinctArchiveBlob> = reader
            .get(Handle::<SuccinctArchiveBlob>::from_hash(output))
            .unwrap();
        let expected =
            <SuccinctArchiveBlob as CollectionDerivation>::map(&(), &input, &reader).unwrap();
        assert_eq!(expected.get_handle(), output.get_handle());
        pile.close().unwrap();
    }

    /// Each own commit gets one BM25 leaf. With no source merge to mirror,
    /// the index reads its two leaves as two segments, stands for both
    /// commits, and a repeat publishes nothing.
    #[test]
    fn bm25_uses_per_commit_leaves_and_reads_them_unmerged() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        for (locator, text) in [("session:alpha", "alpha"), ("session:beta", "beta")] {
            let mut writer =
                pollster::block_on(ArchiveImportWriter::open(&pile_path, Some(&key))).unwrap();
            writer.stage_fragment(projection(locator, text)).unwrap();
            writer.finish(Ok(())).unwrap();
        }

        let report = pollster::block_on(ensure_bm25_index(&pile_path, Some(&key))).unwrap();

        assert_eq!(report.source_elements, 2);
        assert_eq!(report.lagging, 0);
        assert_eq!(report.cover_segments, 2);
        let length = std::fs::metadata(&pile_path).unwrap().len();
        assert_eq!(
            pollster::block_on(ensure_bm25_index(&pile_path, Some(&key))).unwrap(),
            report
        );
        assert_eq!(std::fs::metadata(&pile_path).unwrap().len(), length);

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let target = test_target(&mut pile, source, &pile_path, &key);
        let records = {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot).unwrap()
        };
        let derives: Vec<_> = records
            .derives()
            .iter()
            .filter(|claim| claim.collection() == target.handle())
            .copied()
            .collect();
        let merges: Vec<_> = records
            .merges()
            .iter()
            .filter(|claim| claim.collection() == target.handle())
            .copied()
            .collect();
        assert_eq!(derives.len(), 2);
        assert!(merges.is_empty());
        let store_snapshot = pile.snapshot().unwrap();
        let source_view = store_snapshot.collection(source).unwrap();
        let attached = store_snapshot.collection(target).unwrap();
        assert!(attached.missing_from(&source_view).unwrap().is_empty());
        assert_eq!(attached.cover().len(), 2);
        drop((source_view, attached));
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
                pollster::block_on(ArchiveImportWriter::open(&pile_path, Some(&key))).unwrap();
            writer
                .stage_fragment(projection_at(locator, "shared closure needle", seconds))
                .unwrap();
            writer.finish(Ok(())).unwrap();
        }

        let report = pollster::block_on(ensure_bm25_index(&pile_path, Some(&key))).unwrap();
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
            pollster::block_on(ArchiveImportWriter::open(&pile_path, Some(&key))).unwrap();
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
            pollster::block_on(ArchiveImportWriter::open(&pile_path, Some(&key))).unwrap();
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
            pollster::block_on(ArchiveImportWriter::open(&pile_path, Some(&key))).unwrap();
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
    /// leaf of its own and a DERIVE names one source foundation, so no leaf
    /// sees both halves, and mirroring a merge of the two needs both leaves
    /// first: this law cannot index the block, now or later. That is the
    /// block's lag and nobody else's. Every other commit is still derived,
    /// search reads around the block and says it lags, explicit maintenance
    /// names it with the mapping's reason, and a repeat publishes nothing.
    #[test]
    fn bm25_leaves_a_split_block_as_lag_and_derives_every_other_commit() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let (block_element, remainder_element) =
            projection_split_across_source_elements("session:split", "closure needle");
        let signer = load_signer(&pile_path, Some(&key)).unwrap();
        let mut pile = open_pile_strict(&pile_path).unwrap();
        let collection =
            open_configured(&mut pile, schema::DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        let block_commit = pile.commit(collection, &signer, block_element).unwrap();
        let remainder_commit = pile.commit(collection, &signer, remainder_element).unwrap();
        let target = test_target(&mut pile, collection, &pile_path, &key);
        pile.close().unwrap();
        // A whole commit after the split one: derived like any other.
        commit_projection(&pile_path, &key, "session:whole", "whole haystack");

        let archive = pollster::block_on(ensure_local(&pile_path, Some(&key))).unwrap();
        assert_eq!(archive.support().unwrap().len(), 3);
        assert_eq!(
            projection_ids(&archive.view::<FactArchive>().unwrap()).len(),
            2
        );
        drop(archive);

        let report = pollster::block_on(ensure_bm25_index(&pile_path, Some(&key))).unwrap();
        assert_eq!(report.source_elements, 3);
        assert_eq!(report.lagging, 1, "only the split block lags");
        let length = std::fs::metadata(&pile_path).unwrap().len();
        assert_eq!(
            pollster::block_on(ensure_bm25_index(&pile_path, Some(&key))).unwrap(),
            report
        );
        assert_eq!(
            std::fs::metadata(&pile_path).unwrap().len(),
            length,
            "a refused leaf publishes nothing on a later pass either"
        );

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let records = discovered_records(&pile.snapshot().unwrap()).unwrap();
        let has_leaf = |commit: &CollectionCommit| {
            let locator = triblespace::core::collection::SourceLocator::of(commit.data().raw);
            records
                .derives()
                .iter()
                .any(|derive| derive.collection() == target.handle() && derive.input() == locator)
        };
        assert!(!has_leaf(&block_commit));
        assert!(has_leaf(&remainder_commit));
        assert_eq!(
            records
                .derives()
                .iter()
                .filter(|derive| derive.collection() == target.handle())
                .count(),
            2
        );
        let error = pollster::block_on(
            pile.maintain_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(target, &signer),
        )
        .err()
        .expect("explicit maintenance names the block it cannot derive");
        let CollectionRealizationError::Unmappable { blocked } = error else {
            panic!("expected Unmappable, got {error}");
        };
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].0, block_commit.data());
        assert!(
            blocked[0].1.contains("references absent part"),
            "{}",
            blocked[0].1
        );
        pile.close().unwrap();

        let (_, index, lag) =
            pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert_eq!(lag.index, 1);
        assert!(lag.facts.is_current());
        let query = index.query().unwrap();
        assert_eq!(query.query_multi(&hash_tokens("whole haystack")).len(), 1);
        assert!(query.query_multi(&hash_tokens("closure needle")).is_empty());
    }

    /// Eight own commits fill the root's lowest tier, so the root carry joins
    /// them with one 8-input MERGE. BM25 maintenance derives the eight leaves
    /// and mirrors that merge exactly once: one target MERGE over the eight
    /// leaf images, its result the mapping of the merged source node's own
    /// bytes. The index then reads as one segment that finds every commit,
    /// and a repeat pass publishes nothing.
    #[test]
    fn bm25_mirrors_an_own_root_merge_once_and_reads_it_as_one_segment() {
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

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let target = test_target(&mut pile, source, &pile_path, &key);
        drop(pollster::block_on(pile.maintain(source, &signer)).unwrap());
        let records = discovered_records(&pile.snapshot().unwrap()).unwrap();
        let root_merges: Vec<_> = records
            .merges()
            .iter()
            .filter(|merge| merge.collection() == source.handle())
            .copied()
            .collect();
        assert_eq!(root_merges.len(), 1, "one carry of the full tier");
        assert_eq!(root_merges[0].inputs().len(), words.len());

        let bm25_records = |pile: &mut Pile| {
            let records = discovered_records(&pile.snapshot().unwrap()).unwrap();
            let derives = records
                .derives()
                .iter()
                .filter(|derive| derive.collection() == target.handle())
                .count();
            let merges: Vec<_> = records
                .merges()
                .iter()
                .filter(|merge| merge.collection() == target.handle())
                .copied()
                .collect();
            (derives, merges)
        };
        drop(
            pollster::block_on(
                pile.maintain_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(target, &signer),
            )
            .unwrap(),
        );
        let (derives, merges) = bm25_records(&mut pile);
        assert_eq!(derives, words.len());
        assert_eq!(merges.len(), 1, "the own root merge is mirrored once");
        assert_eq!(merges[0].inputs().len(), words.len());
        let snapshot = pile.snapshot().unwrap();
        let merged: Blob<SimpleArchive> = snapshot
            .get(Handle::<SimpleArchive>::from_hash(root_merges[0].result()))
            .unwrap();
        let expected = archive_bm25::derive_element(&snapshot, merged).unwrap();
        assert_eq!(
            Handle::<PortableBM25Blob>::to_hash(expected.get_handle()),
            merges[0].result(),
            "the mirror's image is the merged source node's own image"
        );
        let attached = snapshot.collection(target).unwrap();
        assert_eq!(attached.cover().len(), 1);
        assert_eq!(
            crate::storage::underived(&snapshot, source, target).unwrap(),
            0
        );
        drop((attached, snapshot));

        let length = std::fs::metadata(&pile_path).unwrap().len();
        drop(
            pollster::block_on(
                pile.maintain_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(target, &signer),
            )
            .unwrap(),
        );
        assert_eq!(bm25_records(&mut pile), (derives, merges));
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

    /// A new commit costs the index exactly one new leaf, the index stands
    /// for every commit after each pass, and a complete repeat publishes no
    /// record. With no source merge to mirror, each leaf is its own segment.
    #[test]
    fn bm25_maintenance_derives_only_the_new_leaf_and_repeats_without_work() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile_path, &key);
        let bm25_derives = |pile: &mut Pile, target: Collection<PortableBM25Blob>| {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot)
                .unwrap()
                .derives()
                .iter()
                .filter(|claim| claim.collection() == target.handle())
                .count()
        };
        let maintain_fresh = |pile: &mut Pile, source, target, segments: usize| {
            let maintained = pollster::block_on(
                pile.maintain_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(target, &signer),
            )
            .unwrap();
            let source_view = maintained.collection(source).unwrap();
            let index = maintained.collection(target).unwrap();
            assert!(index.missing_from(&source_view).unwrap().is_empty());
            assert_eq!(index.cover().len(), segments);
        };

        commit_projection(&pile_path, &key, "session:first", "first residual");
        let mut pile = open_pile_strict(&pile_path).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let target = test_target(&mut pile, source, &pile_path, &key);
        maintain_fresh(&mut pile, source, target, 1);
        assert_eq!(bm25_derives(&mut pile, target), 1);
        pile.close().unwrap();

        commit_projection(&pile_path, &key, "session:second", "second residual");
        let mut pile = open_pile_strict(&pile_path).unwrap();
        maintain_fresh(&mut pile, source, target, 2);
        assert_eq!(
            bm25_derives(&mut pile, target),
            2,
            "only the new commit is derived"
        );
        let records_before = {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot).unwrap()
        };
        let counts_before = (
            records_before.derives().len(),
            records_before.merges().len(),
        );
        maintain_fresh(&mut pile, source, target, 2);
        let records_after = {
            let store_snapshot = pile.snapshot().unwrap();
            discovered_records(&store_snapshot).unwrap()
        };
        assert_eq!(
            (records_after.derives().len(), records_after.merges().len()),
            counts_before,
            "a complete retry publishes no collection records"
        );
        pile.close().unwrap();
    }

    #[test]
    fn maintenance_recovers_a_pending_derive_with_a_missing_output() {
        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile_path, &key);
        let commit = commit_projection(&pile_path, &key, "session:pending", "recover output");

        let mut pile = open_pile_strict(&pile_path).unwrap();
        let source = test_source(&mut pile, &pile_path, &key);
        let target = test_target(&mut pile, source, &pile_path, &key);
        let store_snapshot = pile.snapshot().unwrap();
        let input: Blob<SimpleArchive> = store_snapshot
            .get(Handle::<SimpleArchive>::from_hash(commit.data()))
            .unwrap();
        let output = archive_bm25::derive_element(&store_snapshot, input).unwrap();
        let output_data = Handle::<PortableBM25Blob>::to_hash(output.get_handle());
        let pending = CollectionDerive::sign(
            &signer,
            target.handle(),
            triblespace::core::collection::SourceLocator::of(commit.data().raw),
            output_data,
        );
        drop(output);
        drop(store_snapshot);
        CollectionStore::insert(&mut pile, CollectionRecord::Derive(pending)).unwrap();
        assert!(pile
            .snapshot()
            .unwrap()
            .metadata(Handle::<PortableBM25Blob>::from_hash(output_data))
            .unwrap()
            .is_none());

        let ready_snapshot = pollster::block_on(
            pile.maintain_with::<archive_bm25::ArchiveBlockTextBm25Mapping>(target, &signer),
        )
        .unwrap();
        let ready = ready_snapshot.collection(target).unwrap();
        let source_view = ready_snapshot.collection(source).unwrap();
        assert!(ready.missing_from(&source_view).unwrap().is_empty());
        drop(source_view);
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
        assert_eq!(
            records
                .derives()
                .iter()
                .filter(|claim| **claim == pending)
                .count(),
            1,
            "the recovered deterministic equation remains one record"
        );
        pile.close().unwrap();
    }
    #[test]
    fn exact_fact_cover_and_raw_export_need_no_outer_commits() {
        use triblespace::core::collection::CollectionDerivation;

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
        let policy = source.policy(&pile.snapshot().unwrap()).unwrap();
        let succinct = pile
            .derive::<SuccinctArchiveBlob>(source, (), policy.clone())
            .unwrap();
        let rank9 = pile
            .derive::<Rank9AcceleratedSuccinctArchiveBlob>(succinct, (), policy)
            .unwrap();
        let (_, facts, _metadata, blobs) = fragment.into_parts();
        stage_embedded_blobs(&mut pile, embedded_blobs(blobs)).unwrap();
        let data = pile.put::<SimpleArchive, _>(facts).unwrap();
        // Raw value construction needs no COMMIT authority. It does not,
        // however, manufacture admitted support or signed collection records.
        let before = pile.snapshot().unwrap();
        let raw: Blob<SimpleArchive> = before.get(data).unwrap();
        let compact = SuccinctArchiveBlob::map(&(), &raw, &before).unwrap();
        pile.put::<SuccinctArchiveBlob, _>(compact.clone()).unwrap();
        let accelerated =
            Rank9AcceleratedSuccinctArchiveBlob::map(&(), &compact, &pile.snapshot().unwrap())
                .unwrap();
        let member = pile
            .put::<Rank9AcceleratedSuccinctArchiveBlob, _>(accelerated)
            .unwrap();
        let after = pile.snapshot().unwrap();
        assert!(after.collection(rank9).unwrap().cover().is_empty());
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
}
