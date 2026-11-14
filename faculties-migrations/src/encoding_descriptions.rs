//! Additive backfill of value-encoding descriptions into old COMMIT metadata.
//!
//! Since triblespace-rs `ee27c4cb`, every attribute record that `entity!`
//! carries into a commit's metadata archive holds the description of the
//! attribute's value encoding (`attribute::encoding_description::<S>()`), not
//! only its id under `metadata::value_encoding`. Commits written before that
//! name encodings without saying what they are.
//!
//! For every `COMMIT(collection, data, M)` in an explicitly planned collection
//! that the given key signed, this module reads `M`, adds the description of
//! every encoding `M` names under `metadata::value_encoding` that the frozen
//! [`Catalog`] knows, and so builds `M'`. When `M' == M` nothing is emitted.
//! Otherwise it signs `COMMIT(collection, data, M')` with the same key. Nothing
//! is removed or rewritten: `M'` is a superset of `M`, and the old commit stays.
//!
//! Re-running is a no-op without bookkeeping: Ed25519 signing is deterministic,
//! a `SimpleArchive` is canonical, and the pile skips a record or blob it
//! already holds. A commit written with descriptions maps to itself.
//!
//! Encodings the catalog does not know are counted and listed, never fatal;
//! so are attributes with no `value_encoding`, metadata archives that are not
//! resident or do not decode, and commits whose signature does not verify.
//! Commits by any other key are left alone: each author re-signs its own.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;

use faculties::storage::{load_signer, open_pile_strict};
use triblespace::core::attribute::{encoding_description, Attribute};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::UnknownBlob;
use triblespace::core::blob::{Blob, IntoBlob, TryFromBlob};
use triblespace::core::collection::records::CollectionHandle;
use triblespace::core::collection::{
    CollectionCommit, CollectionRead, CollectionRecord, CollectionRecordSelector, CollectionStore,
};
use triblespace::core::id::Id;
use triblespace::core::inline::encodings::hash::Handle;
use triblespace::core::inline::{Inline, InlineEncoding};
use triblespace::core::macros::{find, pattern};
use triblespace::core::metadata::{self, MetaDescribe};
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::{BlobStoreGet, BlobStoreMeta, BlobStorePut, SnapshotSource};
use triblespace::core::trible::TribleSet;

/// Pile frame sizes, for the byte estimate: every record is one 256-byte
/// frame, and a blob is a 256-byte envelope plus its payload padded to 256.
const FRAME: u64 = 256;

fn blob_frame_bytes(len: usize) -> u64 {
    FRAME + (len as u64).div_ceil(FRAME) * FRAME
}

/// One encoding the catalog can describe.
pub struct CatalogEntry {
    /// `<S as MetaDescribe>::id()`, the value `metadata::value_encoding` holds.
    pub id: Id,
    /// The Rust type, for the report only.
    pub name: &'static str,
    /// The facts of `encoding_description::<S>()`.
    pub facts: TribleSet,
    /// The blobs those facts reference (texts and value formatters).
    pub blobs: Vec<Blob<UnknownBlob>>,
}

/// The frozen, typed list of encodings this migration describes.
///
/// Each entry is computed exactly as the `attributes!` record computes it
/// (`encoding_description::<S>()`), so an enriched archive matches what the
/// current macro writes. Entries are sorted by id; the list is built once per
/// run and only read afterwards.
pub struct Catalog {
    entries: Vec<CatalogEntry>,
}

impl Catalog {
    /// Every encoding named by an attribute the faculties schemas, the
    /// secrets schema, and the triblespace core and net crates declare, plus
    /// the retired `ShortString`. `catalog_covers_every_schema_encoding`
    /// keeps the list honest; mary's encodings are deliberately absent.
    #[allow(deprecated)]
    pub fn frozen() -> Self {
        use faculties::schemas::embeddings::Embedding768;
        use faculties::schemas::home::Sha256;
        use faculties::secrets::encrypted::Encrypted;
        use triblespace::core::inline::encodings::socketaddr::SocketAddress;
        use triblespace::core::inline::encodings::UnknownInline;
        use triblespace::core::prelude::blobencodings::{
            RawBytes, SimpleArchive, UTF8String, WasmCode,
        };
        use triblespace::core::prelude::inlineencodings::{
            Blake3, Boolean, ED25519PublicKey, ED25519RComponent, ED25519SComponent, GenId, Handle,
            Hash, LineLocation, NsDuration, NsTAIInterval, ShortString, F64, I256BE, R256BE,
            R256LE, U256BE,
        };

        let mut catalog = Self {
            entries: Vec::new(),
        };
        catalog.add::<Boolean>();
        catalog.add::<ED25519PublicKey>();
        catalog.add::<ED25519RComponent>();
        catalog.add::<ED25519SComponent>();
        catalog.add::<F64>();
        catalog.add::<GenId>();
        catalog.add::<Hash<Blake3>>();
        catalog.add::<Hash<Sha256>>();
        catalog.add::<I256BE>();
        catalog.add::<LineLocation>();
        catalog.add::<NsDuration>();
        catalog.add::<NsTAIInterval>();
        catalog.add::<R256BE>();
        catalog.add::<R256LE>();
        catalog.add::<ShortString>();
        catalog.add::<SocketAddress>();
        catalog.add::<U256BE>();
        catalog.add::<UnknownInline>();
        catalog.add::<Handle<RawBytes>>();
        catalog.add::<Handle<SimpleArchive>>();
        catalog.add::<Handle<UTF8String>>();
        catalog.add::<Handle<WasmCode>>();
        catalog.add::<Handle<Embedding768>>();
        catalog.add::<Handle<Encrypted<RawBytes>>>();
        catalog.add::<Handle<Encrypted<SimpleArchive>>>();
        catalog.add::<Handle<Encrypted<UTF8String>>>();
        // Encodings whose types this crate cannot name without a new
        // dependency: taken from the attribute that declares them.
        catalog.add_of(&*faculties::schemas::files::file::embedding);
        catalog.add_of(&*faculties::schemas::memory::search_index::index);
        catalog
    }

    fn add<S: MetaDescribe>(&mut self) {
        let id = <S as MetaDescribe>::id();
        if self.get(id).is_some() {
            return;
        }
        let (_, facts, _, mut blobs) = encoding_description::<S>().into_parts();
        let blobs = blobs
            .snapshot()
            .expect("MemoryBlobStore::snapshot is infallible")
            .into_iter()
            .map(|(_, blob)| blob)
            .collect();
        let position = self.entries.partition_point(|entry| entry.id < id);
        self.entries.insert(
            position,
            CatalogEntry {
                id,
                name: std::any::type_name::<S>(),
                facts,
                blobs,
            },
        );
    }

    fn add_of<S: MetaDescribe + InlineEncoding>(&mut self, _declared_by: &Attribute<S>) {
        self.add::<S>();
    }

    /// The entry for one encoding id, if the catalog knows it.
    pub fn get(&self, id: Id) -> Option<&CatalogEntry> {
        self.entries
            .binary_search_by(|entry| entry.id.cmp(&id))
            .ok()
            .map(|index| &self.entries[index])
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// Union of every entry's facts.
    pub fn facts(&self) -> TribleSet {
        let mut facts = TribleSet::new();
        for entry in &self.entries {
            facts.union(entry.facts.clone());
        }
        facts
    }

    /// Handle of the canonical archive of [`Self::facts`]. Two builds with the
    /// same hash write identical `M'` for identical `M`.
    pub fn hash(&self) -> Inline<Handle<SimpleArchive>> {
        let blob: Blob<SimpleArchive> = self.facts().to_blob();
        blob.get_handle()
    }
}

/// What enriching one metadata archive found.
pub struct Enrichment {
    /// `M'`: the archive's facts plus every known, missing description.
    pub facts: TribleSet,
    /// Description blobs of the encodings whose facts were missing.
    pub blobs: Vec<Blob<UnknownBlob>>,
    /// Encodings whose description this archive gains.
    pub described: Vec<Id>,
    /// Encoding ids the catalog does not know, with the attributes naming them.
    pub unknown: BTreeMap<Id, BTreeSet<Id>>,
    /// Attributes in the archive with no `value_encoding` fact at all.
    pub bare_attributes: BTreeSet<Id>,
}

impl Enrichment {
    /// `M' == M`: `M'` is a superset, so equal length is equality.
    pub fn unchanged(&self, archive: &TribleSet) -> bool {
        self.facts.len() == archive.len()
    }
}

/// Add the description of every encoding `archive` names that `catalog`
/// knows. One pass, exactly as the macro does it: only encodings the archive
/// itself names, never encodings the added descriptions might name.
pub fn enrich(catalog: &Catalog, archive: &TribleSet) -> Enrichment {
    let mut facts = archive.clone();
    let mut blobs = Vec::new();
    let mut described = Vec::new();
    let mut unknown: BTreeMap<Id, BTreeSet<Id>> = BTreeMap::new();

    let mut named: BTreeMap<Id, BTreeSet<Id>> = BTreeMap::new();
    for (attribute, encoding) in find!(
        (attribute: Id, encoding: Id),
        pattern!(archive, [{ ?attribute @ metadata::value_encoding: ?encoding }])
    ) {
        named.entry(encoding).or_default().insert(attribute);
    }
    for (encoding, attributes) in named {
        let Some(entry) = catalog.get(encoding) else {
            unknown.insert(encoding, attributes);
            continue;
        };
        if entry.facts.difference(archive).is_empty() {
            continue;
        }
        facts.union(entry.facts.clone());
        blobs.extend(entry.blobs.iter().cloned());
        described.push(encoding);
    }

    let typed: BTreeSet<Id> = find!(
        attribute: Id,
        pattern!(archive, [{ ?attribute @ metadata::value_encoding: _?encoding }])
    )
    .collect();
    let bare_attributes = find!(
        attribute: Id,
        pattern!(archive, [{ ?attribute @ metadata::tag: metadata::KIND_ATTRIBUTE }])
    )
    .chain(find!(
        attribute: Id,
        pattern!(archive, [{ _?usage @ metadata::attribute: ?attribute }])
    ))
    .filter(|attribute| !typed.contains(attribute))
    .collect();

    Enrichment {
        facts,
        blobs,
        described,
        unknown,
        bare_attributes,
    }
}

/// One unknown encoding id within one collection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UnknownEncoding {
    /// Distinct metadata archives naming it.
    pub archives: usize,
    /// The signer's commits whose metadata names it.
    pub commits: usize,
    /// Attributes declared with it.
    pub attributes: BTreeSet<Id>,
}

/// Everything one planned collection showed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectionReport {
    pub collection: CollectionHandle,
    /// Every COMMIT record in the collection.
    pub commits: usize,
    /// Commits by the signer before this run.
    pub signer_commits: usize,
    /// Commits by other keys, left alone.
    pub other_signers: usize,
    /// Commits whose signature does not verify, skipped.
    pub bad_signatures: usize,
    /// Distinct metadata archives of the signer's commits.
    pub archives: usize,
    /// Of those, archives `M` whose `M'` differs.
    pub archives_enriched: usize,
    /// Of those, archives that already describe every known encoding.
    pub archives_current: usize,
    /// Metadata archives that are not resident (distinct).
    pub non_resident: usize,
    /// Metadata archives that are not a canonical SimpleArchive (distinct).
    pub undecodable: usize,
    /// Signer commits whose metadata was not readable for either reason.
    pub unreadable_commits: usize,
    /// Signer commits whose `M'` differs from `M`.
    pub enriched_commits: usize,
    /// Of those, the `COMMIT(collection, data, M')` already in the collection.
    pub present_commits: usize,
    /// `COMMIT(collection, data, M')` records this run writes (or would).
    pub new_commits: usize,
    /// Blobs this run writes (or would): new archives and description blobs
    /// not resident before the run, each counted once per run.
    pub new_blobs: usize,
    /// Framed pile bytes of `new_commits` and `new_blobs`.
    pub new_bytes: u64,
    pub unknown: BTreeMap<Id, UnknownEncoding>,
    /// Attributes with no `value_encoding`, with the archives naming them.
    pub bare_attributes: BTreeMap<Id, usize>,
}

/// One run over every planned collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Report {
    pub catalog_hash: Inline<Handle<SimpleArchive>>,
    pub signer: [u8; 32],
    pub applied: bool,
    pub collections: Vec<CollectionReport>,
}

impl Report {
    pub fn new_commits(&self) -> usize {
        self.collections.iter().map(|c| c.new_commits).sum()
    }

    pub fn new_blobs(&self) -> usize {
        self.collections.iter().map(|c| c.new_blobs).sum()
    }

    pub fn new_bytes(&self) -> u64 {
        self.collections.iter().map(|c| c.new_bytes).sum()
    }
}

/// How one distinct metadata archive enriched.
#[derive(Clone)]
enum Outcome {
    NonResident,
    Undecodable,
    Current {
        unknown: BTreeMap<Id, BTreeSet<Id>>,
        bare: BTreeSet<Id>,
    },
    Enriched {
        archive: Inline<Handle<SimpleArchive>>,
        unknown: BTreeMap<Id, BTreeSet<Id>>,
        bare: BTreeSet<Id>,
    },
}

/// Scratch for one run: the outcome of every distinct archive seen so far and
/// every blob already counted (and, when applying, written). It lives for one
/// operation and is dropped with it.
struct Run<'a> {
    catalog: &'a Catalog,
    snapshot: &'a PileSnapshot,
    signer: &'a SigningKey,
    apply: bool,
    outcomes: BTreeMap<[u8; 32], Outcome>,
    staged: BTreeSet<[u8; 32]>,
    planned: BTreeSet<CollectionCommit>,
}

impl Run<'_> {
    /// Write (when applying) one blob, and count it when it was not resident
    /// before the run and has not been counted yet.
    fn stage(
        &mut self,
        store: &mut Pile,
        blob: Blob<UnknownBlob>,
        report: &mut CollectionReport,
    ) -> Result<()> {
        let handle = blob.get_handle();
        if !self.staged.insert(handle.raw) {
            return Ok(());
        }
        let resident = self
            .snapshot
            .metadata(handle)
            .map_err(|error| anyhow!("check blob residency: {error}"))?
            .is_some();
        if !resident {
            report.new_blobs += 1;
            report.new_bytes += blob_frame_bytes(blob.bytes.len());
        }
        if self.apply {
            // A resident blob is put too: the pile returns the existing handle
            // without appending, and the new commit's closure is certain.
            store
                .put::<UnknownBlob, _>(blob)
                .map_err(|error| anyhow!("put blob: {error:?}"))?;
        }
        Ok(())
    }

    fn outcome(
        &mut self,
        store: &mut Pile,
        metadata: Inline<Handle<SimpleArchive>>,
        report: &mut CollectionReport,
    ) -> Result<Outcome> {
        if let Some(outcome) = self.outcomes.get(&metadata.raw) {
            return Ok(outcome.clone());
        }
        let outcome = match self
            .snapshot
            .get::<Blob<SimpleArchive>, SimpleArchive>(metadata)
        {
            Err(_) => Outcome::NonResident,
            Ok(blob) => match TribleSet::try_from_blob(blob) {
                Err(_) => Outcome::Undecodable,
                Ok(archive) => {
                    let enrichment = enrich(self.catalog, &archive);
                    if enrichment.unchanged(&archive) {
                        Outcome::Current {
                            unknown: enrichment.unknown,
                            bare: enrichment.bare_attributes,
                        }
                    } else {
                        for blob in enrichment.blobs {
                            self.stage(store, blob, report)?;
                        }
                        let enriched: Blob<SimpleArchive> = enrichment.facts.to_blob();
                        let archive = enriched.get_handle();
                        self.stage(store, enriched.transmute(), report)?;
                        Outcome::Enriched {
                            archive,
                            unknown: enrichment.unknown,
                            bare: enrichment.bare_attributes,
                        }
                    }
                }
            },
        };
        self.outcomes.insert(metadata.raw, outcome.clone());
        Ok(outcome)
    }

    fn collection(
        &mut self,
        store: &mut Pile,
        collection: CollectionHandle,
    ) -> Result<CollectionReport> {
        let mut report = CollectionReport {
            collection,
            commits: 0,
            signer_commits: 0,
            other_signers: 0,
            bad_signatures: 0,
            archives: 0,
            archives_enriched: 0,
            archives_current: 0,
            non_resident: 0,
            undecodable: 0,
            unreadable_commits: 0,
            enriched_commits: 0,
            present_commits: 0,
            new_commits: 0,
            new_blobs: 0,
            new_bytes: 0,
            unknown: BTreeMap::new(),
            bare_attributes: BTreeMap::new(),
        };
        let records = self
            .snapshot
            .select_records(&BTreeSet::from([CollectionRecordSelector::Collection(
                collection,
            )]))
            .map_err(|error| anyhow!("select collection records: {error}"))?;
        let held: BTreeSet<CollectionCommit> = records
            .iter()
            .filter_map(|record| match record {
                CollectionRecord::Commit(commit) => Some(*commit),
                _ => None,
            })
            .collect();
        let signer = self.signer.verifying_key().to_bytes();
        let mut seen = BTreeSet::new();

        for commit in &held {
            report.commits += 1;
            if commit.verify_strict().is_err() {
                report.bad_signatures += 1;
                continue;
            }
            if commit.public_key().raw != signer {
                report.other_signers += 1;
                continue;
            }
            report.signer_commits += 1;
            let first = seen.insert(commit.metadata().raw);
            let outcome = self.outcome(store, commit.metadata(), &mut report)?;
            if first {
                report.archives += 1;
            }
            let (unknown, bare, archive) = match outcome {
                Outcome::NonResident | Outcome::Undecodable => {
                    report.unreadable_commits += 1;
                    if first {
                        match outcome {
                            Outcome::NonResident => report.non_resident += 1,
                            _ => report.undecodable += 1,
                        }
                    }
                    continue;
                }
                Outcome::Current { unknown, bare } => {
                    if first {
                        report.archives_current += 1;
                    }
                    (unknown, bare, None)
                }
                Outcome::Enriched {
                    archive,
                    unknown,
                    bare,
                } => {
                    if first {
                        report.archives_enriched += 1;
                    }
                    (unknown, bare, Some(archive))
                }
            };
            for (encoding, attributes) in unknown {
                let entry = report.unknown.entry(encoding).or_default();
                entry.commits += 1;
                if first {
                    entry.archives += 1;
                    entry.attributes.extend(attributes);
                }
            }
            if first {
                for attribute in bare {
                    *report.bare_attributes.entry(attribute).or_default() += 1;
                }
            }
            let Some(archive) = archive else {
                continue;
            };
            report.enriched_commits += 1;
            let successor = CollectionCommit::sign(self.signer, collection, commit.data(), archive);
            if held.contains(&successor) || !self.planned.insert(successor) {
                report.present_commits += 1;
                continue;
            }
            report.new_commits += 1;
            report.new_bytes += FRAME;
            if self.apply {
                store
                    .insert(CollectionRecord::Commit(successor))
                    .map_err(|error| anyhow!("insert enriched COMMIT: {error}"))?;
            }
        }
        Ok(report)
    }
}

/// Enrich every planned collection against one frozen snapshot of `store`.
/// Writes only when `apply` is set.
pub fn run(
    store: &mut Pile,
    catalog: &Catalog,
    signer: &SigningKey,
    plan: &[CollectionHandle],
    apply: bool,
) -> Result<Report> {
    let snapshot = store
        .snapshot()
        .context("freeze encoding-descriptions source snapshot")?;
    let mut run = Run {
        catalog,
        snapshot: &snapshot,
        signer,
        apply,
        outcomes: BTreeMap::new(),
        staged: BTreeSet::new(),
        planned: BTreeSet::new(),
    };
    let mut collections = Vec::with_capacity(plan.len());
    for collection in plan {
        collections.push(run.collection(store, *collection)?);
    }
    drop(run);
    drop(snapshot);
    if apply {
        store
            .flush()
            .map_err(|error| anyhow!("flush pile: {error:?}"))?;
    }
    Ok(Report {
        catalog_hash: catalog.hash(),
        signer: signer.verifying_key().to_bytes(),
        applied: apply,
        collections,
    })
}

/// Collection handles, one per line. Blank lines and `#` comments are
/// skipped; a `blake3:` prefix is accepted, and anything after the first
/// whitespace on a line is ignored, so a census table pastes in directly.
pub fn parse_plan(text: &str) -> Result<Vec<CollectionHandle>> {
    let mut plan = Vec::new();
    let mut seen = BTreeSet::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some(token) = line.split_whitespace().next() else {
            continue;
        };
        let hex_text = token.strip_prefix("blake3:").unwrap_or(token);
        let mut raw = [0_u8; 32];
        hex::decode_to_slice(hex_text, &mut raw).map_err(|_| {
            anyhow!(
                "plan line {}: {token:?} is not a 64-hex-digit collection handle",
                number + 1
            )
        })?;
        if seen.insert(raw) {
            plan.push(Inline::new(raw));
        }
    }
    if plan.is_empty() {
        bail!("the plan names no collection");
    }
    Ok(plan)
}

/// Refuse unless `expected` names this build's catalog hash.
pub fn check_catalog(catalog: &Catalog, expected: &str) -> Result<()> {
    let expected = expected.trim();
    let expected = expected.strip_prefix("blake3:").unwrap_or(expected);
    let actual = hex::encode(catalog.hash().raw);
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("catalog hash is blake3:{actual}, expected {expected}; refusing to run");
    }
    Ok(())
}

/// Open `pile`, run, and close it. The signer is the key at `key`.
pub fn run_path(
    pile: &Path,
    key: &Path,
    catalog: &Catalog,
    plan: &[CollectionHandle],
    apply: bool,
) -> Result<Report> {
    let signer = load_signer(pile, Some(key)).context("load encoding-descriptions signer")?;
    let mut store = open_pile_strict(pile)?;
    let result = run(&mut store, catalog, &signer, plan, apply);
    let close = store
        .close()
        .map_err(|error| anyhow!("close pile: {error:?}"));
    match (result, close) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

/// Every collection in `pile` with its COMMIT count, to write a plan from.
pub fn list_path(pile: &Path) -> Result<Vec<(CollectionHandle, usize)>> {
    let mut store = open_pile_strict(pile)?;
    let listed = (|| {
        let snapshot = store.snapshot().context("freeze listing snapshot")?;
        let collections = snapshot
            .collections()
            .map_err(|error| anyhow!("list collections: {error}"))?;
        collections
            .into_iter()
            .map(|collection| {
                let records = snapshot
                    .select_records(&BTreeSet::from([CollectionRecordSelector::Collection(
                        collection,
                    )]))
                    .map_err(|error| anyhow!("select collection records: {error}"))?;
                let commits = records
                    .iter()
                    .filter(|record| matches!(record, CollectionRecord::Commit(_)))
                    .count();
                Ok((collection, commits))
            })
            .collect::<Result<Vec<_>>>()
    })();
    let close = store
        .close()
        .map_err(|error| anyhow!("close pile: {error:?}"));
    match (listed, close) {
        (Ok(listed), Ok(())) => Ok(listed),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::path::PathBuf;

    use super::*;
    use faculties::storage::initialize_signer;
    use triblespace::core::collection::{
        AdmissionPolicy, Collection, CollectionPolicy, CollectionStoreExt,
    };
    use triblespace::core::id::ExclusiveId;
    use triblespace::core::id_hex;
    use triblespace::core::macros::entity;
    use triblespace::core::prelude::blobencodings::UTF8String;
    use triblespace::core::prelude::inlineencodings::NsTAIInterval;
    use triblespace::core::repo::potential_handles;
    use triblespace::core::trible::Fragment;

    /// Encoding id no build knows. Minted with `trible genid` on 2026-11-14.
    const UNKNOWN_ENCODING: Id = id_hex!("DB0A36D2D52D4E3E304D4479453FB81F");
    /// Attribute declared with [`UNKNOWN_ENCODING`]. Minted 2026-11-14.
    const UNKNOWN_ATTRIBUTE: Id = id_hex!("9E6AF952F9EE61F9B25F9F21810717C2");
    /// Attribute record with no `value_encoding`. Minted 2026-11-14.
    const BARE_ATTRIBUTE: Id = id_hex!("ED4C93F5B4EEA8CD9C8D72FFDE7B5BB1");
    /// A goal's parent in the sample fragment. Minted 2026-11-14.
    const PARENT: Id = id_hex!("CC4E8F81DE23A6749A6092B8D9294ADA");

    /// Every public `attributes!` module of the faculties schemas, the
    /// secrets schema, and the core metadata vocabulary.
    fn schema_descriptions() -> TribleSet {
        use faculties::schemas as s;
        let modules: Vec<fn() -> Fragment> = vec![
            s::alarm::alarm::describe,
            s::archive::archive::describe,
            s::archive::import_schema::describe,
            s::archive::search_index::describe,
            s::archive_v3::citation::describe,
            s::archive_v3::fact::describe,
            s::archive_v3::gloss::describe,
            s::archive_v3::media_type::describe,
            s::archive_v3::node::describe,
            s::archive_v3::node::ends::describe,
            s::archive_v3::occurrence::describe,
            s::archive_v3::renderer::describe,
            s::archive_v3::window::describe,
            s::archive_v3::witness::describe,
            s::blockdag::block::describe,
            s::blockdag::content_fact::describe,
            s::blockdag::source_projection::describe,
            s::body::capture::describe,
            s::body::intent::describe,
            s::care::care::describe,
            s::code::attrs::describe,
            s::compass::board::describe,
            s::config::config::describe,
            s::decide::decide::describe,
            s::decide::factor::describe,
            s::decide::resolution::describe,
            s::discord::discord::describe,
            s::embeddings::attr::describe,
            s::files::file::describe,
            s::habit::attrs::describe,
            s::headspace::playground_config::describe,
            s::home::home::describe,
            s::imagine::imagine::describe,
            s::linkedin::describe,
            s::mail::acceptance::describe,
            s::mail::account::describe,
            s::mail::attachment_occurrence::describe,
            s::mail::attempt::describe,
            s::mail::draft::describe,
            s::mail::imported::describe,
            s::mail::imported_legacy::describe,
            s::mail::observation::describe,
            s::mail::projection::describe,
            s::mail::read::describe,
            s::mail::wire::describe,
            s::memory::archive_import_schema::describe,
            s::memory::archive_schema::describe,
            s::memory::comb::describe,
            s::memory::ctx::describe,
            s::memory::search_index::describe,
            s::message::local::describe,
            s::orient::presentation::describe,
            s::patience::exec_schema::describe,
            s::planner::cancellation::describe,
            s::planner::event::describe,
            s::planner::note::describe,
            s::posture::posture::describe,
            s::quota::provider_status::describe,
            s::quota::quota::describe,
            s::reason::reason_schema::describe,
            s::relations::group::describe,
            s::relations::identity::describe,
            s::relations::lifecycle::describe,
            s::relations::profile::describe,
            s::relations::relations::describe,
            s::secrets_import::describe,
            s::status::status::describe,
            s::swarm_health::attrs::describe,
            s::teams::teams::describe,
            s::triage::cog::describe,
            s::triage::config::describe,
            s::triage::context::describe,
            s::triage::exec::describe,
            s::triage::local::describe,
            s::triage::model_chat::describe,
            s::triage::reason::describe,
            s::voice::route::describe,
            s::voice::utterance::describe,
            s::web::web_schema::describe,
            s::wiki::attrs::describe,
            s::workspace::describe,
            faculties::secrets::schema::describe,
            metadata::describe,
        ];
        let mut facts = TribleSet::new();
        for describe in modules {
            let (_, module_facts, metafacts, _) = describe().into_parts();
            facts.union(module_facts);
            facts.union(metafacts);
        }
        facts
    }

    fn named_encodings(facts: &TribleSet) -> BTreeSet<Id> {
        find!(
            encoding: Id,
            pattern!(facts, [{ _?attribute @ metadata::value_encoding: ?encoding }])
        )
        .collect()
    }

    /// The metadata a pre-`ee27c4cb` build wrote for the same fragment: the
    /// attribute records (identity and core) and usage annotations, without
    /// the value-encoding descriptions that commit added to each record.
    fn pre_ee27c4cb(metafacts: &TribleSet) -> TribleSet {
        let records: BTreeSet<Id> = find!(
            entity: Id,
            pattern!(metafacts, [{ ?entity @ metadata::tag: metadata::KIND_ATTRIBUTE }])
        )
        .chain(find!(
            entity: Id,
            pattern!(metafacts, [{ ?entity @ metadata::tag: metadata::KIND_ATTRIBUTE_USAGE }])
        ))
        .collect();
        metafacts
            .iter()
            .filter(|trible| records.contains(trible.e()))
            .copied()
            .collect()
    }

    /// A fragment naming five encodings, two only reachable via attributes.
    fn sample_fragment(title: &str) -> Fragment {
        use faculties::schemas::compass::board;
        use faculties::schemas::embeddings::{attr, Embedding768};
        use faculties::schemas::{files, memory};
        entity! {
            board::title: title.to_owned(),
            board::parent: PARENT,
            attr::embedding: Inline::<Handle<Embedding768>>::new([7; 32]),
            files::file::embedding: Inline::new([9; 32]),
            memory::search_index::indexed_at: Inline::<NsTAIInterval>::new([0; 32]),
        }
    }

    fn blob_handles(blobs: &[Blob<UnknownBlob>]) -> BTreeSet<[u8; 32]> {
        blobs.iter().map(|blob| blob.get_handle().raw).collect()
    }

    #[test]
    fn catalog_covers_every_schema_encoding() {
        let catalog = Catalog::frozen();
        let union = schema_descriptions();
        let named = named_encodings(&union);
        assert!(named.len() > 10, "only {} encodings named", named.len());
        let missing: Vec<String> = named
            .iter()
            .filter(|encoding| catalog.get(**encoding).is_none())
            .map(|encoding| format!("{encoding:X}"))
            .collect();
        assert!(missing.is_empty(), "catalog misses {missing:?}");

        // Cross-check against the macro: every description the current
        // `attributes!` records carry is exactly the catalog's, so the union
        // gains nothing from enrichment.
        assert!(enrich(&catalog, &union).unchanged(&union));
        for encoding in &named {
            let entry = catalog.get(*encoding).unwrap();
            assert!(
                entry.facts.difference(&union).is_empty(),
                "{} described differently than the macro does",
                entry.name
            );
        }

        // One pass is a fixpoint: no description names a further encoding.
        assert!(named_encodings(&catalog.facts()).is_empty());
        // ShortString is retired and still described.
        assert!(catalog
            .get(id_hex!("2D848DB0AF112DB226A6BF1A3640D019"))
            .is_some());
    }

    #[test]
    fn enriching_the_pre_ee27c4cb_shape_reproduces_the_current_archive() {
        let catalog = Catalog::frozen();
        let fragment = sample_fragment("golden");
        let current = fragment.metafacts().clone();
        let old = pre_ee27c4cb(&current);
        assert!(
            old.len() < current.len(),
            "fragment carries no descriptions"
        );
        assert!(!old.is_empty());

        let enrichment = enrich(&catalog, &old);
        assert_eq!(enrichment.described.len(), 5);
        assert!(enrichment.unknown.is_empty());
        assert!(enrichment.bare_attributes.is_empty());
        assert!(enrichment.facts == current);
        let enriched: Blob<SimpleArchive> = enrichment.facts.clone().to_blob();
        let written: Blob<SimpleArchive> = current.clone().to_blob();
        assert_eq!(enriched.get_handle(), written.get_handle());

        // Every blob the added facts reference that the fragment carries is
        // carried by the enrichment too.
        let added = current.difference(&old);
        let referenced: BTreeSet<[u8; 32]> =
            potential_handles(&added).map(|handle| handle.raw).collect();
        let (_, _, _, mut blobs) = fragment.into_parts();
        let carried: Vec<Blob<UnknownBlob>> = blobs
            .snapshot()
            .unwrap()
            .into_iter()
            .map(|(_, blob)| blob)
            .filter(|blob| referenced.contains(&blob.get_handle().raw))
            .collect();
        assert!(!carried.is_empty());
        assert!(blob_handles(&carried).is_subset(&blob_handles(&enrichment.blobs)));

        // And the enriched archive is already current.
        assert!(enrich(&catalog, &current).unchanged(&current));
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        path: PathBuf,
        key: PathBuf,
        signer: SigningKey,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.pile");
        let key = directory.path().join("test.key");
        File::create(&path).unwrap();
        let signer = initialize_signer(&path, Some(&key)).unwrap();
        Fixture {
            _directory: directory,
            path,
            key,
            signer,
        }
    }

    fn other_key() -> SigningKey {
        SigningKey::from_bytes(&[0x5A; 32])
    }

    fn open(fixture: &Fixture) -> (Pile, Collection<SimpleArchive>) {
        let mut store = open_pile_strict(&fixture.path).unwrap();
        let root = fixture.signer.verifying_key();
        let policy =
            CollectionPolicy::new(AdmissionPolicy::direct(root), AdmissionPolicy::direct(root));
        let collection = store.collection("legacy", policy).unwrap();
        (store, collection)
    }

    /// Commit `fragment` as a pre-`ee27c4cb` build did, plus `extra`
    /// metadata facts. Returns the commit and its metadata facts.
    fn old_commit(
        store: &mut Pile,
        collection: CollectionHandle,
        signer: &SigningKey,
        fragment: Fragment,
        extra: TribleSet,
    ) -> (CollectionCommit, TribleSet) {
        let (_, facts, metafacts, mut blobs) = fragment.into_parts();
        for (_, blob) in blobs.snapshot().unwrap() {
            store.put::<UnknownBlob, _>(blob).unwrap();
        }
        let mut old = pre_ee27c4cb(&metafacts);
        old.union(extra);
        let data = store.put::<SimpleArchive, _>(facts).unwrap();
        let metadata = store.put::<SimpleArchive, _>(old.clone()).unwrap();
        let commit = CollectionCommit::sign(signer, collection, Handle::to_hash(data), metadata);
        store.insert(CollectionRecord::Commit(commit)).unwrap();
        (commit, old)
    }

    fn commits(fixture: &Fixture, collection: CollectionHandle) -> Vec<CollectionCommit> {
        let mut store = open_pile_strict(&fixture.path).unwrap();
        let snapshot = store.snapshot().unwrap();
        let commits = snapshot
            .select_records(&BTreeSet::from([CollectionRecordSelector::Collection(
                collection,
            )]))
            .unwrap()
            .into_iter()
            .filter_map(|record| match record {
                CollectionRecord::Commit(commit) => Some(commit),
                _ => None,
            })
            .collect();
        drop(snapshot);
        store.close().unwrap();
        commits
    }

    fn length(fixture: &Fixture) -> u64 {
        fs::metadata(&fixture.path).unwrap().len()
    }

    #[test]
    fn dry_run_writes_nothing_apply_enriches_and_a_second_apply_writes_nothing() {
        let fixture = fixture();
        let catalog = Catalog::frozen();
        let (mut store, collection) = open(&fixture);
        let handle = collection.handle();
        let (old, old_metadata) = old_commit(
            &mut store,
            handle,
            &fixture.signer,
            sample_fragment("old goal"),
            TribleSet::new(),
        );
        store.close().unwrap();
        let plan = [handle];

        let before = length(&fixture);
        let dry = run_path(&fixture.path, &fixture.key, &catalog, &plan, false).unwrap();
        assert_eq!(length(&fixture), before, "a dry run wrote");
        let [report] = &dry.collections[..] else {
            panic!("one collection")
        };
        assert_eq!(report.signer_commits, 1);
        assert_eq!(report.archives_enriched, 1);
        assert_eq!(report.new_commits, 1);
        assert!(report.new_blobs >= 1);
        assert!(!dry.applied);

        let first = run_path(&fixture.path, &fixture.key, &catalog, &plan, true).unwrap();
        assert_eq!(
            first.collections, dry.collections,
            "apply differs from its plan"
        );
        assert_eq!(length(&fixture) - before, first.new_bytes());

        let after = commits(&fixture, handle);
        assert_eq!(after.len(), 2);
        let new = after.iter().find(|commit| **commit != old).unwrap();
        assert_eq!(new.collection(), handle);
        assert_eq!(new.data(), old.data());
        assert_eq!(new.public_key(), old.public_key());
        new.verify_strict().unwrap();
        let mut store = open_pile_strict(&fixture.path).unwrap();
        let snapshot = store.snapshot().unwrap();
        let enriched: TribleSet = snapshot.get(new.metadata()).unwrap();
        assert!(
            old_metadata.difference(&enriched).is_empty(),
            "old facts dropped"
        );
        let current = sample_fragment("old goal").metafacts().clone();
        assert!(
            enriched == current,
            "M' is not what the current macro writes"
        );
        for handle in potential_handles(&enriched) {
            if catalog.entries().iter().any(|entry| {
                entry
                    .blobs
                    .iter()
                    .any(|blob| blob.get_handle().raw == handle.raw)
            }) {
                assert!(snapshot.metadata(handle).unwrap().is_some());
            }
        }
        drop(snapshot);
        store.close().unwrap();

        let settled = length(&fixture);
        let second = run_path(&fixture.path, &fixture.key, &catalog, &plan, true).unwrap();
        assert_eq!(second.new_commits(), 0);
        assert_eq!(second.new_blobs(), 0);
        assert_eq!(second.new_bytes(), 0);
        assert_eq!(length(&fixture), settled, "a second apply wrote");
        let [report] = &second.collections[..] else {
            panic!("one collection")
        };
        assert_eq!(report.signer_commits, 2);
        assert_eq!(report.present_commits, 1);
        assert_eq!(report.archives_current, 1);
        assert_eq!(report.archives_enriched, 1);
    }

    #[test]
    fn a_commit_that_already_carries_descriptions_produces_nothing() {
        let fixture = fixture();
        let catalog = Catalog::frozen();
        let (mut store, collection) = open(&fixture);
        store
            .commit(collection, &fixture.signer, sample_fragment("modern goal"))
            .unwrap();
        store.close().unwrap();
        let plan = [collection.handle()];

        let before = length(&fixture);
        let report = run_path(&fixture.path, &fixture.key, &catalog, &plan, true).unwrap();
        let [collection_report] = &report.collections[..] else {
            panic!("one collection")
        };
        assert_eq!(collection_report.signer_commits, 1);
        assert_eq!(collection_report.archives_current, 1);
        assert_eq!(collection_report.archives_enriched, 0);
        assert_eq!(collection_report.enriched_commits, 0);
        assert_eq!(report.new_commits(), 0);
        assert_eq!(report.new_blobs(), 0);
        assert_eq!(length(&fixture), before);
        assert_eq!(commits(&fixture, collection.handle()).len(), 1);
    }

    #[test]
    fn commits_signed_by_another_key_are_left_alone() {
        let fixture = fixture();
        let catalog = Catalog::frozen();
        let (mut store, collection) = open(&fixture);
        let handle = collection.handle();
        old_commit(
            &mut store,
            handle,
            &other_key(),
            sample_fragment("someone else's goal"),
            TribleSet::new(),
        );
        store.close().unwrap();

        let before = length(&fixture);
        let report = run_path(&fixture.path, &fixture.key, &catalog, &[handle], true).unwrap();
        let [collection_report] = &report.collections[..] else {
            panic!("one collection")
        };
        assert_eq!(collection_report.commits, 1);
        assert_eq!(collection_report.other_signers, 1);
        assert_eq!(collection_report.signer_commits, 0);
        assert_eq!(collection_report.archives, 0);
        assert_eq!(report.new_commits(), 0);
        assert_eq!(length(&fixture), before);
        assert_eq!(commits(&fixture, handle).len(), 1);
    }

    #[test]
    fn unknown_encodings_and_bare_attributes_are_counted_not_fatal() {
        let fixture = fixture();
        let catalog = Catalog::frozen();
        assert!(catalog.get(UNKNOWN_ENCODING).is_none());
        let (mut store, collection) = open(&fixture);
        let handle = collection.handle();
        let mut extra = entity! {
            ExclusiveId::force_ref(&UNKNOWN_ATTRIBUTE) @
            metadata::value_encoding: UNKNOWN_ENCODING,
            metadata::tag: metadata::KIND_ATTRIBUTE,
        }
        .facts()
        .clone();
        extra.union(
            entity! {
                ExclusiveId::force_ref(&BARE_ATTRIBUTE) @
                metadata::tag: metadata::KIND_ATTRIBUTE,
            }
            .facts()
            .clone(),
        );
        let (_, old_metadata) = old_commit(
            &mut store,
            handle,
            &fixture.signer,
            sample_fragment("goal with a stranger"),
            extra,
        );
        // A second commit naming the same archive: one archive, two commits.
        let other_data: Blob<SimpleArchive> = TribleSet::new().to_blob();
        let other_data = store.put::<SimpleArchive, _>(other_data).unwrap();
        let metadata: Blob<SimpleArchive> = old_metadata.to_blob();
        store
            .insert(CollectionRecord::Commit(CollectionCommit::sign(
                &fixture.signer,
                handle,
                Handle::to_hash(other_data),
                metadata.get_handle(),
            )))
            .unwrap();
        store.close().unwrap();

        let report = run_path(&fixture.path, &fixture.key, &catalog, &[handle], true).unwrap();
        let [collection_report] = &report.collections[..] else {
            panic!("one collection")
        };
        let unknown = &collection_report.unknown[&UNKNOWN_ENCODING];
        assert_eq!(unknown.archives, 1);
        assert_eq!(unknown.commits, 2);
        assert_eq!(unknown.attributes, BTreeSet::from([UNKNOWN_ATTRIBUTE]));
        assert_eq!(collection_report.unknown.len(), 1);
        assert_eq!(
            collection_report.bare_attributes,
            BTreeMap::from([(BARE_ATTRIBUTE, 1)])
        );
        // The known encodings are still described, for both commits.
        assert_eq!(collection_report.archives_enriched, 1);
        assert_eq!(collection_report.new_commits, 2);
        assert_eq!(commits(&fixture, handle).len(), 4);
    }

    #[test]
    fn unreadable_metadata_is_counted_and_skipped() {
        let fixture = fixture();
        let catalog = Catalog::frozen();
        let (mut store, collection) = open(&fixture);
        let handle = collection.handle();
        let data = store.put::<SimpleArchive, _>(TribleSet::new()).unwrap();
        store
            .insert(CollectionRecord::Commit(CollectionCommit::sign(
                &fixture.signer,
                handle,
                Handle::to_hash(data),
                Inline::new([0xA7; 32]),
            )))
            .unwrap();
        // Not canonical: a lone UTF-8 blob read as an archive.
        let text = store
            .put::<UTF8String, _>("not an archive".to_owned())
            .unwrap();
        store
            .insert(CollectionRecord::Commit(CollectionCommit::sign(
                &fixture.signer,
                handle,
                Handle::to_hash(data),
                text.transmute(),
            )))
            .unwrap();
        store.close().unwrap();

        let before = length(&fixture);
        let report = run_path(&fixture.path, &fixture.key, &catalog, &[handle], true).unwrap();
        let [collection_report] = &report.collections[..] else {
            panic!("one collection")
        };
        assert_eq!(collection_report.non_resident, 1);
        assert_eq!(collection_report.undecodable, 1);
        assert_eq!(collection_report.unreadable_commits, 2);
        assert_eq!(report.new_commits(), 0);
        assert_eq!(length(&fixture), before);
    }

    #[test]
    fn the_plan_reads_handles_and_the_catalog_check_refuses_a_mismatch() {
        let raw = [0xAB_u8; 32];
        let text = format!(
            "# census\n\nblake3:{} compass 27753\n{}\n",
            hex::encode(raw),
            hex::encode_upper(raw)
        );
        let plan = parse_plan(&text).unwrap();
        assert_eq!(plan, vec![Inline::new(raw)]);
        assert!(parse_plan("compass\n").is_err());
        assert!(parse_plan("# nothing\n").is_err());

        let catalog = Catalog::frozen();
        let hash = hex::encode(catalog.hash().raw);
        check_catalog(&catalog, &hash).unwrap();
        check_catalog(&catalog, &format!("blake3:{}", hash.to_uppercase())).unwrap();
        assert!(check_catalog(&catalog, &hex::encode([0_u8; 32])).is_err());
    }
}
