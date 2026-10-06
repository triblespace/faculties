//! Staging and publication for the Code collection.
//!
//! This is a near-verbatim copy of `ArchiveImportWriter` with the Archive scope
//! replaced by the Code scope and the block-DAG vocabulary replaced by the
//! extraction law. The two helpers it needs from `archive_collection.rs`
//! (`ensure_facts` and `fact_archive_contains`) are private there and that file
//! is in another agent's working set right now, so they are copied rather than
//! widened. Generalizing both into one `CollectionImportWriter<S>` is the
//! follow-up once that work lands; doing it first would mean editing a file
//! somebody else is holding.
//!
//! Commit granularity is one COMMIT per repository per run, never one per file.
//! `ArchiveImportWriter`'s own measurement is the reason: ~9.3 s of pile-opening
//! against ~1 s of projection put a 3,161-file backfill at 8.2 hours of pure
//! opening when the open was paid per unit. Pay it once.

use std::sync::Arc;
use triblespace::core::collection::AttachedSnapshot;

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::blob::encodings::{simplearchive::SimpleArchive, UnknownBlob};
use triblespace::core::blob::Blob;
use triblespace::core::collection::{
    Collection, CollectionCommit, CollectionSnapshotExt, CollectionStoreExt,
};
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::async_store::{AsyncBlobStoreAcquire, AsyncBlobStoreGet};
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::{BlobStorePut, SnapshotSource, StorageClose, Store};
use triblespace::prelude::*;

use crate::schemas::code::DEFAULT_SCOPE_ID;
use crate::storage::{FactArchive, FacultyStore};

/// Stage Code fragments for commit-last publication.
pub struct CodeImportWriter<P = FacultyStore> {
    pile: P,
    collection: Collection<SimpleArchive>,
    signer: SigningKey,
    current: FactArchive,
    delta: Fragment,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl CodeImportWriter {
    /// Open a synchronous import session. Async callers run the complete
    /// open/stage/close lifetime on a blocking worker, not inside their runtime.
    pub fn open(pile_path: &std::path::Path, key_path: Option<&std::path::Path>) -> Result<Self> {
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
                };
                if let Err(error) = writer.stage_fragment(crate::code::law_fragment()) {
                    return close_pile(
                        writer.pile,
                        Err(error),
                        "closing Code pile after extraction-law staging failed",
                    );
                }
                Ok(writer)
            }
            Err(error) => close_pile(
                pile,
                Err(error),
                "closing Code pile after failed open also failed",
            ),
        }
    }
}

impl<P> CodeImportWriter<P>
where
    P: Store + AsyncBlobStoreAcquire + Send,
    P::Snapshot: AsyncBlobStoreGet,
{
    /// Borrow the full caller-owned store; no local-backend guard spans I/O.
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
        };
        writer.stage_fragment(crate::code::law_fragment())?;
        Ok(writer)
    }

    fn prepare(
        pile: &mut P,
        signer: &SigningKey,
        runtime: &Arc<tokio::runtime::Runtime>,
    ) -> Result<(Collection<SimpleArchive>, FactArchive)> {
        let source = crate::collection_names::write_target_acquiring(
            pile,
            DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            None,
            runtime,
        )?;
        let (succinct, rank9) = crate::storage::fact_pair(pile, source)?;
        runtime
            .block_on(async {
                crate::storage::tolerate_own_lag(pile.maintain_attached(succinct, signer).await)?;
                crate::storage::tolerate_own_lag(pile.maintain_attached(rank9, signer).await)
            })
            .context("maintain Code import facts")?;
        let reader = crate::storage::AcquiringReader::new(pile.snapshot()?, runtime.clone());
        let current =
            crate::storage::acquire_facts(&reader, rank9).context("read Code import facts")?;
        Ok((source, current))
    }

    /// Whether this exact entity is already catalogued.
    ///
    /// The unit fast path: a hit means these exact bytes at this exact path in
    /// this exact repo are already here, so the file is not parsed at all. One
    /// Blake3 and one `exists!` per unchanged file is what makes a re-ingest of
    /// a corpus with three changed files cost three parses.
    pub fn holds_entity(&self, entity: Id) -> bool {
        use triblespace::core::metadata;
        exists!((tag: Id), pattern!(&self.current, [{ entity @ metadata::tag: ?tag }]))
            || exists!((tag: Id), pattern!(self.delta.facts(), [{ entity @ metadata::tag: ?tag }]))
    }

    pub fn stage_fragment(&mut self, fragment: Fragment) -> Result<()> {
        // A Fragment is the independently derivable source unit. A wholly known
        // candidate is an idempotent replay and can be skipped. Once it
        // contributes even one new fact, retain its complete closure in this
        // COMMIT element — set union makes that duplication semantically free,
        // while exact homomorphisms can derive every leaf without depending on
        // an implicit merge with historical commits.
        let (_, facts, metafacts, blobs) = fragment.into_parts();
        if facts.iter().all(|fact| {
            self.delta.facts().contains(fact) || fact_archive_contains(&self.current, fact)
        }) {
            return Ok(());
        }

        // Embedded payloads dominate an ingest's resident memory — a unit
        // carries its whole file. Append each content-addressed dependency
        // immediately, keeping payload bytes out of the long-lived delta. They
        // stay semantically unreachable until a signed COMMIT names the facts
        // referencing them.
        let embedded = embedded_blobs(blobs);
        stage_embedded_blobs(&mut self.pile, embedded)?;

        self.delta += Fragment::from_parts(facts, metafacts, Default::default());
        Ok(())
    }

    pub fn delta_len(&self) -> usize {
        self.delta.facts().len()
    }

    /// Publish the staged delta as ONE signed COMMIT and keep the pile open.
    ///
    /// `current` MUST absorb what was just published, or the next unit's
    /// idempotence check would re-stage facts this commit already carries.
    pub fn commit_unit(&mut self) -> Result<Option<CollectionCommit>> {
        if self.delta.facts().is_empty() {
            return Ok(None);
        }
        let fragment = std::mem::replace(&mut self.delta, Fragment::empty());
        let published = fragment.facts().clone();
        crate::collection_names::require_command_write_admission_acquiring(
            &mut self.pile,
            self.collection,
            &self.signer,
            "Code",
            "code find",
            &self.runtime,
        )?;
        let commit = self
            .pile
            .commit(self.collection, &self.signer, fragment)
            .context("commit authored Code projection unit")?;
        self.current = extend_archive(&self.current, &published);
        Ok(Some(commit))
    }
}

impl CodeImportWriter {
    /// Close the pile, publishing any still-staged delta first.
    pub fn close<T>(mut self, surrounding: Result<T>) -> Result<T> {
        let result = surrounding.and_then(|value| {
            self.commit_unit()?;
            Ok(value)
        });
        close_pile(
            self.pile,
            result,
            "closing Code pile after failure also failed",
        )
    }
}

fn stage_embedded_blobs<S>(store: &mut S, embedded: Vec<Blob<UnknownBlob>>) -> Result<()>
where
    S: BlobStorePut,
{
    for blob in embedded {
        store
            .put::<UnknownBlob, _>(blob)
            .context("stage Code embedded blob")?;
    }
    Ok(())
}

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

fn close_pile<T>(pile: impl StorageClose, result: Result<T>, failure_context: &str) -> Result<T> {
    match (result, pile.close()) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(close_error)) => Err(anyhow!("close Code pile: {close_error}")),
        (Err(error), Err(close_error)) => {
            Err(error.context(format!("{failure_context} also failed: {close_error}")))
        }
    }
}

/// Maintain the Code collection's attached fact pair and return the attached
/// read. Storage work, not domain decoding: consumers choose their own typed
/// queries over [`crate::storage::FactView::facts`].
pub async fn ensure_facts(
    pile: &mut Pile,
    source: Collection<SimpleArchive>,
    signer: &SigningKey,
) -> Result<AttachedSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>> {
    let succinct = pile
        .attach::<SuccinctArchiveBlob>(source, ())
        .context("register Succinct Code fact collection")?;
    let rank9 = pile
        .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
        .context("register Rank9 Code fact collection")?;
    // The source is carried and its frontier attached, Succinct first; a
    // commit left unattached is read from its own bytes.
    crate::storage::tolerate_own_lag(pile.maintain_attached(succinct, signer).await)
        .context("maintain Succinct Code fact collection")?;
    crate::storage::tolerate_own_lag(pile.maintain_attached(rank9, signer).await)
        .context("maintain Rank9 Code fact collection")?;
    pile.snapshot()
        .context("freeze maintained Code facts")?
        .attached(rank9)
        .context("attach Code fact collection")
}

/// Attach the Code facts for reading.
///
/// A read ensures the collection's own accelerated FACT representation, which
/// is what `pattern!` is answered from and therefore not optional. It does not
/// touch the LEXICAL covers: `code index` maintains those, once and explicitly.
/// Correctness never depends on a BM25 cover — a stale one makes `code search`
/// say so and name the verb that fixes it — and a collection this size that
/// re-indexed itself on every read would make `code find` cost what an `orient
/// wake` costs.
pub fn ensure_local_with_storage(
    storage: &crate::storage::Storage,
) -> Result<AttachedSnapshot<PileSnapshot, Rank9AcceleratedSuccinctArchiveBlob>> {
    storage.with_store(|store, signer, runtime| {
        let source = crate::collection_names::write_target_acquiring(
            store,
            DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            None,
            runtime,
        )?;
        let mut local = store.store();
        let pile = &mut *local;
        pollster::block_on(ensure_facts(pile, source, signer))
    })
}
