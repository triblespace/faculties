//! Collection-native Archive runtime over the V4 descriptor-handle calculus.
//!
//! Archive authorship has one durable Ed25519 signer and one fixed canonical
//! SimpleArchive-union descriptor. Imports cut each source's projected nodes
//! into content-defined pieces and publish one signed COMMIT per piece,
//! without reading anything the Archive holds; a piece published before is
//! the same bytes and the same record again, and costs nothing. Reads
//! snapshot that same collection, carrying it and attaching its views as
//! they go; there is no Repository branch, CAS head, sidecar registry, or
//! fallback identity.

#[cfg(test)]
use crate::storage::FactView;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use triblespace::core::collection::AttachedSnapshot;

use anybytes::Bytes;
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::blob::encodings::{simplearchive::SimpleArchive, UnknownBlob};
use triblespace::core::blob::{Blob, IntoBlob};
use triblespace::core::collection::{
    Collection, CollectionCommit, CollectionData, CollectionSnapshotExt, CollectionStoreExt,
};
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::async_store::{AsyncBlobStoreAcquire, AsyncBlobStoreGet};
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::{BlobStoreGet, BlobStorePut, SnapshotSource, StorageClose, Store};
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
use triblespace::core::collection::{CollectionAttachment, CollectionMap};
use triblespace::core::collection::{CollectionRecord, CollectionStore};
use triblespace::core::repo::BlobStoreMeta;

type RawHandle = Inline<Handle<RawBytes>>;

/// Stage Archive fragments and publish them as content-defined pieces.
///
/// Supplied facts remain open-world relations, including opaque ids and further
/// annotations. Publication does not require a closed-world catalog decode, and
/// it reads nothing the Archive already holds: an import is a pure function
/// from the source to COMMITs, and the collection is their union.
///
/// The fragments of one source unit arrive in the importer's content-determined
/// order, one whole node per fragment. The writer cuts that stream into
/// pieces at boundaries the content chooses: a piece ends after any fragment
/// holding a node whose id has at least [`PIECE_BITS`] trailing zero bits, and
/// at the end of the unit. The same nodes therefore always cut into the same
/// pieces, a piece is always the same bytes, and its signed COMMIT is the same
/// record (deterministic Ed25519 over the collection, the data hash and the
/// metadata hash, with no time in it). Publishing a piece the pile already
/// holds appends nothing: blob puts and record inserts both return early on a
/// resident value. Re-importing an unchanged source costs nothing, and a grown
/// one costs the pieces from the first changed one on.
///
/// Every piece, the unit's last one included, is two COMMITs: the identity
/// of its nodes, as the importers name it when they build them
/// ([`blockdag::Identity`]), and everything that annotates them (timestamps,
/// receipts with their times, roles, models, session ids, a subagent's spawn
/// metadata, recovered attachment bytes). The same nodes read from a fork's
/// replay, from an export with or without its sidecar files, or with or
/// without a subagent's metadata have the same identity COMMIT; only the
/// annotation COMMIT differs. A unit therefore costs at least two COMMITs.
///
/// Piece COMMITs carry empty metadata. The Archive's attribute descriptions,
/// without anything that varies with the build, are the metadata of the
/// vocabulary COMMIT ([`blockdag::attribute_descriptions`]), published before
/// a writer's first piece and the same bytes in every writer. Metafacts that
/// fragments carry are not published.
pub struct ArchiveImportWriter<P = FacultyStore> {
    pile: P,
    collection: Collection<SimpleArchive>,
    signer: SigningKey,
    /// Node identities of the piece being cut.
    core: TribleSet,
    /// Annotations of the piece being cut.
    occurrence: TribleSet,
    /// Whether the vocabulary COMMIT is still to be prepared.
    vocabulary: bool,
    /// Pieces closed in the unit being staged.
    pieces: usize,
    /// Signed COMMITs of the unit's pieces, in insertion order, whose bytes
    /// are already appended; the records are inserted when the unit ends.
    pending: Vec<CollectionCommit>,
    /// Trailing zero bits of a node id that end a piece.
    piece_bits: u32,
    /// Keeps the runtime the descriptor was acquired through alive until close.
    #[allow(dead_code)]
    runtime: Arc<tokio::runtime::Runtime>,
}

/// What one source unit published: how many pieces it cut and the COMMITs it
/// inserted, in insertion order. A COMMIT the pile already held is inserted
/// again at no cost, so these count work, not growth; the pile's growth is
/// the caller's to measure.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UnitPublication {
    pub pieces: usize,
    pub commits: Vec<CollectionCommit>,
}

impl UnitPublication {
    /// The last COMMIT inserted, if the unit published anything.
    pub fn last(&self) -> Option<CollectionCommit> {
        self.commits.last().copied()
    }

    /// Whether the unit staged nothing.
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }
}

/// Trailing zero bits of a node id that end a piece: on average one piece per
/// 2^`PIECE_BITS` nodes.
///
/// The larger a piece, the more a growing source republishes on every import
/// (its unclosed tail) and the more of a fork's prefix its first new piece
/// repeats; the smaller, the more COMMITs the collection holds and the more
/// of them every carry and attachment pass visits. Measured on sky,
/// 2026-10-18, projecting 38 Codex rollouts (11.2 GB, the largest 4.4 GB) and
/// 31 Claude Code transcripts (5.9 GB) without a pile: at 11 bits a piece
/// held 1,961 and 1,732 nodes on average, the largest piece 19,757 nodes, and
/// the median unclosed tail 141 and 104 nodes (about 15 tribles per node).
pub const PIECE_BITS: u32 = 11;

/// Whether `node` ends a piece under `bits` trailing zero bits.
fn ends_piece(node: Id, bits: u32) -> bool {
    let raw: [u8; 16] = node.into();
    u128::from_be_bytes(raw).trailing_zeros() >= bits
}

impl ArchiveImportWriter {
    /// Open a synchronous import session. Async callers run the complete
    /// open/stage/finish lifetime on a blocking worker, not inside their runtime.
    pub fn open(pile_path: &std::path::Path, key_path: Option<&std::path::Path>) -> Result<Self> {
        #[cfg(test)]
        OPENS.with(|opens| opens.set(opens.get() + 1));
        let signer = crate::storage::load_signer(pile_path, key_path)?;
        let runtime = Arc::new(crate::storage::runtime()?);
        let mut pile = crate::storage::open_store_as(pile_path, signer.verifying_key())?;
        match Self::prepare(&mut pile, &signer, &runtime, None) {
            Ok(collection) => Ok(Self::new(pile, collection, signer, runtime)),
            Err(error) => close_pile(
                pile,
                Err(error),
                "closing Archive pile after failed open also failed",
            ),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// How often [`ArchiveImportWriter::open`] ran on this thread, so a test
    /// can pin how many times one command opens the pile.
    pub(crate) static OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl<P> ArchiveImportWriter<P>
where
    P: Store + AsyncBlobStoreAcquire + Send,
    P::Snapshot: AsyncBlobStoreGet,
{
    /// Borrow a caller-owned store without taking its local-backend guard.
    /// The caller keeps the transport alive until the last commit completes.
    pub fn from_store(
        pile: P,
        signer: &SigningKey,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self> {
        Self::from_store_selected(pile, signer, runtime, None)
    }

    /// Import into the exact immutable workspace selection.
    pub fn from_store_with_storage(
        pile: P,
        signer: &SigningKey,
        runtime: Arc<tokio::runtime::Runtime>,
        storage: &crate::storage::Storage,
    ) -> Result<Self> {
        Self::from_store_selected(pile, signer, runtime, Some(storage))
    }

    fn from_store_selected(
        mut pile: P,
        signer: &SigningKey,
        runtime: Arc<tokio::runtime::Runtime>,
        storage: Option<&crate::storage::Storage>,
    ) -> Result<Self> {
        let collection = Self::prepare(&mut pile, signer, &runtime, storage)?;
        Ok(Self::new(pile, collection, signer.clone(), runtime))
    }

    fn new(
        pile: P,
        collection: Collection<SimpleArchive>,
        signer: SigningKey,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Self {
        Self {
            pile,
            collection,
            signer,
            core: TribleSet::new(),
            occurrence: TribleSet::new(),
            vocabulary: true,
            pieces: 0,
            pending: Vec::new(),
            piece_bits: PIECE_BITS,
            runtime,
        }
    }

    /// Open the Archive root for writing. Only its descriptor is read; no
    /// commit, merge or attachment of the Archive is.
    fn prepare(
        pile: &mut P,
        signer: &SigningKey,
        runtime: &Arc<tokio::runtime::Runtime>,
        storage: Option<&crate::storage::Storage>,
    ) -> Result<Collection<SimpleArchive>> {
        match storage {
            Some(storage) => {
                storage.open_collection_read(
                    pile,
                    schema::DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                    runtime,
                )?;
                storage.open_collection_write(
                    pile,
                    schema::DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                    runtime,
                )
            }
            None => crate::collection_names::open_configured_acquiring(
                pile,
                schema::DEFAULT_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            ),
        }
    }

    /// Exact descriptor selected when this writer opened, for local progress binding.
    pub fn collection_handle(&self) -> triblespace::core::collection::records::CollectionHandle {
        self.collection.handle()
    }

    /// Add one fragment to the piece being cut, with the part of it that is
    /// node identity, and end the piece when one of those nodes says so.
    ///
    /// A fragment is the importers' unit: one whole node with its receipt,
    /// or annotations of a receipt. `identity` is what the builder that made
    /// the nodes named as their identity ([`blockdag::block_parts`]); every
    /// other fact of the fragment annotates. The writer classifies nothing.
    /// Only a fragment holding a node ends a piece, so a piece never splits a
    /// node, and the annotations an importer emits after its nodes (the
    /// leaves' session id and a subagent's spawn metadata) ride in the unit's
    /// last piece. Nothing is compared with what the Archive holds, or with
    /// what this writer staged before: the union absorbs every repetition.
    pub fn stage(&mut self, fragment: Fragment, identity: blockdag::Identity) -> Result<()> {
        let (_, facts, _, blobs) = fragment.into_parts();

        // Embedded payloads can dominate an import's resident memory. Append
        // each already-constructed content-addressed dependency immediately,
        // keeping payload bytes out of the piece. They remain semantically
        // unreachable until a signed COMMIT names the facts which reference
        // them. Blobs only the dropped metafacts name (attribute descriptions,
        // whose text varies with the build) are not appended.
        stage_embedded_blobs(&mut self.pile, named_blobs(&facts, blobs))?;

        self.occurrence += facts.difference(identity.facts());
        let ends = identity
            .nodes()
            .iter()
            .any(|node| ends_piece(*node, self.piece_bits));
        self.core += identity.into_facts();
        if ends {
            self.publish_piece()?;
        }
        Ok(())
    }

    /// Stage a fragment of annotations alone: it names no node, so it ends no
    /// piece, and all of it goes into the annotation COMMIT.
    pub fn stage_annotations(&mut self, fragment: Fragment) -> Result<()> {
        self.stage(fragment, blockdag::Identity::none())
    }

    /// Cut pieces at `bits` trailing zero bits instead of [`PIECE_BITS`].
    #[cfg(test)]
    pub(crate) fn set_piece_bits(&mut self, bits: u32) {
        self.piece_bits = bits;
    }

    /// Facts staged in the piece being cut.
    pub fn delta_len(&self) -> usize {
        self.core.len() + self.occurrence.len()
    }

    /// End ONE source unit: publish its last piece, then insert every
    /// piece's signed COMMITs, and keep the pile open.
    ///
    /// A piece's bytes are appended when it closes, but no record names them
    /// until the unit ends, so a unit whose source fails publishes nothing
    /// ([`Self::close`] with an error inserts no record). The records go in
    /// one after another: the vocabulary's first, then for each piece its
    /// annotations before its node identities, so a node is never visible
    /// without its annotations, and its predecessors are always in an earlier
    /// piece. A process killed between two inserts leaves a prefix visible;
    /// rerunning the import inserts every record again, the present ones at
    /// no cost, and completes the unit.
    ///
    /// Nothing here carries the collection or attaches its views. Every
    /// read prepares what it reads (`maintain_attached` carries the source
    /// first), and on hosts running `trible pile collection maintain-all`
    /// over the Archive's Rank9 target both stay current in the background.
    pub fn commit_unit(&mut self) -> Result<UnitPublication> {
        self.publish_piece()?;
        let commits = std::mem::take(&mut self.pending);
        for commit in &commits {
            CollectionStore::insert(&mut self.pile, CollectionRecord::Commit(*commit))
                .map_err(|error| anyhow!("insert an Archive piece COMMIT: {error}"))?;
        }
        Ok(UnitPublication {
            pieces: std::mem::take(&mut self.pieces),
            commits,
        })
    }

    /// Prepare the piece being cut, if it holds anything: its annotation
    /// COMMIT, then its identity COMMIT.
    fn publish_piece(&mut self) -> Result<()> {
        let core = std::mem::take(&mut self.core);
        let occurrence = std::mem::take(&mut self.occurrence);
        if core.is_empty() && occurrence.is_empty() {
            return Ok(());
        }
        if self.vocabulary {
            self.vocabulary = false;
            let (_, facts, _, blobs) = blockdag::vocabulary_fragment().into_parts();
            stage_embedded_blobs(&mut self.pile, named_blobs(&facts, blobs))?;
            let (descriptions, description_blobs) = blockdag::attribute_descriptions();
            stage_embedded_blobs(&mut self.pile, description_blobs)?;
            self.prepare_commit(facts, descriptions)?;
        }
        self.pieces += 1;
        self.prepare_commit(occurrence, TribleSet::new())?;
        self.prepare_commit(core, TribleSet::new())
    }

    /// Append one COMMIT's data and metadata, if it holds facts, and sign it
    /// over the handles the store returned; the record waits for the unit's
    /// end.
    fn prepare_commit(&mut self, facts: TribleSet, metadata: TribleSet) -> Result<()> {
        if facts.is_empty() {
            return Ok(());
        }
        let data = self
            .pile
            .put::<SimpleArchive, _>(facts)
            .context("stage an Archive piece")?;
        let metadata = self
            .pile
            .put::<SimpleArchive, _>(metadata)
            .context("stage an Archive piece's metadata")?;
        self.pending.push(CollectionCommit::sign(
            &self.signer,
            self.collection.handle(),
            Handle::<SimpleArchive>::to_hash(data),
            metadata,
        ));
        Ok(())
    }
}

impl ArchiveImportWriter {
    /// Close the pile, ending the unit being staged first. After a failure
    /// nothing staged since the last unit ended is published.
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

    pub fn finish<T>(mut self, surrounding: Result<T>) -> Result<(T, UnitPublication)> {
        let result = surrounding.and_then(|value| {
            let publication = self.commit_unit()?;
            Ok((value, publication))
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

/// The embedded blobs some fact of `facts` names, in handle order. Blobs only
/// metafacts name (attribute descriptions, whose text varies with the build)
/// are left out.
fn named_blobs(
    facts: &TribleSet,
    blobs: triblespace::core::blob::MemoryBlobStore,
) -> Vec<Blob<UnknownBlob>> {
    let named: std::collections::HashSet<[u8; 32]> = facts
        .iter()
        .map(|fact| {
            let mut value = [0u8; 32];
            value.copy_from_slice(&fact.data[32..]);
            value
        })
        .collect();
    embedded_blobs(blobs)
        .into_iter()
        .filter(|blob| named.contains(&blob.get_handle().raw))
        .collect()
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
        let source = storage.open_collection_read(
            store,
            schema::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            runtime,
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
) -> Result<
    AttachedSnapshot<
        crate::storage::AcquiringReader<crate::storage::FacultySnapshot>,
        Rank9AcceleratedSuccinctArchiveBlob,
    >,
> {
    storage.with_store(|store, signer, runtime| {
        let source = storage.open_collection_read(
            store,
            schema::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            runtime,
        )?;
        let (succinct, rank9) = crate::storage::fact_pair(store, source)?;
        runtime.block_on(async {
            crate::storage::tolerate_own_lag(store.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(store.maintain_attached(rank9, signer).await)?;
            Ok::<_, anyhow::Error>(())
        })?;
        crate::storage::AcquiringReader::new(store.snapshot()?, runtime.clone())
            .attached_acquiring(rank9)
            .context("attach frozen Archive facts")
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
        let source = storage.open_collection_read(
            store,
            schema::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            runtime,
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
    pile.attach_with(source, archive_bm25::ArchiveTextBm25Mapping)
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
        pile.maintain_attached_with::<archive_bm25::ArchiveTextBm25Mapping>(target, signer)
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
        let source = storage.open_collection_read(
            store,
            schema::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            runtime,
        )?;
        let target = store.attach_with(source, archive_bm25::ArchiveTextBm25Mapping)?;
        let (succinct, rank9) = crate::storage::fact_pair(store, source)?;
        runtime.block_on(async {
            crate::storage::tolerate_own_lag(store.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(store.maintain_attached(rank9, signer).await)?;
            crate::storage::tolerate_own_lag(
                store
                    .maintain_attached_with::<archive_bm25::ArchiveTextBm25Mapping>(target, signer)
                    .await,
            )?;
            Ok::<_, anyhow::Error>(())
        })?;
        // What maintenance left unattached is the lag. It is counted, not
        // built again in memory to find out which of it could have been.
        let snapshot = crate::storage::AcquiringReader::new(store.snapshot()?, runtime.clone());
        let attached = snapshot.attached_acquiring(target)?;
        Ok(Bm25IndexReport {
            source_elements: snapshot.collection(source)?.support()?.len(),
            lagging: attached.residual().len(),
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
        let source = storage.open_collection_read(
            store,
            schema::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            runtime,
        )?;
        let mut local = store.store();
        let pile = &mut *local;
        pollster::block_on(async {
            let target = bm25_target(pile, source)?;
            let (succinct, rank9) = fact_views(pile, source)?;
            drop(ensure_facts(pile, source, signer).await?);
            crate::storage::tolerate_own_lag(
                pile.maintain_attached_with::<archive_bm25::ArchiveTextBm25Mapping>(target, signer)
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

/// Acquire a query's fixed Archive fact and BM25 observations without
/// maintaining either. Both views select their support at the same
/// watermark; exact-byte arrival below does not reselect either of them.
/// The surrounding operation owns the store.
///
/// The index is the attached BM25 cover alone. Its residual is reported as
/// lag and never built here: only `archive index` maintains this cover, so
/// the residual can be most of the Archive, and building it in a search
/// costs what maintaining it costs. On 2026-10-26 a search that maintained
/// first mapped a whole merge node of the Archive in the searching process
/// and grew to about 62 GB.
pub(crate) fn search_views_with_storage(
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
        let source = storage.open_collection_read(
            store,
            schema::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            runtime,
        )?;
        let target = store.attach_with(source, archive_bm25::ArchiveTextBm25Mapping)?;
        let (succinct, rank9) = crate::storage::fact_pair(store, source)?;
        let after = crate::storage::AcquiringReader::new(store.snapshot()?, runtime.clone());
        let facts = after.attached_acquiring(rank9)?;
        let search = after.attached_acquiring(target)?;
        let lag = ArchiveSearchLag {
            facts: FactLag::of(&after, source, succinct, rank9)?,
            index: search.residual().len(),
        };
        let index = search
            .view::<archive_bm25::ArchiveBM25View>()
            .context("read Archive BM25 cover")?;
        Ok((facts, index, lag))
    })
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
            .attach_with(source, archive_bm25::ArchiveTextBm25Mapping)
            .unwrap()
    }

    /// Initialize the durable signer used by the Archive fixture.
    fn initialize_archive_fixture(pile: &std::path::Path, key: &std::path::Path) -> SigningKey {
        initialize_signer(pile, Some(key)).unwrap()
    }

    use super::*;
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
                archive
                    .stage_fixture(projection("async-worker", "body"))
                    .unwrap();
                assert!(!archive.finish(Ok(())).unwrap().1.is_empty());

                let mut code =
                    crate::code::ingest::CodeImportWriter::open(&pile, Some(&key)).unwrap();
                code.stage_fragment(entity! { metadata::description: "code import worker" })
                    .unwrap();
                assert!(code.commit_unit().unwrap().is_some());
                code.close(Ok(())).unwrap();

                // Failed construction drops its owned runtime on the same
                // blocking worker, never on the async executor.
                let absent = directory.path().join("absent").join("pile");
                assert!(ArchiveImportWriter::open(&absent, Some(&key)).is_err());
            })
            .await
            .unwrap();
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
        )
        .next()
        .unwrap();
        assert!(!snapshot.contains_blob(handle).unwrap());

        writer.stage_fixture(fragment).unwrap();
        // A later exact byte is available through the still-owned Leech even
        // though the reader's facts, residency and proof observation are old.
        let fetched: View<str> = BlobStoreGet::get(&reader, handle).unwrap();
        assert_eq!(fetched.as_ref(), body);
        assert!(!reader.contains_blob(handle).unwrap());
        assert!(!writer.commit_unit().unwrap().is_empty());
        writer.close(Ok(())).unwrap();

        // Closing, not the captured reader, owns acquisition lifetime. It
        // cannot silently reopen a pile or endpoint after the writer ends.
        assert!(BlobStoreGet::get::<View<str>, UTF8String>(&reader, handle).is_err());
        let mut reopened = open_pile_strict(&pile).unwrap();
        let bytes: View<str> = BlobStoreGet::get(&reopened.snapshot().unwrap(), handle).unwrap();
        assert_eq!(bytes.as_ref(), body);
        reopened.close().unwrap();
    }

    /// A fixture fragment with the identity its builder named.
    #[derive(Clone)]
    struct Staged {
        fragment: Fragment,
        identity: blockdag::Identity,
    }

    impl std::ops::Deref for Staged {
        type Target = Fragment;
        fn deref(&self) -> &Fragment {
            &self.fragment
        }
    }

    impl From<Fragment> for Staged {
        /// A fragment no builder named an identity for: it holds no node.
        fn from(fragment: Fragment) -> Self {
            Self {
                fragment,
                identity: blockdag::Identity::none(),
            }
        }
    }

    impl From<Staged> for Fragment {
        fn from(staged: Staged) -> Self {
            staged.fragment
        }
    }

    impl ArchiveImportWriter {
        /// Stage a fixture with the identity its builder named.
        fn stage_fixture(&mut self, fixture: impl Into<Staged>) -> Result<()> {
            let fixture = fixture.into();
            self.stage(fixture.fragment, fixture.identity)
        }
    }

    fn projection(locator: &str, text: &str) -> Staged {
        let fact = blockdag::text_fact(
            schema::content_fact::modality::TEXT,
            schema::content_fact::direction::IN,
            text,
        )
        .unwrap();
        let part = fact;
        let (block, identity) = blockdag::block_parts([], None, part).unwrap();
        let fragment = blockdag::source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            locator,
            format!("{{\\\"text\\\":{text:?}}}").into_bytes(),
            block,
        )
        .unwrap();
        Staged { fragment, identity }
    }

    fn projection_at(locator: &str, text: &str, unix_seconds: f64) -> Staged {
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
    ) -> Staged {
        let fact =
            blockdag::text_fact(modality, schema::content_fact::direction::IN, text).unwrap();
        let part = fact;
        let epoch = Epoch::from_unix_seconds(unix_seconds);
        let timestamp: Inline<NsTAIInterval> =
            (epoch, epoch).try_to_inline().expect("valid test interval");
        let (block, identity) = blockdag::block_parts([], Some(timestamp), part).unwrap();
        let fragment = blockdag::source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            locator,
            format!("{{\"text\":{text:?}}}").into_bytes(),
            block,
        )
        .unwrap();
        Staged { fragment, identity }
    }

    fn projection_after_at(
        locator: &str,
        text: &str,
        unix_seconds: Option<f64>,
        predecessors: &[Id],
    ) -> (Staged, Id) {
        let fact = blockdag::text_fact(
            schema::content_fact::modality::TEXT,
            schema::content_fact::direction::IN,
            text,
        )
        .unwrap();
        let part = fact;
        let timestamp = unix_seconds.map(|seconds| {
            let epoch = Epoch::from_unix_seconds(seconds);
            (epoch, epoch).try_to_inline().expect("valid test interval")
        });
        let (block, identity) =
            blockdag::block_parts(predecessors.iter().copied(), timestamp, part).unwrap();
        let block_id = block.root().unwrap();
        let fragment = blockdag::source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            locator,
            format!("{{\"text\":{text:?}}}").into_bytes(),
            block,
        )
        .unwrap();
        (Staged { fragment, identity }, block_id)
    }

    fn projection_split_across_source_elements(locator: &str, text: &str) -> (Fragment, Fragment) {
        let fact = blockdag::text_fact(
            schema::content_fact::modality::TEXT,
            schema::content_fact::direction::IN,
            text,
        )
        .unwrap();
        let part = fact;
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

    /// One COMMIT of `fragment`, straight into the Archive root: the
    /// fixtures that use it test the collection's views, not the writer.
    fn commit_fragment(
        pile: &std::path::Path,
        key: &std::path::Path,
        fragment: impl Into<Fragment>,
    ) -> CollectionCommit {
        let fragment: Fragment = fragment.into();
        let signer = load_signer(pile, Some(key)).unwrap();
        let mut store = open_pile_strict_as(pile, signer.verifying_key()).unwrap();
        let source = test_source(&mut store, pile, key);
        let commit = store.commit(source, &signer, fragment).unwrap();
        store.close().unwrap();
        commit
    }

    fn commit_projection(
        pile: &std::path::Path,
        key: &std::path::Path,
        locator: &str,
        text: &str,
    ) -> CollectionCommit {
        commit_fragment(pile, key, projection(locator, text))
    }

    /// The first embedded blob one of the fragment's facts names.
    fn first_embedded_handle(fragment: &Fragment) -> Inline<Handle<UnknownBlob>> {
        let named: BTreeSet<[u8; 32]> = fragment
            .facts()
            .iter()
            .map(|fact| {
                let mut value = [0u8; 32];
                value.copy_from_slice(&fact.data[32..]);
                value
            })
            .collect();
        let mut blobs = fragment.blobs().clone();
        blobs
            .snapshot()
            .unwrap()
            .iter()
            .find(|(handle, _)| named.contains(&handle.raw))
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
        writer.stage_fixture(fragment).unwrap();

        assert!(writer.delta_len() > 0);

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

        let commit = writer.finish(Ok(())).unwrap().1.last().unwrap();
        let after = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(
            after.support().len(),
            3,
            "the vocabulary and the piece's annotation and identity COMMITs"
        );
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
        writer.stage_fixture(fragment).unwrap();
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
    fn writer_publishes_opaque_annotations_idempotently() {
        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile, &key);

        // An opaque entity of no Archive kind: no node and no content fact,
        // so it is all annotation, with repeated values of each attribute.
        let fact = fucid();
        let annotation_kind = fucid();
        let other_kind = fucid();
        let fragment = entity! { &fact @
            metadata::tag*: [&other_kind.id, &annotation_kind.id],
            metadata::name*: ["first annotation", "second annotation"],
            metadata::description*: ["first body", "second body"],
        };
        let annotation = entity! { &fact @
            metadata::name: "later annotation",
        };

        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        // The fixture holds no node: every fact annotates, and nothing cuts.
        writer.stage_fixture(fragment.clone()).unwrap();
        let first = writer.commit_unit().unwrap();
        assert_eq!(first.pieces, 1);
        assert_eq!(first.commits.len(), 2, "the vocabulary and the annotations");
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
            .all(|fact| exists!(prepared_facts.pattern(
                inlineencodings::GenId::inline_from(*fact.e()),
                inlineencodings::GenId::inline_from(*fact.a()),
                *fact.v::<UnknownInline>(),
            ))));
        // The same unit again is the same COMMIT, and adds nothing.
        let length = std::fs::metadata(&pile).unwrap().len();
        writer.stage_fixture(fragment.clone()).unwrap();
        assert_eq!(writer.commit_unit().unwrap().commits, first.commits[1..]);
        assert_eq!(std::fs::metadata(&pile).unwrap().len(), length);
        writer.stage_fixture(annotation.clone()).unwrap();
        assert!(!writer.finish(Ok(())).unwrap().1.is_empty());

        let snapshot = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(snapshot.support().len(), 3, "the vocabulary and two units");
        let facts = snapshot.facts().unwrap();
        let tags: BTreeSet<_> = find!(
            tag: Id,
            pattern!(&facts, [{ fact.id @ metadata::tag: ?tag }])
        )
        .collect();
        assert_eq!(tags, BTreeSet::from([other_kind.id, annotation_kind.id]));
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
            pattern!(&facts, [{ fact.id @ metadata::description: ?payload }])
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

        // Replaying both units republishes the same COMMITs: no bytes.
        let length = std::fs::metadata(&pile).unwrap().len();
        let mut retry = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        retry.stage_fixture(fragment).unwrap();
        assert_eq!(retry.commit_unit().unwrap(), first);
        retry.stage_fixture(annotation).unwrap();
        assert!(!retry.finish(Ok(())).unwrap().1.is_empty());
        assert_eq!(std::fs::metadata(&pile).unwrap().len(), length);
    }

    /// A writer publishes COMMITs and nothing else: no MERGE and no MAP,
    /// however many units it imports. The next read carries the source and
    /// attaches its views, and reads every unit.
    #[test]
    fn a_writer_leaves_carrying_to_the_reader() {
        use triblespace::core::collection::MERGE_FAN_IN;

        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile, &key);

        let units = MERGE_FAN_IN * MERGE_FAN_IN + 3;
        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        let source = writer.collection;
        for index in 0..units {
            writer
                .stage_fixture(projection(
                    &format!("session:{index}"),
                    &format!("body {index}"),
                ))
                .unwrap();
            assert!(!writer.commit_unit().unwrap().is_empty());
        }
        writer.close(Ok(())).unwrap();

        let mut reopened = open_pile_strict_as(&pile, signer.verifying_key()).unwrap();
        let snapshot = reopened.snapshot().unwrap();
        let records = discovered_records(&snapshot).unwrap();
        assert_eq!(
            records
                .commits()
                .iter()
                .filter(|commit| commit.collection() == source.handle())
                .count(),
            2 * units + 1,
            "two COMMITs per unit and the vocabulary's"
        );
        assert!(records.merges().is_empty(), "the writer merged");
        assert!(records.maps().is_empty(), "the writer attached");
        drop(snapshot);
        reopened.close().unwrap();

        let observed = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(observed.support().len(), 2 * units + 1);
        assert!(observed.residual().is_empty());
        assert_eq!(projection_ids(&observed.facts().unwrap()).len(), units);
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
        writer.stage_fixture(fragment.clone()).unwrap();
        let (_, published) = writer.finish(Ok(())).unwrap();
        let first = published.last().unwrap();
        let mut retry = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        let length = std::fs::metadata(&pile).unwrap().len();
        retry.stage_fixture(fragment).unwrap();
        let (_, repeated) = retry.finish(Ok(())).unwrap();
        assert_eq!(repeated, published, "the same unit is the same COMMITs");
        assert_eq!(std::fs::metadata(&pile).unwrap().len(), length);

        let snapshot = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(
            snapshot.support().len(),
            3,
            "the vocabulary and the unit's two"
        );
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
        let admitted = pile
            .commit(collection, &signer, fragment.fragment.clone())
            .unwrap();
        let foreign = SigningKey::from_bytes(&[0xA7; 32]);
        let duplicate = pile
            .commit(collection, &foreign, fragment.fragment)
            .unwrap();
        assert_eq!(duplicate.data(), admitted.data());
        pile.close().unwrap();

        let snapshot = pollster::block_on(ensure_local(&pile_path, Some(&key_path))).unwrap();
        assert_eq!(snapshot.support().len(), 1);
        assert!(facts_stand_on(&snapshot, &admitted));
        assert_eq!(projection_ids(&snapshot.facts().unwrap()).len(), 1);
    }

    #[test]
    fn a_second_unit_keeps_its_whole_closure_including_shared_facts() {
        let directory = TempDir::new().unwrap();
        let pile = directory.path().join("archive.pile");
        std::fs::File::create(&pile).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile, &key);

        let first_fragment = projection("session:one", "shared");
        let first_len = first_fragment.facts().len();
        let mut first_writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        first_writer.stage_fixture(first_fragment.clone()).unwrap();
        let first = first_writer.finish(Ok(())).unwrap().1;

        let second_fragment = projection("session:two", "shared");
        let mut second_writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        second_writer
            .stage_fixture(second_fragment.clone())
            .unwrap();
        assert_eq!(
            second_writer.delta_len(),
            second_fragment.facts().len(),
            "a unit retains its whole closure, facts already published included"
        );
        let second = second_writer.finish(Ok(())).unwrap().1;
        // The two receipts annotate one node: the identity COMMIT, inserted
        // last, is the same record, and the annotation COMMITs differ.
        assert_eq!(first.last(), second.last());
        assert_ne!(first.commits[1], second.commits[0]);

        let snapshot = pollster::block_on(ensure_local(&pile, Some(&key))).unwrap();
        assert_eq!(
            snapshot.support().len(),
            4,
            "the vocabulary, one identity and two annotation COMMITs"
        );
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

        let mut writer = ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        writer
            .stage_fixture(projection_at_modality(
                "session:text",
                schema::content_fact::modality::TEXT,
                "spoken",
                1.0,
            ))
            .unwrap();
        writer
            .stage_fixture(projection_at_modality(
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
                    { item.block @ schema::block::holds: _?fact },
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

        let mut writer = ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        // Deliberately stage out of causal and temporal order. The collection
        // is a set; replay order must come solely from canonical semantics.
        writer.stage_fixture(regressed_child).unwrap();
        writer.stage_fixture(independent).unwrap();
        writer.stage_fixture(untimed).unwrap();
        writer.stage_fixture(parent).unwrap();
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

        commit_projection(&pile_path, &key, "session:index", "exact succinct");

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
            commit_projection(&pile_path, &key, locator, text);
        }

        let report = ensure_bm25_index(&pile_path, Some(&key)).unwrap();

        assert_eq!(report.source_elements, 2);
        assert_eq!(report.lagging, 0);
        assert_eq!(report.cover_segments, 2);
        let length = std::fs::metadata(&pile_path).unwrap().len();
        assert_eq!(ensure_bm25_index(&pile_path, Some(&key)).unwrap(), report);
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
            commit_fragment(
                &pile_path,
                &key,
                projection_at(locator, "shared closure needle", seconds),
            );
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
        let mut writer = ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        writer.stage_fixture(first_fragment).unwrap();
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
        let mut writer = ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        writer.stage_fixture(second_fragment.clone()).unwrap();
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
        let mut retry = ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
        retry.stage_fixture(second_fragment).unwrap();
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
    /// part/fact closure live in separate signed elements. The BM25
    /// documents are texts, so each commit maps on its own: the block's
    /// commit names no text and the remainder names the block's. Maintenance
    /// attaches every node with nothing residual, a repeat publishes nothing,
    /// and the text is found at once.
    #[test]
    fn bm25_indexes_a_split_block_without_waiting_for_a_merged_node() {
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
        // A whole commit after the split one.
        commit_projection(&pile_path, &key, "session:whole", "whole haystack");

        let report = ensure_bm25_index(&pile_path, Some(&key)).unwrap();
        assert_eq!(report.source_elements, 3);
        assert_eq!(report.lagging, 0, "a split block's node maps on its own");
        let length = std::fs::metadata(&pile_path).unwrap().len();
        assert_eq!(ensure_bm25_index(&pile_path, Some(&key)).unwrap(), report);
        assert_eq!(
            std::fs::metadata(&pile_path).unwrap().len(),
            length,
            "a repeat pass publishes nothing"
        );

        let mut pile = open_pile_strict_as(&pile_path, signer.verifying_key()).unwrap();
        let records = discovered_records(&pile.snapshot().unwrap()).unwrap();
        let attached_nodes: BTreeSet<_> = records
            .maps()
            .iter()
            .filter(|map| map.attached() == target.handle())
            .map(|map| map.node())
            .collect();
        assert!(attached_nodes.contains(&block_commit.data()));
        assert!(attached_nodes.contains(&remainder_commit.data()));
        assert_eq!(attached_nodes.len(), 3);
        pile.close().unwrap();

        let (_, index, lag) =
            pollster::block_on(ensure_search_local(&pile_path, Some(&key))).unwrap();
        assert!(lag.is_current(), "{lag:?}");
        let query = index.query().unwrap();
        assert_eq!(query.query_multi(&hash_tokens("whole haystack")).len(), 1);
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
                pile.maintain_attached_with::<archive_bm25::ArchiveTextBm25Mapping>(
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
                pile.maintain_attached_with::<archive_bm25::ArchiveTextBm25Mapping>(
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
                pile.maintain_attached_with::<archive_bm25::ArchiveTextBm25Mapping>(
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
            pile.maintain_attached_with::<archive_bm25::ArchiveTextBm25Mapping>(target, &signer),
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
    fn exact_fact_cover_needs_no_outer_commits() {
        use triblespace::core::collection::CollectionAttachment;

        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        initialize_archive_fixture(&pile_path, &key);

        let block = fucid();
        let timestamp = crate::clock::point(hifitime::Epoch::from_tai_duration(
            hifitime::Duration::from_total_nanoseconds(1),
        ))
        .unwrap();
        let fragment = entity! { &block @
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

    /// Faculties' own importer commits each node together with its held
    /// facts. (The Archive BM25 mapping no longer depends on this: its
    /// documents are texts, whole wherever they are named.) This pins that
    /// property for data we write; it says nothing of
    /// another writer's commits or of historical input, and the writer itself
    /// checks nothing. Every commit that tags a node holds the node's `holds`
    /// facts and each held fact's tag, modality, direction and payload, and
    /// every content fact a commit tags is held by a node that commit tags.
    /// The second transcript repeats the first's opening message and its
    /// first reply nodes at other times, so their receipts gain new facts
    /// while the nodes and their facts are already in the pile: a writer that
    /// committed only the facts the pile lacked would leave those receipts
    /// pointing at nodes their commit does not hold, and this test would
    /// fail.
    #[test]
    fn every_import_commit_holds_whole_nodes() {
        const FIRST: &str = r#"{"type":"user","sessionId":"whole-1","uuid":"u1","parentUuid":null,"timestamp":"2026-03-01T15:34:01.542Z","message":{"role":"user","content":"hello there"}}
{"type":"assistant","sessionId":"whole-1","uuid":"a1","parentUuid":"u1","timestamp":"2026-03-01T15:34:02.000Z","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"thinking","thinking":"consider it","signature":"opaque"},{"type":"text","text":"hi!"},{"type":"tool_use","id":"toolu_1","name":"Screenshot","input":{"display":1}}]}}
{"type":"user","sessionId":"whole-1","uuid":"u2","parentUuid":"a1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"screenshot below"},{"type":"image","source":{"type":"base64","media_type":"IMAGE/PNG; charset=binary","data":"iVBORw=="}}]}]}}"#;
        const SECOND: &str = r#"{"type":"user","sessionId":"whole-2","uuid":"v1","parentUuid":null,"timestamp":"2026-03-02T09:00:00.000Z","message":{"role":"user","content":"hello there"}}
{"type":"assistant","sessionId":"whole-2","uuid":"b1","parentUuid":"v1","timestamp":"2026-03-02T09:00:01.000Z","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"thinking","thinking":"consider it","signature":"opaque"},{"type":"text","text":"hi!"},{"type":"text","text":"and something more"}]}}"#;

        let directory = TempDir::new().unwrap();
        let pile_path = directory.path().join("archive.pile");
        std::fs::File::create(&pile_path).unwrap();
        let key = directory.path().join("archive.key");
        let signer = initialize_archive_fixture(&pile_path, &key);
        for (name, transcript) in [("first.jsonl", FIRST), ("second.jsonl", SECOND)] {
            let mut writer = ArchiveImportWriter::open(&pile_path, Some(&key)).unwrap();
            let projection = crate::archive_claude_code::project_bytes(
                name,
                Bytes::from_source(transcript.as_bytes().to_vec()),
                |projected| writer.stage(projected.fragment, projected.identity),
            );
            let (_, commit) = writer.finish(projection).unwrap();
            assert!(!commit.is_empty(), "{name} commits");
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
            // The vocabulary and annotation COMMITs hold no node.
            .filter(|facts: &TribleSet| {
                exists!(pattern!(facts, [{ _?node @ metadata::tag: &schema::block::KIND }]))
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
        let held_by = |node: Id| -> BTreeSet<Id> {
            find!(
                fact: Id,
                pattern!(&union, [{ node @ schema::block::holds: ?fact }])
            )
            .collect()
        };
        // One node as the whole collection states it: the node's tag and
        // `holds` facts, and each held fact's tag, modality, direction and
        // payload.
        let read = [
            metadata::tag.id(),
            schema::block::holds.id(),
            schema::content_fact::modality.id(),
            schema::content_fact::direction.id(),
            schema::content_fact::payload.id(),
            schema::content_fact::blob.id(),
            schema::content_fact::asset_pointer.id(),
        ];
        let unit = |node: Id| -> TribleSet {
            let facts = held_by(node);
            let mut unit = TribleSet::new();
            for trible in union.iter() {
                let entity = trible.e();
                if (*entity == node || facts.contains(entity)) && read.contains(trible.a()) {
                    unit.insert(trible);
                }
            }
            unit
        };

        let mut holding_nodes = 0;
        for (index, facts) in commits.iter().enumerate() {
            for node in tagged(facts, schema::block::KIND) {
                let missing = unit(node)
                    .iter()
                    .filter(|trible| !facts.contains(trible))
                    .count();
                assert_eq!(
                    missing, 0,
                    "commit {index} holds node {node:X} without all of its facts"
                );
                if !held_by(node).is_empty() {
                    holding_nodes += 1;
                }
            }
            for fact in tagged(facts, schema::content_fact::KIND) {
                let nodes: BTreeSet<Id> = find!(
                    node: Id,
                    pattern!(facts, [{
                        ?node @
                            metadata::tag: &schema::block::KIND,
                            schema::block::holds: &fact,
                    }])
                )
                .collect();
                assert!(
                    !nodes.is_empty(),
                    "commit {index} holds fact {fact:X} without a node it tags"
                );
            }
        }
        assert!(holding_nodes >= 6, "{holding_nodes}");
        // The case a splitting writer gets wrong is present: the two commits
        // share nodes.
        assert!(!tagged(&commits[0], schema::block::KIND)
            .is_disjoint(&tagged(&commits[1], schema::block::KIND)));
    }

    /// A Claude Code transcript of `turns` question/answer pairs on one chain.
    fn claude_transcript(session: &str, turns: usize) -> String {
        let mut out = String::new();
        let mut parent: Option<String> = None;
        for turn in 0..turns {
            for kind in ["user", "assistant"] {
                let uuid = format!("{session}-{kind}-{turn}");
                let parent_json = parent
                    .as_ref()
                    .map_or("null".to_owned(), |parent| format!("\"{parent}\""));
                let second = turn * 2 + usize::from(kind == "assistant");
                let at = format!(
                    "2026-03-01T{:02}:{:02}:{:02}.000Z",
                    10 + second / 3600,
                    (second / 60) % 60,
                    second % 60
                );
                let line = if kind == "user" {
                    format!(
                        r#"{{"type":"user","sessionId":"{session}","uuid":"{uuid}","parentUuid":{parent_json},"timestamp":"{at}","message":{{"role":"user","content":"question {turn} of {session}"}}}}"#
                    )
                } else {
                    format!(
                        r#"{{"type":"assistant","sessionId":"{session}","uuid":"{uuid}","parentUuid":{parent_json},"timestamp":"{at}","message":{{"role":"assistant","model":"claude-opus-4","content":[{{"type":"text","text":"answer {turn} of {session}"}}]}}}}"#
                    )
                };
                out.push_str(&line);
                out.push('\n');
                parent = Some(uuid);
            }
        }
        out
    }

    /// A Codex rollout of `turns` turns, each a prompt, a reasoning summary
    /// and an answer, stamped from `start_second` on.
    fn codex_rollout_lines(session: &str, first_turn: usize, turns: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for turn in first_turn..first_turn + turns {
            let at = |offset: usize| {
                let second = turn * 3 + offset;
                format!(
                    "2026-08-01T{:02}:{:02}:{:02}.000Z",
                    10 + second / 3600,
                    (second / 60) % 60,
                    second % 60
                )
            };
            lines.push(format!(
                r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"user_message","message":"question {turn} of {session}","images":[],"local_images":[]}}}}"#,
                at(0)
            ));
            lines.push(format!(
                r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"agent_reasoning","text":"weighing question {turn} of {session}"}}}}"#,
                at(1)
            ));
            lines.push(format!(
                r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"agent_message","message":"answer {turn} of {session}"}}}}"#,
                at(2)
            ));
        }
        lines
    }

    fn codex_parent(turns: usize) -> String {
        let mut text = String::from(concat!(
            r#"{"timestamp":"2026-08-01T09:59:59.000Z","type":"session_meta","payload":{"id":"piece-parent","session_id":"piece-parent"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-01T09:59:59.500Z","type":"turn_context","payload":{"model":"gpt-test"}}"#,
            "\n",
        ));
        for line in codex_rollout_lines("piece-parent", 0, turns) {
            text.push_str(&line);
            text.push('\n');
        }
        text
    }

    /// A fork as Codex writes one: its own session record, every parent
    /// record again under the copy's timestamp, then turns of its own.
    fn codex_fork(parent: &str, own_turns: usize, parent_turns: usize) -> String {
        let mut fork = String::from(concat!(
            r#"{"timestamp":"2026-08-03T09:00:00.000Z","type":"session_meta","payload":{"id":"piece-fork","session_id":"piece-parent","forked_from_id":"piece-parent"}}"#,
            "\n"
        ));
        for line in parent.lines() {
            let (_, rest) = line.split_once(r#"","type""#).unwrap();
            fork.push_str(r#"{"timestamp":"2026-08-03T09:00:00.500Z","type""#);
            fork.push_str(rest);
            fork.push('\n');
        }
        fork.push_str(
            r#"{"timestamp":"2026-08-03T09:00:05.000Z","type":"turn_context","payload":{"model":"gpt-test"}}"#,
        );
        fork.push('\n');
        for line in codex_rollout_lines("piece-fork", parent_turns + 2000, own_turns) {
            // Own turns start after the fork's creation.
            let (_, rest) = line.split_once(r#"","type""#).unwrap();
            fork.push_str(r#"{"timestamp":"2026-08-03T10:00:00.000Z","type""#);
            fork.push_str(rest);
            fork.push('\n');
        }
        fork
    }

    fn import_source(
        pile: &std::path::Path,
        key: &std::path::Path,
        name: &str,
        text: &str,
    ) -> UnitPublication {
        let mut writer = ArchiveImportWriter::open(pile, Some(key)).unwrap();
        // One piece per sixteen nodes on average, so small fixtures cut.
        writer.set_piece_bits(4);
        let projection = stage_source(&mut writer, name, text);
        writer.finish(projection).unwrap().1
    }

    fn stage_source(writer: &mut ArchiveImportWriter, name: &str, text: &str) -> Result<()> {
        let bytes = Bytes::from_source(text.as_bytes().to_vec());
        if name.starts_with("rollout") {
            crate::archive_codex::project_bytes(name, bytes, |projected| {
                writer.stage(projected.fragment, projected.identity)
            })
            .map(drop)
        } else {
            crate::archive_claude_code::project_bytes(name, bytes, |projected| {
                writer.stage(projected.fragment, projected.identity)
            })
            .map(drop)
        }
    }

    fn pile_length(pile: &std::path::Path) -> u64 {
        std::fs::metadata(pile).unwrap().len()
    }

    /// A COMMIT as two piles can compare it: its data and metadata handles.
    /// The signature differs between keys and is left out.
    type Record = (CollectionData, Inline<Handle<SimpleArchive>>);

    /// Every Archive COMMIT in the pile.
    fn archive_commit_records(pile: &std::path::Path, key: &std::path::Path) -> BTreeSet<Record> {
        let signer = load_signer(pile, Some(key)).unwrap();
        let mut store = open_pile_strict_as(pile, signer.verifying_key()).unwrap();
        let source = test_source(&mut store, pile, key);
        let snapshot = store.snapshot().unwrap();
        let commits = discovered_records(&snapshot)
            .unwrap()
            .commits()
            .iter()
            .filter(|commit| commit.collection() == source.handle())
            .map(|commit| (commit.data(), commit.metadata()))
            .collect();
        drop(snapshot);
        store.close().unwrap();
        commits
    }

    /// The node ids every given COMMIT's data holds.
    fn blocks_in_commits(pile: &std::path::Path, commits: &BTreeSet<Record>) -> BTreeSet<Id> {
        let mut store = open_pile_strict(pile).unwrap();
        let snapshot = store.snapshot().unwrap();
        let mut blocks = BTreeSet::new();
        for (data, _) in commits {
            let facts: TribleSet = snapshot
                .get::<TribleSet, SimpleArchive>(Handle::<SimpleArchive>::from_hash(*data))
                .unwrap();
            blocks.extend(find!(
                block: Id,
                pattern!(&facts, [{ ?block @ metadata::tag: &schema::block::KIND }])
            ));
        }
        drop(snapshot);
        store.close().unwrap();
        blocks
    }

    /// Records of `after` that `before` lacks.
    fn novel(after: BTreeSet<Record>, before: &BTreeSet<Record>) -> BTreeSet<Record> {
        after.difference(before).copied().collect()
    }

    fn fresh_archive(directory: &TempDir, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let pile = directory.path().join(format!("{name}.pile"));
        let key = directory.path().join(format!("{name}.key"));
        std::fs::File::create(&pile).unwrap();
        initialize_archive_fixture(&pile, &key);
        (pile, key)
    }

    /// The nodes the read Archive holds, and those of them with a timestamp,
    /// and the receipts, and those of them with a source timestamp.
    fn timed_union(
        pile: &std::path::Path,
        key: &std::path::Path,
    ) -> (BTreeSet<Id>, BTreeSet<Id>, BTreeSet<Id>, BTreeSet<Id>) {
        let observed = pollster::block_on(ensure_local(pile, Some(key))).unwrap();
        let facts = observed.facts().unwrap();
        let blocks = find!(
            block: Id,
            pattern!(&facts, [{ ?block @ metadata::tag: &schema::block::KIND }])
        )
        .collect();
        let timed = find!(
            (block: Id, at: Inline<NsTAIInterval>),
            pattern!(&facts, [{ ?block @ schema::block::timestamp: ?at }])
        )
        .map(|(block, _)| block)
        .collect();
        let receipts = projection_ids(&facts).into_iter().collect();
        let timed_receipts = find!(
            (receipt: Id, at: Inline<NsTAIInterval>),
            pattern!(&facts, [{ ?receipt @ schema::source_projection::source_timestamp: ?at }])
        )
        .map(|(receipt, _)| receipt)
        .collect();
        (blocks, timed, receipts, timed_receipts)
    }

    #[test]
    fn reimporting_an_unchanged_session_adds_no_bytes() {
        let directory = TempDir::new().unwrap();
        let (pile, key) = fresh_archive(&directory, "archive");
        let transcript = claude_transcript("unchanged", 300);
        let first = import_source(&pile, &key, "unchanged.jsonl", &transcript);
        assert!(first.pieces > 4, "{}", first.pieces);
        let length = pile_length(&pile);
        let commits = archive_commit_records(&pile, &key);
        let again = import_source(&pile, &key, "unchanged.jsonl", &transcript);
        assert_eq!(again, first, "the same COMMITs, inserted again");
        assert_eq!(pile_length(&pile), length, "the second import added bytes");
        assert_eq!(archive_commit_records(&pile, &key), commits);
    }

    #[test]
    fn reimporting_an_unchanged_rollout_adds_no_bytes() {
        let directory = TempDir::new().unwrap();
        let (pile, key) = fresh_archive(&directory, "archive");
        let rollout = codex_parent(300);
        import_source(&pile, &key, "rollout-unchanged.jsonl", &rollout);
        let length = pile_length(&pile);
        import_source(&pile, &key, "rollout-unchanged.jsonl", &rollout);
        assert_eq!(pile_length(&pile), length, "the second import added bytes");
    }

    /// What a rebuilt cohort changes: the attribute descriptions fragments
    /// carry (doc comments, module paths). A piece's COMMITs stay the same
    /// records, so an unchanged source re-imported by another build inserts
    /// no new record. Piece COMMITs carry empty metadata, and the vocabulary
    /// COMMIT's metadata holds no doc text and no module path.
    #[test]
    fn piece_commits_do_not_depend_on_the_build() {
        let directory = TempDir::new().unwrap();
        let transcript = claude_transcript("rebuilt", 120);
        let (plain, key_a) = fresh_archive(&directory, "plain");
        import_source(&plain, &key_a, "rebuilt.jsonl", &transcript);
        let (rebuilt, key_b) = fresh_archive(&directory, "rebuilt");
        let other_build = entity! { _ @
            metadata::description: "a doc comment another build wrote",
            metadata::source_module: "another::module::path",
        };
        {
            let mut writer = ArchiveImportWriter::open(&rebuilt, Some(&key_b)).unwrap();
            writer.set_piece_bits(4);
            crate::archive_claude_code::project_bytes(
                "rebuilt.jsonl",
                Bytes::from_source(transcript.as_bytes().to_vec()),
                |projected| {
                    let (_, facts, mut metafacts, mut blobs) = projected.fragment.into_parts();
                    metafacts += other_build.facts().clone();
                    blobs.union(other_build.blobs().clone());
                    writer.stage(
                        Fragment::from_parts(facts, metafacts, blobs),
                        projected.identity,
                    )
                },
            )
            .unwrap();
            writer.close(Ok(())).unwrap();
        }
        let records = archive_commit_records(&plain, &key_a);
        assert_eq!(records, archive_commit_records(&rebuilt, &key_b));
        // Nor is the other build's doc text appended as an orphan blob.
        let doc = find!(
            text: Inline<Handle<UTF8String>>,
            pattern!(other_build.facts(), [{ _?usage @ metadata::description: ?text }])
        )
        .next()
        .unwrap();
        let mut store = open_pile_strict(&rebuilt).unwrap();
        assert!(store.snapshot().unwrap().metadata(doc).unwrap().is_none());
        store.close().unwrap();

        let mut store = open_pile_strict(&plain).unwrap();
        let snapshot = store.snapshot().unwrap();
        let empty: Blob<SimpleArchive> = TribleSet::new().to_blob();
        let mut described = 0;
        for (data, metadata) in &records {
            let facts: TribleSet = snapshot
                .get::<TribleSet, SimpleArchive>(Handle::<SimpleArchive>::from_hash(*data))
                .unwrap();
            let meta: TribleSet = snapshot.get(*metadata).unwrap();
            let vocabulary = exists!(pattern!(&facts, [{
                _?tag @ metadata::tag: &metadata::KIND_TAG
            }]));
            if vocabulary {
                described += 1;
                assert!(!meta.is_empty(), "the vocabulary describes the attributes");
                assert!(meta
                    .iter()
                    .all(|fact| *fact.a() != metadata::description.id()
                        && *fact.a() != metadata::source_module.id()));
            } else {
                assert_eq!(*metadata, empty.get_handle(), "a piece carries metadata");
            }
        }
        assert_eq!(described, 1);
        drop(snapshot);
        store.close().unwrap();
    }

    #[test]
    fn a_grown_session_republishes_only_its_tail() {
        let directory = TempDir::new().unwrap();
        let (pile, key) = fresh_archive(&directory, "archive");
        let empty = pile_length(&pile);
        import_source(&pile, &key, "grown.jsonl", &claude_transcript("grown", 400));
        let first = pile_length(&pile) - empty;
        let before = archive_commit_records(&pile, &key);
        import_source(&pile, &key, "grown.jsonl", &claude_transcript("grown", 410));
        let added = pile_length(&pile) - empty - first;
        let republished =
            blocks_in_commits(&pile, &novel(archive_commit_records(&pile, &key), &before));
        // 20 new nodes; the rest is the tail piece the growth reopened.
        assert!(
            republished.len() < 20 + 200,
            "{} nodes republished for 20 new ones",
            republished.len()
        );
        assert!(added * 3 < first, "{added} bytes added after {first}");
    }

    /// A content-defined cut resynchronises after an insertion: a side branch
    /// written into the middle of the file costs the piece it lands in. A
    /// rule that cut by position would shift every later piece.
    #[test]
    fn an_interleaved_branch_costs_one_piece() {
        let directory = TempDir::new().unwrap();
        let (pile, key) = fresh_archive(&directory, "archive");
        let base = claude_transcript("branchy", 400);
        import_source(&pile, &key, "branchy.jsonl", &base);
        let before = archive_commit_records(&pile, &key);
        let base_blocks = blocks_in_commits(&pile, &before);
        let mut lines: Vec<&str> = base.lines().collect();
        let side_user = r#"{"type":"user","sessionId":"branchy","uuid":"branchy-side-user","parentUuid":"branchy-assistant-5","timestamp":"2026-03-01T10:00:11.500Z","message":{"role":"user","content":"a side question"}}"#;
        let side_answer = r#"{"type":"assistant","sessionId":"branchy","uuid":"branchy-side-assistant","parentUuid":"branchy-side-user","timestamp":"2026-03-01T10:00:11.700Z","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"text","text":"a side answer"}]}}"#;
        lines.insert(12, side_answer);
        lines.insert(12, side_user);
        let branched = lines.join("\n") + "\n";
        import_source(&pile, &key, "branchy.jsonl", &branched);
        let after = archive_commit_records(&pile, &key);
        let all = blocks_in_commits(&pile, &after);
        let again: BTreeSet<_> = blocks_in_commits(&pile, &novel(after, &before))
            .intersection(&base_blocks)
            .copied()
            .collect();
        assert_eq!(
            all.len(),
            base_blocks.len() + 2,
            "the branch adds its two nodes"
        );
        assert!(
            again.len() < 100,
            "the branch republished {} of {} nodes",
            again.len(),
            base_blocks.len()
        );
    }

    #[test]
    fn a_codex_fork_converges_with_its_parent_pieces() {
        let directory = TempDir::new().unwrap();
        let (pile, key) = fresh_archive(&directory, "archive");
        let parent = codex_parent(300);
        let fork = codex_fork(&parent, 5, 300);
        import_source(&pile, &key, "rollout-parent.jsonl", &parent);
        let before = archive_commit_records(&pile, &key);
        let parent_blocks = blocks_in_commits(&pile, &before);
        assert!(parent_blocks.len() >= 900, "{}", parent_blocks.len());
        import_source(&pile, &key, "rollout-fork.jsonl", &fork);
        let again: BTreeSet<_> =
            blocks_in_commits(&pile, &novel(archive_commit_records(&pile, &key), &before))
                .intersection(&parent_blocks)
                .copied()
                .collect();
        assert!(
            again.len() < 200,
            "the fork republished {} of the parent's {} nodes",
            again.len(),
            parent_blocks.len()
        );
    }

    /// A content fact that recurs in two pieces of one unit is in both
    /// pieces' identity COMMITs: what a piece publishes does not depend on
    /// what an earlier piece of its unit already staged.
    #[test]
    fn a_content_fact_recurring_in_two_pieces_is_in_both_identity_commits() {
        let directory = TempDir::new().unwrap();
        let (pile, key) = fresh_archive(&directory, "archive");
        let transcript = claude_transcript("recur", 12)
            .replace(
                r#""text":"answer 2 of recur""#,
                r#""text":"the same answer""#,
            )
            .replace(
                r#""text":"answer 9 of recur""#,
                r#""text":"the same answer""#,
            );
        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        // One piece per node: the two answers land in different pieces.
        writer.set_piece_bits(0);
        stage_source(&mut writer, "recur.jsonl", &transcript).unwrap();
        writer.close(Ok(())).unwrap();

        let mut store = open_pile_strict(&pile).unwrap();
        let snapshot = store.snapshot().unwrap();
        let mut holders = 0usize;
        for (data, _) in archive_commit_records(&pile, &key) {
            let facts: TribleSet = snapshot
                .get::<TribleSet, SimpleArchive>(Handle::<SimpleArchive>::from_hash(data))
                .unwrap();
            for (node, fact) in find!(
                (node: Id, fact: Id),
                pattern!(&facts, [{ ?node @ schema::block::holds: ?fact }])
            ) {
                holders += 1;
                assert!(
                    exists!(pattern!(&facts, [{
                        fact @
                            metadata::tag: &schema::content_fact::KIND,
                            schema::content_fact::modality: _?modality,
                            schema::content_fact::payload: _?payload,
                    }])),
                    "node {node:X} holds {fact:X} in a COMMIT without the fact"
                );
            }
        }
        drop(snapshot);
        store.close().unwrap();
        assert_eq!(holders, 24, "every node of the unit");
    }

    /// What a source publishes does not depend on what the Archive already
    /// holds, nor on what the same writer published before it.
    #[test]
    fn pieces_do_not_depend_on_the_archive_or_the_writer() {
        let directory = TempDir::new().unwrap();
        let parent = codex_parent(300);
        let fork = codex_fork(&parent, 5, 300);
        let (fresh, key_a) = fresh_archive(&directory, "fresh");
        let alone = import_source(&fresh, &key_a, "rollout-fork.jsonl", &fork);
        let alone_records = archive_commit_records(&fresh, &key_a);

        let (held, key_b) = fresh_archive(&directory, "held");
        import_source(&held, &key_b, "rollout-parent.jsonl", &parent);
        let beside = import_source(&held, &key_b, "rollout-fork.jsonl", &fork);
        assert_eq!(beside.pieces, alone.pieces);
        assert!(alone_records.is_subset(&archive_commit_records(&held, &key_b)));

        let (together, key_c) = fresh_archive(&directory, "together");
        {
            let mut writer = ArchiveImportWriter::open(&together, Some(&key_c)).unwrap();
            writer.set_piece_bits(4);
            stage_source(&mut writer, "rollout-parent.jsonl", &parent).unwrap();
            writer.commit_unit().unwrap();
            stage_source(&mut writer, "rollout-fork.jsonl", &fork).unwrap();
            let in_one_writer = writer.commit_unit().unwrap();
            assert_eq!(in_one_writer.pieces, alone.pieces);
            assert_eq!(in_one_writer.commits.len() + 1, alone.commits.len());
            writer.close(Ok(())).unwrap();
        }
        assert_eq!(
            archive_commit_records(&held, &key_b),
            archive_commit_records(&together, &key_c)
        );
    }

    /// A unit whose source fails after pieces have closed inserts no record,
    /// though the closed pieces' bytes are in the pile.
    #[test]
    fn a_failed_unit_with_closed_pieces_publishes_no_record() {
        let directory = TempDir::new().unwrap();
        let (pile, key) = fresh_archive(&directory, "archive");
        let empty = pile_length(&pile);
        let transcript = claude_transcript("failing", 300);
        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        writer.set_piece_bits(4);
        let mut staged = 0usize;
        let result = crate::archive_claude_code::project_bytes(
            "failing.jsonl",
            Bytes::from_source(transcript.as_bytes().to_vec()),
            |projected| {
                staged += 1;
                if staged == 400 {
                    bail!("the source fails here");
                }
                writer.stage(projected.fragment, projected.identity)
            },
        )
        .map(drop);
        assert!(writer.pending.len() > 4, "pieces closed before the failure");
        assert!(writer.close(result).is_err());
        assert!(
            pile_length(&pile) > empty,
            "closed pieces appended their bytes"
        );
        assert!(
            archive_commit_records(&pile, &key).is_empty(),
            "a failed unit made COMMITs visible"
        );
    }

    /// A process killed after any number of a unit's record inserts leaves
    /// no node visible without its annotations, and rerunning the import
    /// completes the unit to exactly the records of an uninterrupted import.
    #[test]
    fn a_partial_insert_shows_no_bare_node_and_a_retry_completes_it() {
        let directory = TempDir::new().unwrap();
        let transcript = claude_transcript("partial", 40);
        let (clean, clean_key) = fresh_archive(&directory, "clean");
        let complete = import_source(&clean, &clean_key, "partial.jsonl", &transcript);
        let expected = archive_commit_records(&clean, &clean_key);
        assert!(complete.commits.len() > 4, "{}", complete.commits.len());
        for inserted in 0..complete.commits.len() {
            let (pile, key) = fresh_archive(&directory, &format!("partial-{inserted}"));
            let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
            writer.set_piece_bits(4);
            stage_source(&mut writer, "partial.jsonl", &transcript).unwrap();
            // What `commit_unit` does, stopped after `inserted` records.
            writer.publish_piece().unwrap();
            writer.pending.truncate(inserted);
            writer.commit_unit().unwrap();
            writer.close(Ok(())).unwrap();
            assert_eq!(archive_commit_records(&pile, &key).len(), inserted);
            let (blocks, timed, _, _) = timed_union(&pile, &key);
            assert!(
                blocks.is_subset(&timed),
                "{inserted}: a node without its time"
            );

            import_source(&pile, &key, "partial.jsonl", &transcript);
            assert_eq!(archive_commit_records(&pile, &key), expected, "{inserted}");
        }
    }

    #[test]
    fn import_needs_no_other_archive_body() {
        let directory = TempDir::new().unwrap();
        let (full, key) = fresh_archive(&directory, "full");
        import_source(
            &full,
            &key,
            "elsewhere.jsonl",
            &claude_transcript("elsewhere", 20),
        );
        let commits: Vec<CollectionCommit> = {
            let signer = load_signer(&full, Some(&key)).unwrap();
            let mut store = open_pile_strict_as(&full, signer.verifying_key()).unwrap();
            let source = test_source(&mut store, &full, &key);
            let snapshot = store.snapshot().unwrap();
            let commits = discovered_records(&snapshot)
                .unwrap()
                .commits()
                .iter()
                .filter(|commit| commit.collection() == source.handle())
                .copied()
                .collect();
            drop(snapshot);
            store.close().unwrap();
            commits
        };
        assert!(!commits.is_empty());

        // The same signer's pile holding those COMMIT records and none of
        // their bodies: what a host sees before sync hands the bytes over.
        let thin = directory.path().join("thin.pile");
        std::fs::File::create(&thin).unwrap();
        {
            let signer = load_signer(&thin, Some(&key)).unwrap();
            let mut store = open_pile_strict_as(&thin, signer.verifying_key()).unwrap();
            test_source(&mut store, &thin, &key);
            for commit in &commits {
                CollectionStore::insert(&mut store, CollectionRecord::Commit(*commit)).unwrap();
            }
            store.close().unwrap();
        }
        let published = import_source(&thin, &key, "here.jsonl", &claude_transcript("here", 20));
        assert!(!published.is_empty());
        let observed = pollster::block_on(ensure_local(&thin, Some(&key))).unwrap();
        let facts = observed.facts().unwrap();
        let blocks: BTreeSet<Id> = find!(
            block: Id,
            pattern!(&facts, [{ ?block @ metadata::tag: &schema::block::KIND }])
        )
        .collect();
        assert_eq!(blocks.len(), 40, "the local import reads back whole");
    }

    /// A content fact's media type is part of its identity, and its name is
    /// part of the media type's: both ride in the identity COMMIT.
    #[test]
    fn a_media_type_stays_whole_in_the_identity_commit() {
        let fact = crate::blockdag::blob_fact(
            schema::content_fact::modality::IMAGE,
            schema::content_fact::direction::IN,
            vec![1u8, 2, 3],
            "image/png",
        )
        .unwrap();
        let (node, identity) = crate::blockdag::block_parts(None, None, fact).unwrap();
        let receipt =
            crate::blockdag::block_source_projection(schema::source_projection::SOURCE_CODEX, node)
                .unwrap();
        assert_eq!(identity.nodes().len(), 1);
        let annotations = receipt.facts().difference(identity.facts());
        let identity = identity.into_facts();
        let media: Vec<Id> = find!(
            media: Id,
            pattern!(&identity, [{ _?fact @ schema::content_fact::media_type: ?media }])
        )
        .collect();
        assert_eq!(media.len(), 1);
        let media = media[0];
        assert!(exists!(
            pattern!(&identity, [{ media @ metadata::name: _?name }])
        ));
        assert!(!exists!(
            pattern!(&annotations, [{ media @ metadata::name: _?name }])
        ));
        assert!(exists!(pattern!(&annotations, [{
            _?receipt @ schema::source_projection::projects_to: _?node
        }])));
    }

    /// A node's identity, as `block_parts` names it, is what its id derives
    /// from: its predecessors and the facts it holds, with their own
    /// identities and the kind tags. Its timestamp, and the receipt built
    /// around it with every receipt annotation, stay outside.
    #[test]
    fn block_parts_names_the_node_identity() {
        let parent = blockdag::block(
            None,
            None,
            blockdag::text_fact(
                schema::content_fact::modality::TEXT,
                schema::content_fact::direction::IN,
                "before",
            )
            .unwrap(),
        )
        .unwrap()
        .root()
        .unwrap();
        let epoch = Epoch::from_unix_seconds(1_700_000_000.0);
        let at: Inline<NsTAIInterval> = (epoch, epoch).try_to_inline().unwrap();
        let (node, identity) = blockdag::block_parts(
            [parent],
            Some(at),
            blockdag::text_fact(
                schema::content_fact::modality::TEXT,
                schema::content_fact::direction::OUT,
                "after",
            )
            .unwrap(),
        )
        .unwrap();
        let node_id = node.root().unwrap();
        let receipt =
            blockdag::block_source_projection(schema::source_projection::SOURCE_CLAUDE_CODE, node)
                .unwrap();
        let receipt = blockdag::annotate_source_projection(
            receipt,
            blockdag::ProjectionAnnotations {
                source_timestamp: Some(at),
                raw_role: Some("assistant".to_owned()),
                ..blockdag::ProjectionAnnotations::default()
            },
        )
        .unwrap();
        assert_eq!(identity.nodes(), &[node_id]);
        let annotations = receipt.facts().difference(identity.facts());
        let identity = identity.into_facts();
        assert!(exists!(pattern!(&identity, [{ node_id @
            schema::block::previous: &parent,
            metadata::tag: &schema::block::KIND,
        }])));
        assert!(exists!(
            pattern!(&identity, [{ node_id @ schema::block::holds: _?fact }, {
                _?fact @
                    metadata::tag: &schema::content_fact::KIND,
                    schema::content_fact::modality: &schema::content_fact::modality::TEXT,
                    schema::content_fact::direction: &schema::content_fact::direction::OUT,
                    schema::content_fact::payload: _?payload,
            }])
        ));
        assert!(exists!(
            pattern!(&annotations, [{ node_id @ schema::block::timestamp: at }])
        ));
        assert!(exists!(pattern!(&annotations, [{ _?receipt @
            schema::source_projection::projects_to: node_id,
            schema::source_projection::source_timestamp: at,
        }])));
        assert!(!exists!(
            pattern!(&identity, [{ node_id @ schema::block::timestamp: _?any }])
        ));
        assert!(!exists!(pattern!(&identity, [{
            _?receipt @ schema::source_projection::projects_to: node_id
        }])));
    }

    /// A session that grows, and a Claude Code session copied into a new
    /// session file (a resume keeps every message's time, role and model),
    /// bring back every closed piece of the first import as the same two
    /// COMMITs. Only the first import's last piece, which carries the leaf's
    /// session id, is not reused. Neither case needs the two-COMMIT split.
    #[test]
    fn growth_and_a_resumed_copy_reuse_both_commits_of_every_closed_piece() {
        let original = claude_transcript("resumed", 300);
        let grown = claude_transcript("resumed", 310);
        let copy = grown.replace(r#""sessionId":"resumed""#, r#""sessionId":"resumed-again""#);
        assert_ne!(copy, grown);
        for (label, later) in [("grown", &grown), ("resumed copy", &copy)] {
            let directory = TempDir::new().unwrap();
            let (pile, key) = fresh_archive(&directory, "archive");
            let first = import_source(&pile, &key, "resumed.jsonl", &original);
            let second = import_source(&pile, &key, "resumed-again.jsonl", later);
            assert!(first.pieces > 4, "{label}: {}", first.pieces);
            let closed = &first.commits[1..first.commits.len() - 2];
            let reused: BTreeSet<_> = second.commits.iter().copied().collect();
            let missing = closed
                .iter()
                .filter(|commit| !reused.contains(commit))
                .count();
            assert_eq!(missing, 0, "{label}: closed-piece COMMITs not reused");
        }
    }
}
