//! Collection-native Wiki values and open-world revision-DAG reads.
//!
//! Native revision identity is the authored artifact
//! (author, title, content, tags, supersedes). Legacy version entities remain
//! byte-for-byte present after migration; additive supersession facts connect
//! their existing ids. A collection COMMIT records curation: its signer chose
//! to publish the artifact, but is not thereby asserted to be its author. No
//! fragment anchor, alias entity, mutable head, or migration marker
//! participates in either model.
//!
//! Every read is a typed query at its point of use; nothing loads the
//! collection into a catalog first. A fact a reader does not model is ignored,
//! an incomplete or undecodable row matches no pattern, and several values
//! for one attribute are each read: [`revision_records`] returns one reading
//! per complete combination under the one id. Supersession is read as the
//! maintained latest index reads it: every target of a supersedes fact is
//! superseded, whatever asserts it, so every member of a supersedes cycle is
//! retired and its entry has no current state. Writes validate their own
//! inputs ([`revision_record`], [`tag_record`]); no read refuses a union.

pub mod cli;
pub mod mcp;
pub mod operations;

pub use operations::{Export, ImportDocument, ListOptions, Wiki};

use crate::storage::FactRead;
use std::collections::{BTreeMap, BTreeSet};

use anybytes::View;
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use triblespace::core::attestation;
use triblespace::core::collection::latest::{LatestBlob, LatestIndex};
use triblespace::core::collection::{
    CollectionCommit, CollectionRead, CollectionSnapshotExt, CollectionStoreExt,
};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::{BlobStoreGet, CapabilityProofRead, SnapshotSource};
use triblespace::prelude::*;

use crate::collection_names::open_configured;
use crate::schemas::wiki::{
    attrs, authorship_fragment, extract_link_targets, revision_fragment, TextHandle,
    DEFAULT_SCOPE_ID, KIND_AUTHORSHIP, KIND_REVISION, KIND_VERSION_ID, TAG_ARCHIVED_ID, TAG_SPECS,
};
use crate::storage::FactArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};

pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;
pub type PublicKeyValue = Inline<inlineencodings::ED25519PublicKey>;

// TODO: Shadowmodel 👻
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorshipRecord {
    pub id: Id,
    pub author: Option<Id>,
    pub authored_at: Option<IntervalValue>,
}

// TODO: Shadowmodel 👻
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionRecord {
    pub id: Id,
    pub title: TextHandle,
    pub content: TextHandle,
    pub tags: BTreeSet<Id>,
    pub supersedes: BTreeSet<Id>,
    pub author: Option<Id>,
    /// True for a revision authored after the cutover, whose id is intrinsic
    /// over its own content. False for a preserved legacy version entity,
    /// whose id predates that identity rule and is kept byte-for-byte.
    pub native: bool,
    /// Every authoring-time observation retained on a legacy identity.
    ///
    /// The deterministic legacy writer intentionally reasserted an existing
    /// content-derived version id with a fresh timestamp. Consequently this is
    /// a set, not a scalar: collapsing it would discard exact historical facts.
    pub legacy_created_at: BTreeSet<IntervalValue>,
    pub authorships: Vec<AuthorshipRecord>,
}

impl RevisionRecord {
    pub const fn is_native(&self) -> bool {
        self.native
    }

    pub fn authored_at(&self) -> Option<IntervalValue> {
        if self.is_native() {
            self.authorships
                .iter()
                .filter_map(|record| record.authored_at)
                .min_by_key(|value| value.raw)
        } else {
            // Legacy latest-version selection used the greatest observation.
            // This also preserves A -> B -> A reverts when distinct legacy
            // states are projected onto one node each in the revision DAG.
            self.legacy_created_at.last().copied()
        }
    }
}

// TODO: Shadowmodel 👻
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryRecord {
    pub roots: Vec<Id>,
    pub members: Vec<Id>,
    pub frontier: Vec<RevisionRecord>,
}

impl EntryRecord {
    /// The entry's stable name: its least root, or, when every member
    /// supersedes another member (a supersedes cycle has no root), its least
    /// member.
    pub fn label(&self) -> Id {
        *self
            .roots
            .first()
            .or_else(|| self.members.first())
            .expect("an entry has at least one member")
    }
}

/// One coherent, shard-preserving Wiki query snapshot and its maintained
/// positive latest-state relation.
pub struct WikiQuerySnapshot {
    facts: FactArchive,
    store_snapshot: PileSnapshot,
    latest: LatestIndex,
}

impl WikiQuerySnapshot {
    /// Shard-preserving facts admitted by this exact snapshot.
    pub fn facts(&self) -> &FactArchive {
        &self.facts
    }

    /// Blob reader captured while validating this exact snapshot.
    pub fn store_snapshot(&self) -> &PileSnapshot {
        &self.store_snapshot
    }

    /// Known latest states attached from the same immutable store observation.
    pub fn latest(&self) -> &LatestIndex {
        &self.latest
    }

    /// Consume the coherent snapshot into its query substrate and indexes.
    pub fn into_parts(self) -> (FactArchive, PileSnapshot, LatestIndex) {
        (self.facts, self.store_snapshot, self.latest)
    }
}
/// Maintained positive latest-state projection used for Wiki frontiers.
pub fn latest_collection<S>(
    store: &mut S,
    authority: VerifyingKey,
) -> Result<Collection<LatestBlob>>
where
    S: CollectionStoreExt + SnapshotSource,
    <S as SnapshotSource>::Snapshot: BlobStoreGet + CapabilityProofRead + CollectionRead,
{
    let source = crate::collection_names::open_configured(store, DEFAULT_SCOPE_ID, authority)?;
    latest_for_source(store, source)
}

pub(crate) fn latest_for_source<S>(
    store: &mut S,
    source: Collection<blobencodings::SimpleArchive>,
) -> Result<Collection<LatestBlob>>
where
    S: CollectionStoreExt + SnapshotSource,
    <S as SnapshotSource>::Snapshot: BlobStoreGet + CapabilityProofRead,
{
    Ok(store.attach::<LatestBlob>(source, metadata::supersedes.id())?)
}

// TODO: Shadowmodel, this should just be `entity!` calls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionDraft {
    pub title: String,
    pub content: String,
    pub tags: BTreeSet<Id>,
    pub predecessors: BTreeSet<Id>,
    pub author: Id,
    pub authored_at: IntervalValue,
}

fn id_values<P: TriblePattern>(
    space: &P,
    entity: Id,
    attribute: &Attribute<inlineencodings::GenId>,
) -> BTreeSet<Id> {
    find!(
        value: Id,
        pattern!(space, [{ entity @ attribute: ?value }])
    )
    .collect()
}

fn text_values<P: TriblePattern>(
    space: &P,
    entity: Id,
    attribute: &Attribute<inlineencodings::Handle<blobencodings::UTF8String>>,
) -> BTreeSet<TextHandle> {
    find!(
        value: TextHandle,
        pattern!(space, [{ entity @ attribute: ?value }])
    )
    .collect()
}

fn interval_values<P: TriblePattern>(
    space: &P,
    entity: Id,
    attribute: &Attribute<inlineencodings::NsTAIInterval>,
) -> BTreeSet<IntervalValue> {
    find!(
        value: IntervalValue,
        pattern!(space, [{ entity @ attribute: ?value }])
    )
    .collect()
}

fn require_point(field: &str, value: IntervalValue) -> Result<()> {
    let (lower, upper): (i128, i128) = value
        .try_from_inline()
        .map_err(|error| anyhow!("decode {field}: {error:?}"))?;
    if lower != upper {
        bail!("{field} must be a point interval");
    }
    Ok(())
}

/// Every queryable Wiki revision id.
///
/// This is a typed projection, not a catalog load: incomplete or undecodable
/// rows simply do not satisfy either revision pattern. Native revisions need
/// their author relation; preserved legacy versions do not.
pub fn revision_ids<P: TriblePattern>(space: &P) -> BTreeSet<Id> {
    let native = find!(
        id: Id,
        pattern!(space, [{
            ?id @ metadata::tag: &KIND_REVISION,
            attrs::title: _?title,
            attrs::content: _?content,
            attrs::author: _?author,
        }])
    );
    let legacy = find!(
        id: Id,
        pattern!(space, [{
            ?id @ metadata::tag: &KIND_VERSION_ID,
            attrs::title: _?title,
            attrs::content: _?content,
        }])
    );
    native.chain(legacy).collect()
}

fn optional_values<T: Copy + Ord>(values: BTreeSet<T>) -> Vec<Option<T>> {
    if values.is_empty() {
        vec![None]
    } else {
        values.into_iter().map(Some).collect()
    }
}

fn query_authorships<P: TriblePattern>(space: &P, revision: Id) -> Vec<AuthorshipRecord> {
    let ids: BTreeSet<Id> = find!(
        id: Id,
        pattern!(space, [{
            ?id @ metadata::tag: &KIND_AUTHORSHIP,
            attrs::revision: &revision,
        }])
    )
    .collect();
    let mut records = Vec::new();
    for id in ids {
        let authors = optional_values(id_values(space, id, &attrs::author));
        let times = optional_values(interval_values(space, id, &metadata::created_at));
        for author in &authors {
            for authored_at in &times {
                records.push(AuthorshipRecord {
                    id,
                    author: *author,
                    authored_at: *authored_at,
                });
            }
        }
    }
    records.sort_by_key(|record| (record.id, record.author, record.authored_at));
    records
}

/// Project every typed interpretation of one Wiki revision directly from a
/// query substrate.
///
/// Scalar multiplicity is kept visible as multiple projections rather than
/// rejected collection-wide or settled by iterator order. Set-valued tags,
/// predecessors, legacy timestamps, and authorship observations remain sets.
pub fn revision_records<P: TriblePattern>(space: &P, id: Id) -> Vec<RevisionRecord> {
    let title_content: BTreeSet<(TextHandle, TextHandle)> = find!(
        (title: TextHandle, content: TextHandle),
        pattern!(space, [{ id @ attrs::title: ?title, attrs::content: ?content }])
    )
    .collect();
    if title_content.is_empty() {
        return Vec::new();
    }

    let mut tags = id_values(space, id, &metadata::tag);
    tags.remove(&KIND_REVISION);
    tags.remove(&KIND_VERSION_ID);
    let supersedes = id_values(space, id, &metadata::supersedes);
    let authorships = query_authorships(space, id);
    let legacy_created_at = interval_values(space, id, &metadata::created_at);
    let authors = id_values(space, id, &attrs::author);
    let native = exists!(
        (),
        pattern!(space, [{ id @ metadata::tag: &KIND_REVISION }])
    );
    let legacy = exists!(
        (),
        pattern!(space, [{ id @ metadata::tag: &KIND_VERSION_ID }])
    );
    let mut records = Vec::new();

    if native {
        for (title, content) in &title_content {
            for author in &authors {
                records.push(RevisionRecord {
                    id,
                    title: *title,
                    content: *content,
                    tags: tags.clone(),
                    supersedes: supersedes.clone(),
                    author: Some(*author),
                    native: true,
                    legacy_created_at: BTreeSet::new(),
                    authorships: authorships.clone(),
                });
            }
        }
    }
    if legacy {
        for (title, content) in &title_content {
            for author in optional_values(authors.clone()) {
                records.push(RevisionRecord {
                    id,
                    title: *title,
                    content: *content,
                    tags: tags.clone(),
                    supersedes: supersedes.clone(),
                    author,
                    native: false,
                    legacy_created_at: legacy_created_at.clone(),
                    authorships: authorships.clone(),
                });
            }
        }
    }
    records
}

/// True when `id` matches either typed revision pattern [`revision_ids`] uses.
fn is_revision<P: TriblePattern>(space: &P, id: Id) -> bool {
    exists!(
        (),
        pattern!(space, [{
            id @ metadata::tag: &KIND_REVISION,
            attrs::title: _?title,
            attrs::content: _?content,
            attrs::author: _?author,
        }])
    ) || exists!(
        (),
        pattern!(space, [{
            id @ metadata::tag: &KIND_VERSION_ID,
            attrs::title: _?title,
            attrs::content: _?content,
        }])
    )
}

fn adjacent_revisions<P: TriblePattern>(space: &P, revision: Id) -> BTreeSet<Id> {
    let predecessors = find!(
        id: Id,
        pattern!(space, [{ revision @ metadata::supersedes: ?id }])
    );
    let successors = find!(
        id: Id,
        pattern!(space, [{ ?id @ metadata::supersedes: &revision }])
    );
    predecessors
        .chain(successors)
        .filter(|id| is_revision(space, *id))
        .collect()
}

/// Every revision connected to `seed` through supersedes facts in either
/// direction, `seed` included; None when `seed` is not a revision. A
/// supersedes fact naming something that is not a revision connects nothing.
pub fn entry_members<P: TriblePattern>(space: &P, seed: Id) -> Option<BTreeSet<Id>> {
    if !is_revision(space, seed) {
        return None;
    }
    let mut members = BTreeSet::from([seed]);
    let mut pending = vec![seed];
    while let Some(current) = pending.pop() {
        for adjacent in adjacent_revisions(space, current) {
            if members.insert(adjacent) {
                pending.push(adjacent);
            }
        }
    }
    Some(members)
}

/// The connected revision entry containing `seed`, projected at the point of
/// use. Only the supersession relation is traversed; no whole-Wiki catalog is
/// constructed.
///
/// Roots are the members superseding no other member; a revision whose
/// predecessor is absent is therefore a root. The frontier is the members the
/// latest index holds: empty when every member is superseded (a cycle, or a
/// supersedes claim from outside the Wiki), or when a detached index has not
/// yet seen the entry.
pub fn entry<P: TriblePattern>(space: &P, latest: &LatestIndex, seed: Id) -> Option<EntryRecord> {
    let members = entry_members(space, seed)?;
    let roots = members
        .iter()
        .copied()
        .filter(|id| id_values(space, *id, &metadata::supersedes).is_disjoint(&members))
        .collect();
    let members: Vec<Id> = members.into_iter().collect();
    let frontier =
        find!(id: Id, and!(latest.has(id), SortedSlice::new_unchecked(&members).has(id)))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .flat_map(|id| revision_records(space, id))
            .collect();
    Some(EntryRecord {
        roots,
        members,
        frontier,
    })
}

/// Every connected Wiki entry, discovered from typed revision rows and
/// projected independently. This is intended only for commands whose answer
/// genuinely ranges over the whole Wiki (list, search, audits, export).
pub fn entries<P: TriblePattern>(space: &P, latest: &LatestIndex) -> Vec<EntryRecord> {
    let mut remaining = revision_ids(space);
    let mut entries = Vec::new();
    while let Some(seed) = remaining.pop_first() {
        if let Some(entry) = entry(space, latest, seed) {
            for member in &entry.members {
                remaining.remove(member);
            }
            entries.push(entry);
        }
    }
    entries.sort_by_key(EntryRecord::label);
    entries
}

/// Causal history for one already-scoped entry, dependencies first.
///
/// A revision is ready once every predecessor it names inside the entry has
/// been emitted; ready revisions are ordered by earliest authored time, then
/// id. A supersedes cycle leaves no member ready, and the reading breaks it at
/// its earliest remaining member by the same order, so history names every
/// member exactly once.
pub fn entry_history<P: TriblePattern>(space: &P, entry: &EntryRecord) -> Vec<RevisionRecord> {
    let members: BTreeSet<Id> = entry.members.iter().copied().collect();
    let mut emitted = BTreeSet::new();
    let mut output = Vec::new();
    while emitted.len() < members.len() {
        let mut ready: Vec<Id> = members
            .iter()
            .copied()
            .filter(|id| !emitted.contains(id))
            .filter(|id| {
                id_values(space, *id, &metadata::supersedes)
                    .intersection(&members)
                    .all(|predecessor| emitted.contains(predecessor))
            })
            .collect();
        let order = |id: &Id| {
            let authored_at = revision_records(space, *id)
                .into_iter()
                .filter_map(|record| record.authored_at().map(|value| value.raw))
                .min();
            (authored_at, *id)
        };
        if ready.is_empty() {
            let stuck = members
                .iter()
                .copied()
                .filter(|id| !emitted.contains(id))
                .min_by_key(order)
                .expect("an unemitted member remains");
            ready.push(stuck);
        }
        ready.sort_by_key(order);
        for id in ready {
            emitted.insert(id);
            output.extend(revision_records(space, id));
        }
    }
    output
}

pub fn author_record(key: &VerifyingKey) -> (Fragment, Id) {
    let key: PublicKeyValue = (*key).to_inline();
    let fragment = entity! { attestation::signed_by: key };
    let author = fragment.root().expect("author description root");
    (fragment, author)
}

pub fn revision_record(draft: RevisionDraft) -> Result<(Fragment, Id)> {
    if draft.title.trim().is_empty() {
        bail!("Wiki title must not be empty");
    }
    for reserved in [KIND_VERSION_ID, KIND_REVISION, KIND_AUTHORSHIP] {
        if draft.tags.contains(&reserved) {
            bail!("Wiki user tags cannot use reserved record kind {reserved:x}");
        }
    }
    require_point("Wiki authored-at", draft.authored_at)?;
    let fragment = revision_fragment(
        draft.author,
        &draft.title,
        &draft.content,
        &draft.tags.into_iter().collect::<Vec<_>>(),
        &draft.predecessors.into_iter().collect::<Vec<_>>(),
    );
    let revision = fragment.root().expect("revision has one intrinsic root");
    let mut output = fragment;
    output += authorship_fragment(revision, Some(draft.author), Some(draft.authored_at))
        .expect("native authorship is non-empty");
    Ok((output, revision))
}

pub fn tag_record(name: &str) -> Result<(Fragment, Id, String)> {
    let normalized = name.trim().to_lowercase();
    if normalized.is_empty() {
        bail!("Wiki tag name must not be empty");
    }
    let mut fragment = Fragment::empty();
    let handle = fragment.put::<blobencodings::UTF8String, _>(normalized.clone());
    if let Some((id, _)) = TAG_SPECS.iter().find(|(_, label)| *label == normalized) {
        fragment += entity! { ExclusiveId::force_ref(id) @ metadata::name: handle };
        Ok((fragment, *id, normalized))
    } else {
        fragment += entity! { metadata::name: handle };
        let id = fragment.root().expect("intrinsic tag root");
        Ok((fragment, id, normalized))
    }
}

pub fn read_text(reader: &impl BlobStoreGet, handle: TextHandle) -> Result<String> {
    let value: View<str> = reader.get(handle)?;
    Ok(value.to_string())
}

/// Every title, content and tag name in `facts` is readable from `reader`.
///
/// A write-boundary check over facts the caller itself authored, such as an
/// import's own seed; never a check over a collection, whose other texts are
/// read open-world by whichever reader asks for them.
pub fn require_text_payloads(reader: &impl BlobStoreGet, facts: &TribleSet) -> Result<()> {
    let texts = [attrs::title.id(), attrs::content.id(), metadata::name.id()];
    for fact in facts {
        if texts.contains(fact.a()) {
            let handle = *fact.v::<inlineencodings::Handle<blobencodings::UTF8String>>();
            let _: View<str> = reader.get(handle).with_context(|| {
                format!("read Wiki text payload {}", hex::encode_upper(handle.raw))
            })?;
        }
    }
    Ok(())
}

/// One cover-tagged maximal Wiki revision, as `orient wake` presents it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverBelief {
    /// The revision itself: a member of its entry's current frontier.
    pub revision: Id,
    pub title: String,
    pub content: String,
    /// True when this revision is its entry's ONLY current head (possibly as
    /// several title/content projections of the one id), so the
    /// frontier-following `wiki show <revision>` shows exactly this revision.
    /// On a fork it would show every head, and `wiki show --exact` names one.
    pub sole_head: bool,
    /// Exactly what that `wiki show` prints: `# <title>`, the revision id,
    /// supersedes and tags lines when present, a blank line, then the content.
    /// A revision id carrying several projections prints all of them under a
    /// `fork:` banner, so every row of that id holds the same `show`. None when
    /// something `wiki show` prints (a tag name, another head) cannot be read
    /// here; [`CoverBeliefs::unreadable`] then says so.
    pub show: Option<String>,
}

/// The cover beliefs that could be read, and what was left out.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CoverBeliefs {
    pub beliefs: Vec<CoverBelief>,
    /// One line per payload that could not be read in this observation, and
    /// what its absence leaves out. Never silently empty when something is.
    pub unreadable: Vec<String>,
}

/// A payload read for a presentation: its value, or None when `unreadable`
/// says it cannot be had here, recorded in `skipped` as `what` and why.
fn presentable<T>(
    result: Result<T>,
    unreadable: &crate::storage::Unreadable<'_>,
    skipped: &mut Vec<String>,
    what: impl FnOnce() -> String,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) => match unreadable(&error) {
            Some(reason) => {
                skipped.push(format!("{} ({reason})", what()));
                Ok(None)
            }
            None => Err(error),
        },
    }
}

/// Every cover-tagged maximal Wiki revision, ordered by title then id.
///
/// A fork is not silently arbitrated: each maximal revision that still carries
/// a tag named `cover` is returned. Concurrent untagged heads likewise do not
/// erase a tagged head. Callers therefore see the authored revision DAG's
/// actual frontier rather than a timestamp-selected legacy approximation.
///
/// A payload read that `unreadable` accepts (one that cannot be had in this
/// observation) leaves out only what depends on it, and is named in
/// [`CoverBeliefs::unreadable`]: a tag whose name is unreadable cannot be
/// recognized as `cover`, a revision whose title or content is unreadable is
/// not shown, and a belief whose `wiki show` cannot be rendered has no `show`.
/// Any other error still fails the read.
pub fn cover_fragments<P: TriblePattern>(
    reader: &PileSnapshot,
    facts: &P,
    latest: &LatestIndex,
    unreadable: &crate::storage::Unreadable<'_>,
) -> Result<CoverBeliefs> {
    let mut skipped = Vec::new();
    let frontier: Vec<RevisionRecord> = find!(revision: Id, latest.has(revision))
        .flat_map(|revision| revision_records(facts, revision))
        .collect();
    let candidate_tags: BTreeSet<Id> = frontier
        .iter()
        .flat_map(|revision| revision.tags.iter().copied())
        .collect();
    let mut cover_tags = BTreeSet::new();
    for tag in candidate_tags {
        for handle in find!(
            handle: TextHandle,
            pattern!(facts, [{ tag @ metadata::name: ?handle }])
        ) {
            let name = presentable(read_text(reader, handle), unreadable, &mut skipped, || {
                format!(
                    "tag {tag:x}: name not readable, so a revision tagged cover only through it is not shown"
                )
            })?;
            if name.is_some_and(|name| name.eq_ignore_ascii_case("cover")) {
                cover_tags.insert(tag);
            }
        }
    }
    if cover_tags.is_empty() {
        return Ok(CoverBeliefs {
            beliefs: Vec::new(),
            unreadable: skipped,
        });
    }

    let mut rows = Vec::new();
    for revision in frontier {
        if revision.tags.is_disjoint(&cover_tags) {
            continue;
        }
        let title = presentable(
            read_text(reader, revision.title),
            unreadable,
            &mut skipped,
            || {
                format!(
                    "cover revision {:x}: title not readable, not shown",
                    revision.id
                )
            },
        )?;
        let content = presentable(
            read_text(reader, revision.content),
            unreadable,
            &mut skipped,
            || {
                format!(
                    "cover revision {:x}: content not readable, not shown",
                    revision.id
                )
            },
        )?;
        if let (Some(title), Some(content)) = (title, content) {
            rows.push((title, content, revision.id));
        }
    }
    rows.sort_by(|left, right| (&left.0, left.2).cmp(&(&right.0, right.2)));
    rows.dedup();
    let mut beliefs = Vec::new();
    for (title, content, revision) in rows {
        let sole_head = entry(facts, latest, revision)
            .is_some_and(|entry| entry.frontier.iter().all(|head| head.id == revision));
        let show = presentable(
            operations::render_show(facts, latest, reader, revision, sole_head),
            unreadable,
            &mut skipped,
            || format!("cover revision {revision:x}: its wiki show cannot be rendered here"),
        )?;
        beliefs.push(CoverBelief {
            revision,
            title,
            content,
            sole_head,
            show,
        });
    }
    Ok(CoverBeliefs {
        beliefs,
        unreadable: skipped,
    })
}

// ── the frontier link model ────────────────────────────────────────────────
//
// Reproduced from admitted content: [`extract_link_targets`] is the one
// extractor, and a target id names the ENTRY that contains the revision it
// points at. Forks stay forks — an entry with two current states is evidence,
// not a row to settle by clock or iteration order — so every read below is
// per-state and the entry keeps all of them.
//
// Lifted out of the `gauge` binary on 2026-08-27 so the link audit in the
// `wiki` CLI and gauge's metrics share one model instead of growing a second
// extractor that drifts from the first.

/// Display every name asserted for a tag directly from a query substrate:
/// several names sorted and joined by " / ", exactly as stored (an
/// unnormalized name is shown as it is). An unnamed tag falls back to the
/// built-in vocabulary and then to hex, so it still prints something
/// addressable.
pub fn tag_display_name_from_facts<P: TriblePattern>(
    facts: &P,
    reader: &impl BlobStoreGet,
    id: Id,
) -> Result<String> {
    let mut names = BTreeSet::new();
    for handle in find!(
        handle: TextHandle,
        pattern!(facts, [{ id @ metadata::name: ?handle }])
    ) {
        names.insert(read_text(reader, handle)?);
    }
    Ok(if names.is_empty() {
        TAG_SPECS
            .iter()
            .find_map(|(known, label)| (*known == id).then_some((*label).to_owned()))
            .unwrap_or_else(|| format!("{id:x}"))
    } else {
        names.into_iter().collect::<Vec<_>>().join(" / ")
    })
}
// TODO: Shadowmodel
/// One current state of an entry, with its content-derived link targets.
#[derive(Clone, Debug)]
pub struct FrontierState {
    pub revision: Id,
    pub title: String,
    pub tags: BTreeSet<String>,
    pub links: Vec<Id>,
}

// TODO: Shadowmodel
/// One logical entry: a stable label, every current state, and whether any of
/// those states is still un-archived.
#[derive(Clone, Debug)]
pub struct FrontierEntry {
    pub label: Id,
    pub states: Vec<FrontierState>,
    pub active: bool,
}

impl FrontierEntry {
    /// The agreed title, or every forked title, on one line.
    pub fn title(&self) -> String {
        let titles: BTreeSet<&str> = self
            .states
            .iter()
            .map(|state| state.title.as_str())
            .collect();
        if titles.len() == 1 {
            titles.first().expect("one title").to_string()
        } else {
            format!(
                "FORK: {}",
                titles.into_iter().collect::<Vec<_>>().join(" | ")
            )
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinkResolution {
    Missing,
    Unique(usize),
    Ambiguous(Vec<usize>),
}

// TODO: Shadowmodel
/// Every entry's current states, plus the indexes that make a link target
/// resolvable: revision ids directly, legacy fragment anchors through the
/// compatibility path, and the non-revision entities a target might name.
#[derive(Clone, Debug)]
pub struct FrontierModel {
    pub entries: Vec<FrontierEntry>,
    selectors: BTreeMap<Id, BTreeSet<usize>>,
    anchors: BTreeMap<Id, BTreeSet<usize>>,
    kinds: BTreeMap<Id, OtherKind>,
}

impl FrontierModel {
    pub fn load<P: TriblePattern>(
        reader: &impl BlobStoreGet,
        facts: &P,
        latest: &LatestIndex,
    ) -> Result<Self> {
        let records = entries(facts, latest);
        let mut entries = Vec::with_capacity(records.len());
        let mut selectors: BTreeMap<Id, BTreeSet<usize>> = BTreeMap::new();

        for (index, entry) in records.iter().enumerate() {
            let label = entry.label();
            let mut states = Vec::with_capacity(entry.frontier.len());
            for revision in &entry.frontier {
                let title = read_text(reader, revision.title)?;
                let content = read_text(reader, revision.content)?;
                let tags = revision
                    .tags
                    .iter()
                    .map(|tag| tag_display_name_from_facts(facts, reader, *tag))
                    .collect::<Result<BTreeSet<_>>>()?;
                let links = extract_link_targets(&content)
                    .into_iter()
                    .filter_map(|raw| Id::from_hex(&raw))
                    .collect();
                states.push(FrontierState {
                    revision: revision.id,
                    title,
                    tags,
                    links,
                });
            }
            for revision in &entry.members {
                selectors.entry(*revision).or_default().insert(index);
            }
            let active = entry
                .frontier
                .iter()
                .any(|revision| !revision.tags.contains(&TAG_ARCHIVED_ID));
            entries.push(FrontierEntry {
                label,
                states,
                active,
            });
        }

        let anchors = Self::index_anchors(facts, &selectors);
        let mut kinds = BTreeMap::new();
        for (id, _) in TAG_SPECS {
            kinds.insert(id, OtherKind::Tag);
        }
        for id in find!(
            id: Id,
            pattern!(facts, [{ ?id @ metadata::name: _?name }])
        ) {
            kinds.insert(id, OtherKind::Tag);
        }
        for id in find!(
            id: Id,
            pattern!(facts, [{ ?id @ attestation::signed_by: _?key }])
        ) {
            kinds.insert(id, OtherKind::Author);
        }
        for id in find!(
            id: Id,
            pattern!(facts, [{ ?id @ metadata::tag: &KIND_AUTHORSHIP }])
        ) {
            kinds.insert(id, OtherKind::Authorship);
        }

        Ok(Self {
            entries,
            selectors,
            anchors,
            kinds,
        })
    }

    pub fn resolve(&self, target: Id) -> LinkResolution {
        match self.selectors.get(&target) {
            None => LinkResolution::Missing,
            Some(entries) if entries.len() == 1 => {
                LinkResolution::Unique(*entries.first().expect("one entry"))
            }
            Some(entries) => LinkResolution::Ambiguous(entries.iter().copied().collect()),
        }
    }

    pub fn state_count(&self) -> usize {
        self.active_entries().map(|entry| entry.states.len()).sum()
    }

    pub fn active_entries(&self) -> impl Iterator<Item = &FrontierEntry> {
        self.entries.iter().filter(|entry| entry.active)
    }

    pub fn active_count(&self) -> usize {
        self.active_entries().count()
    }
}

// ── what a dangling link actually means ────────────────────────────────────
//
// A wiki whose convention is to LINK LIBERALLY cannot treat every unresolved
// target as a defect: a link to a page nobody has written yet marks work, not
// breakage. Three outcomes are genuinely different and a report that merges
// them is worse than no report at all.

/// A non-revision Wiki entity a link target happens to name.
///
/// Not breakage in itself — it says the reference points at the wrong KIND of
/// thing, which is a different mistake from pointing at nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OtherKind {
    Tag,
    Author,
    Authorship,
}

impl OtherKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Author => "author",
            Self::Authorship => "authorship",
        }
    }
}

/// What a link target names, once the whole revision DAG is consulted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinkClass {
    /// Resolves to an entry that is still on the live frontier. A citation of
    /// a SUPERSEDED revision lands here too: it names exactly what its author
    /// read, and `wiki show` follows it forward by default.
    Live(usize),
    /// One id names several disconnected entries. Evidence, never breakage.
    Ambiguous(Vec<usize>),
    /// The target is a real revision, but every current state of its entry is
    /// archived — the live frontier no longer carries it. THIS is the class
    /// that means a reference went stale under someone.
    Retired(usize),
    /// Not a revision, but a legacy fragment anchor whose retained facts still
    /// name an entry. Reachable through the compatibility path only: the
    /// anchor stopped being a selector on 2026-08-18. A migration signal.
    Legacy { entries: Vec<usize>, retired: bool },
    /// No fragment at any revision, ever. A forward reference — the convention
    /// is to link liberally, so this is a TODO list, not a defect list.
    Unwritten(Option<OtherKind>),
}

impl LinkClass {
    /// True only for the one class that means something actually broke.
    pub const fn is_breakage(&self) -> bool {
        matches!(self, Self::Retired(_))
    }
}

// TODO: Shadowmodel
/// One citation, kept with the revision that made it.
#[derive(Clone, Debug)]
pub struct LinkReference {
    pub source: Id,
    pub source_entry: usize,
    pub source_title: String,
    pub target: Id,
    pub class: LinkClass,
}

// TODO: Shadowmodel
/// The whole frontier's outgoing citations, classified, plus the incoming
/// count each entry earned from that same walk.
#[derive(Clone, Debug, Default)]
pub struct LinkAudit {
    pub states: usize,
    pub total: usize,
    pub live: usize,
    pub ambiguous: Vec<LinkReference>,
    pub retired: Vec<LinkReference>,
    pub legacy: Vec<LinkReference>,
    pub unwritten: Vec<LinkReference>,
    /// Incoming resolved citations per entry index, self-citations excluded.
    pub incoming: Vec<usize>,
}

impl LinkAudit {
    pub fn breakage(&self) -> usize {
        self.retired.len()
    }
}

impl FrontierModel {
    /// Every legacy fragment anchor that still names an entry.
    ///
    /// The anchor is no longer a SELECTOR, but its facts were never removed —
    /// the store is append-only — so a reference written before the cutover can
    /// still be told apart from one that never had a target.
    fn index_anchors<P: TriblePattern>(
        facts: &P,
        selectors: &BTreeMap<Id, BTreeSet<usize>>,
    ) -> BTreeMap<Id, BTreeSet<usize>> {
        let mut anchors: BTreeMap<Id, BTreeSet<usize>> = BTreeMap::new();
        for (anchor, revision) in find!(
            (anchor: Id, revision: Id),
            pattern!(facts, [{ ?revision @ attrs::fragment: ?anchor }])
        ) {
            if let Some(entries) = selectors.get(&revision) {
                anchors
                    .entry(anchor)
                    .or_default()
                    .extend(entries.iter().copied());
            }
        }
        anchors
    }

    /// Classify one link target against the complete revision DAG.
    pub fn classify(&self, target: Id) -> LinkClass {
        match self.resolve(target) {
            LinkResolution::Unique(index) => {
                if self.entries[index].active {
                    LinkClass::Live(index)
                } else {
                    LinkClass::Retired(index)
                }
            }
            LinkResolution::Ambiguous(candidates) => LinkClass::Ambiguous(candidates),
            LinkResolution::Missing => match self.anchors.get(&target) {
                Some(entries) => LinkClass::Legacy {
                    entries: entries.iter().copied().collect(),
                    retired: !entries.iter().any(|index| self.entries[*index].active),
                },
                None => LinkClass::Unwritten(self.kinds.get(&target).copied()),
            },
        }
    }

    /// Walk every current state of every live entry once, classifying its
    /// outgoing citations and counting the incoming ones on the way.
    pub fn audit(&self) -> LinkAudit {
        let mut audit = LinkAudit {
            incoming: vec![0; self.entries.len()],
            ..LinkAudit::default()
        };
        for (index, entry) in self.entries.iter().enumerate().filter(|(_, e)| e.active) {
            for state in &entry.states {
                audit.states += 1;
                for &target in &state.links {
                    audit.total += 1;
                    let class = self.classify(target);
                    let reference = LinkReference {
                        source: state.revision,
                        source_entry: index,
                        source_title: state.title.clone(),
                        target,
                        class: class.clone(),
                    };
                    match class {
                        // A page citing itself does not make it referenced.
                        LinkClass::Live(target_entry) => {
                            audit.live += 1;
                            if target_entry != index {
                                audit.incoming[target_entry] += 1;
                            }
                        }
                        LinkClass::Ambiguous(_) => audit.ambiguous.push(reference),
                        LinkClass::Retired(_) => audit.retired.push(reference),
                        LinkClass::Legacy { .. } => audit.legacy.push(reference),
                        LinkClass::Unwritten(_) => audit.unwritten.push(reference),
                    }
                }
            }
        }
        audit
    }

    /// How many legacy fragment anchors the compatibility path can still name.
    ///
    /// Reported alongside the audit so a ZERO in the legacy class is readable:
    /// "no anchor is cited from the frontier" and "the anchor index is empty"
    /// are different findings, and a report that cannot tell them apart is a
    /// check that passes for the wrong reason.
    pub fn anchor_count(&self) -> usize {
        self.anchors.len()
    }

    /// Live entries nothing else at the frontier cites, cheapest form: the
    /// incoming counts the audit already produced.
    pub fn unreferenced(&self, audit: &LinkAudit) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| entry.active && audit.incoming[*index] == 0)
            .map(|(index, _)| index)
            .collect()
    }
}

/// The complete Wiki collection's facts, as they are, for migrations and
/// tests: nothing is checked against a closed shape, and a text that is not
/// resident is only a missing blob for whichever reader later asks for it.
/// Durable application readers use [`query_snapshot`] instead.
pub fn materialize_collection(
    pile: &mut Pile,
    signer: &SigningKey,
) -> Result<(TribleSet, PileSnapshot)> {
    let collection = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
    let store_snapshot = pile.snapshot().context("freeze Wiki store snapshot")?;
    let facts = store_snapshot
        .collection(collection)
        .context("attach Wiki collection")?
        .view::<TribleSet>()
        .context("read Wiki collection")?;
    Ok((facts, store_snapshot))
}

/// Capture one durable Wiki snapshot with shard-preserving facts and its
/// supersession index, as the maintenance worker has carried them.
///
/// A read attaches and never maintains. Positive latest membership never
/// admits states unseen by a lagging index. Normal reads therefore never
/// flatten the collection or validate a closed-world catalog before asking
/// their actual query.
pub async fn query_snapshot(pile: &mut Pile, signer: &SigningKey) -> Result<WikiQuerySnapshot> {
    let collection = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(collection, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(collection, succinct)?;
    let target = latest_collection(pile, signer.verifying_key())?;
    let store_snapshot = pile.snapshot().context("freeze resident Wiki targets")?;
    let facts = store_snapshot
        .read_facts(rank9)
        .context("read Wiki fact collection")?;
    // The index over the same foundations the facts read: revisions no
    // attachment reaches yet are built in memory.
    let latest = store_snapshot
        .attached(target)
        .map_err(|error| anyhow!("observe Wiki supersession index: {error}"))?
        .read::<LatestIndex>()
        .map_err(|error| anyhow!("read Wiki supersession index: {error}"))?
        .into_value();
    Ok(WikiQuerySnapshot {
        facts,
        store_snapshot,
        latest,
    })
}

pub fn commit_collection(
    pile: &mut Pile,
    signer: &SigningKey,
    fragment: Fragment,
) -> Result<CollectionCommit> {
    // The signature is curation of this fragment into the collection. Author
    // attribution lives inside the revision artifact and is intentionally not
    // inferred from, or forced equal to, this signer.
    let collection = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
    pile.commit(collection, signer, fragment)
        .map_err(|error| anyhow!("commit Wiki collection fragment: {error}"))
}

/// The latest index a maintained Wiki would hold over exactly `facts`.
#[cfg(test)]
pub(crate) fn latest_index_for_tests(facts: &TribleSet) -> LatestIndex {
    let observed = triblespace::core::collection::latest::derive_element(
        &facts.clone().to_blob(),
        metadata::supersedes.id(),
    )
    .unwrap();
    LatestIndex::decode(&observed).unwrap()
}

/// The worker's carry for tests: the fact chain and the supersession index.
#[cfg(test)]
pub(crate) fn carry_for_tests(pile: &mut Pile, signer: &SigningKey) {
    let collection = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
    crate::storage::carry_facts(pile, collection, signer);
    let target = latest_collection(pile, signer.verifying_key()).unwrap();
    drop(pollster::block_on(pile.maintain_attached(target, signer)).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    use hifitime::Epoch;
    use triblespace::core::blob::MemoryBlobStore;
    use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
    use triblespace::core::repo::memoryrepo::MemoryRepo;

    #[test]
    fn latest_is_attached_to_its_source_and_carries_no_policy() {
        let mut store = MemoryRepo::default();
        let authority = SigningKey::from_bytes(&[25; 32]).verifying_key();
        let policy =
            CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::delegable(authority));
        let source = store.collection("shared-wiki", policy).unwrap();

        let latest = latest_for_source(&mut store, source).unwrap();
        let snapshot = store.snapshot().unwrap();
        let descriptor: TribleSet = snapshot.get(latest.handle()).unwrap();

        assert_eq!(
            triblespace::core::collection::descriptor::parents(&descriptor).unwrap(),
            vec![source.handle()]
        );
        assert!(latest.policy(&snapshot).is_err());
    }

    fn at(seconds: f64) -> IntervalValue {
        let epoch = Epoch::from_tai_seconds(seconds);
        (epoch, epoch).try_to_inline().unwrap()
    }

    fn span(lower: f64, upper: f64) -> IntervalValue {
        (
            Epoch::from_tai_seconds(lower),
            Epoch::from_tai_seconds(upper),
        )
            .try_to_inline()
            .unwrap()
    }

    fn draft(author: Id, title: &str, predecessors: impl IntoIterator<Item = Id>) -> RevisionDraft {
        RevisionDraft {
            title: title.to_owned(),
            content: format!("{title} body"),
            tags: BTreeSet::new(),
            predecessors: predecessors.into_iter().collect(),
            author,
            authored_at: at(1.0),
        }
    }

    fn latest_index(facts: &TribleSet) -> LatestIndex {
        latest_index_for_tests(facts)
    }

    /// The pure query-layer answer the maintained index must agree with:
    /// each entry's members that no fact in `facts` supersedes.
    fn jit_frontiers(facts: &TribleSet) -> BTreeSet<BTreeSet<Id>> {
        let mut remaining = revision_ids(facts);
        let mut frontiers = BTreeSet::new();
        while let Some(seed) = remaining.pop_first() {
            let members = entry_members(facts, seed).unwrap();
            for member in &members {
                remaining.remove(member);
            }
            frontiers.insert(triblespace::core::query::frontier::latest(
                facts,
                metadata::supersedes.id(),
                members,
            ));
        }
        frontiers
    }

    #[test]
    fn native_identity_includes_author_and_supports_merge() {
        let signer = SigningKey::from_bytes(&[7; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (left_fragment, left) = revision_record(draft(author, "left", [])).unwrap();
        let (right_fragment, right) = revision_record(draft(author, "right", [])).unwrap();
        let (join_fragment, join) =
            revision_record(draft(author, "joined", [left, right])).unwrap();
        let mut facts = TribleSet::new();
        facts += author_fragment;
        facts += left_fragment;
        facts += right_fragment;
        facts += join_fragment;

        let entries = entries(&facts, &latest_index(&facts));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].frontier[0].id, join);
        assert!(find!(
            authored_at: IntervalValue,
            pattern!(&facts, [{
                _?authorship @
                metadata::tag: &KIND_AUTHORSHIP,
                attrs::revision: &join,
                attrs::author: &author,
                metadata::created_at: ?authored_at,
            }])
        )
        .next()
        .is_some());

        let other = SigningKey::from_bytes(&[8; 32]);
        let (_, other_author) = author_record(&other.verifying_key());
        assert_ne!(
            revision_fragment(author, "same", "body", &[], &[]).root(),
            revision_fragment(other_author, "same", "body", &[], &[]).root()
        );
    }

    #[test]
    fn fork_remains_visible_and_history_is_causal() {
        let signer = SigningKey::from_bytes(&[9; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (root_fragment, root) = revision_record(draft(author, "root", [])).unwrap();
        let (left_fragment, left) = revision_record(draft(author, "left", [root])).unwrap();
        let (right_fragment, right) = revision_record(draft(author, "right", [root])).unwrap();
        let mut facts = TribleSet::new();
        facts += author_fragment;
        facts += right_fragment;
        facts += root_fragment;
        facts += left_fragment;
        let entry = &entries(&facts, &latest_index(&facts))[0];
        let frontier: BTreeSet<Id> = entry.frontier.iter().map(|record| record.id).collect();
        assert_eq!(frontier, BTreeSet::from([left, right]));
        let history = entry_history(&facts, entry);
        assert_eq!(history.first().map(|record| record.id), Some(root));
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn maintained_latest_matches_jit_across_fork_and_merge() {
        let signer = SigningKey::from_bytes(&[31; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (root_fragment, root) = revision_record(draft(author, "root", [])).unwrap();
        let (left_fragment, left) = revision_record(draft(author, "left", [root])).unwrap();
        let (right_fragment, right) = revision_record(draft(author, "right", [root])).unwrap();
        let (merge_fragment, merge) =
            revision_record(draft(author, "merge", [left, right])).unwrap();

        let mut fork = author_fragment + root_fragment + left_fragment + right_fragment;
        let fork_index = latest_index(fork.facts());
        assert_eq!(
            entries(fork.facts(), &fork_index)
                .iter()
                .map(|entry| entry.frontier.iter().map(|record| record.id).collect())
                .collect::<BTreeSet<BTreeSet<Id>>>(),
            jit_frontiers(fork.facts())
        );
        assert_eq!(
            find!(id: Id, and!(fork_index.has(id),
                pattern!(fork.facts(), [{ ?id @ metadata::tag: &KIND_REVISION }]))
            )
            .collect::<BTreeSet<_>>(),
            BTreeSet::from([left, right])
        );

        fork += merge_fragment;
        let merge_index = latest_index(fork.facts());
        assert_eq!(
            entries(fork.facts(), &merge_index)
                .iter()
                .map(|entry| entry.frontier.iter().map(|record| record.id).collect())
                .collect::<BTreeSet<BTreeSet<Id>>>(),
            jit_frontiers(fork.facts())
        );
        assert_eq!(
            find!(id: Id, and!(merge_index.has(id),
                pattern!(fork.facts(), [{ ?id @ metadata::tag: &KIND_REVISION }]))
            )
            .collect::<BTreeSet<_>>(),
            BTreeSet::from([merge])
        );
    }

    #[test]
    fn facts_ahead_of_latest_stay_unseen_until_maintenance() {
        pollster::block_on(async {
            let signer = SigningKey::from_bytes(&[34; 32]);
            let (author_fragment, author) = author_record(&signer.verifying_key());
            let (root_fragment, root) = revision_record(draft(author, "root", [])).unwrap();
            let (next_fragment, next) = revision_record(draft(author, "next", [root])).unwrap();
            let (new_fragment, new) = revision_record(draft(author, "new entry", [])).unwrap();
            let mut store = MemoryRepo::for_host(signer.verifying_key());
            let source = store
                .collection(
                    "latest-lag",
                    crate::collection_names::private_policy(signer.verifying_key()),
                )
                .unwrap();
            let target = store
                .attach::<LatestBlob>(source, metadata::supersedes.id())
                .unwrap();
            store
                .commit(source, &signer, author_fragment + root_fragment)
                .unwrap();
            let ready = store.maintain_attached(target, &signer).await.unwrap();
            let lagging = ready
                .attached(target)
                .unwrap()
                .view::<LatestIndex>()
                .unwrap();
            store
                .commit(source, &signer, next_fragment + new_fragment)
                .unwrap();
            let snapshot = store.snapshot().unwrap();
            let facts = snapshot
                .collection(source)
                .unwrap()
                .view::<TribleSet>()
                .unwrap();
            let current = snapshot.attached(target).unwrap();
            assert_eq!(current.residual().len(), 1);
            let current = current.view::<LatestIndex>().unwrap();
            assert_eq!(
                entry(&facts, &current, root)
                    .unwrap()
                    .frontier
                    .iter()
                    .map(|r| r.id)
                    .collect::<Vec<_>>(),
                vec![root],
            );
            assert!(entry(&facts, &current, new).unwrap().frontier.is_empty());
            assert!(!exists!(
                (state: Id),
                and!(current.has(state), state.is(next.to_inline()))
            ));

            let ready = store.maintain_attached(target, &signer).await.unwrap();
            let advanced = ready
                .attached(target)
                .unwrap()
                .view::<LatestIndex>()
                .unwrap();
            assert_eq!(
                entry(&facts, &advanced, root)
                    .unwrap()
                    .frontier
                    .iter()
                    .map(|r| r.id)
                    .collect::<Vec<_>>(),
                vec![next],
            );
            assert_eq!(entry(&facts, &advanced, new).unwrap().frontier[0].id, new);
            assert_eq!(
                entry(&facts, &lagging, root).unwrap().frontier[0].id,
                root,
                "maintenance must not mutate a frozen latest view"
            );
        });
    }

    #[test]
    fn latest_projection_converges_when_successors_arrive_before_ancestors() {
        use triblespace::core::collection::latest::{derive_element, empty, join};

        let author = genid().id;
        let (root_fragment, root) = revision_record(draft(author, "root", [])).unwrap();
        let (middle_fragment, middle) = revision_record(draft(author, "middle", [root])).unwrap();
        let (tip_fragment, tip) = revision_record(draft(author, "tip", [middle])).unwrap();
        let (fork_fragment, fork) = revision_record(draft(author, "fork", [root])).unwrap();
        let fragments = [root_fragment, middle_fragment, tip_fragment, fork_fragment];
        let mut complete = TribleSet::new();
        for fragment in &fragments {
            complete += fragment.facts().clone();
        }
        let expected =
            derive_element(&complete.clone().to_blob(), metadata::supersedes.id()).unwrap();

        for arrival in [[2, 1, 3, 0], [0, 1, 2, 3], [3, 0, 2, 1]] {
            let mut joined = empty();
            for position in arrival {
                let delta = derive_element(
                    &fragments[position].facts().clone().to_blob(),
                    metadata::supersedes.id(),
                )
                .unwrap();
                joined = join(&joined, &delta).unwrap();
            }
            assert_eq!(joined.bytes.as_ref(), expected.bytes.as_ref());
            let latest = LatestIndex::decode(&joined).unwrap();
            assert_eq!(
                entry(&complete, &latest, root)
                    .unwrap()
                    .frontier
                    .iter()
                    .map(|r| r.id)
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([tip, fork]),
            );
        }
    }

    #[test]
    fn exact_snapshot_index_does_not_advance_authority() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wiki.pile");
        File::create(&path).unwrap();
        let signer = SigningKey::from_bytes(&[32; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (root_fragment, root) = revision_record(draft(author, "root", [])).unwrap();

        let mut pile = crate::storage::open_pile_strict_as(&path, signer.verifying_key()).unwrap();
        commit_collection(&mut pile, &signer, author_fragment + root_fragment).unwrap();
        let collection =
            open_configured(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        let store_snapshot = pile.snapshot().unwrap();
        let cover_before = collection.admitted(&store_snapshot).unwrap();
        carry_for_tests(&mut pile, &signer);
        let store_snapshot = pile.snapshot().unwrap();
        let carried = collection.admitted(&store_snapshot).unwrap();
        assert_eq!(carried, cover_before);
        let snapshot = pollster::block_on(query_snapshot(&mut pile, &signer)).unwrap();
        let store_snapshot = pile.snapshot().unwrap();
        let cover_after_index = collection.admitted(&store_snapshot).unwrap();
        assert_eq!(cover_after_index, cover_before);
        assert!(exists!(
            (id: Id),
            and!(snapshot.latest().has(id), id.is(root.to_inline()))
        ));

        pile.close().unwrap();
    }

    #[test]
    fn cover_projection_keeps_each_tagged_frontier_head() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wiki.pile");
        File::create(&path).unwrap();
        let signer = SigningKey::from_bytes(&[15; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (cover_fragment, cover, _) = tag_record("cover").unwrap();
        let (root_fragment, root) = revision_record(draft(author, "root", [])).unwrap();

        let mut left = draft(author, "left", [root]);
        left.tags.insert(cover);
        let (left_fragment, left_id) = revision_record(left).unwrap();
        let mut right = draft(author, "right", [root]);
        right.tags.insert(cover);
        let (right_fragment, right_id) = revision_record(right).unwrap();
        let (untagged_fragment, _) = revision_record(draft(author, "untagged", [root])).unwrap();

        let mut fragment = author_fragment + cover_fragment;
        fragment += root_fragment;
        fragment += left_fragment;
        fragment += right_fragment;
        fragment += untagged_fragment;

        let mut pile = crate::storage::open_pile_strict_as(&path, signer.verifying_key()).unwrap();
        let collection =
            crate::collection_names::open(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key())
                .unwrap();
        pile.commit(collection, &signer, fragment).unwrap();
        let (facts, reader) = materialize_collection(&mut pile, &signer).unwrap();
        let order = latest_index(&facts);
        let CoverBeliefs {
            beliefs,
            unreadable,
        } = cover_fragments(&reader, &facts, &order, &|_| None).unwrap();
        assert!(unreadable.is_empty());
        assert_eq!(
            beliefs
                .iter()
                .map(|belief| (belief.title.as_str(), belief.content.as_str()))
                .collect::<Vec<_>>(),
            vec![("left", "left body"), ("right", "right body")]
        );
        assert_eq!(
            beliefs
                .iter()
                .map(|belief| belief.revision)
                .collect::<Vec<_>>(),
            vec![left_id, right_id]
        );
        // Both heads of one entry: a frontier-following export would refuse.
        assert!(beliefs.iter().all(|belief| !belief.sole_head));
        pile.close().unwrap();
    }

    /// A tag name or a cover revision whose bytes never arrived is skipped
    /// and named; the beliefs that can be read are still returned.
    #[test]
    fn cover_projection_skips_and_reports_what_cannot_be_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wiki.pile");
        File::create(&path).unwrap();
        let signer = SigningKey::from_bytes(&[16; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (cover_fragment, cover, _) = tag_record("cover").unwrap();
        let without_blobs = |fragment: Fragment| {
            let (_, facts, metafacts, _) = fragment.into_parts();
            Fragment::from_parts(facts, metafacts, MemoryBlobStore::new())
        };
        // A tag whose name blob is not here, on an untagged-for-cover head
        // and on a cover head.
        let (lost_tag_fragment, lost_tag, _) = tag_record("elsewhere").unwrap();
        let mut other = draft(author, "other", []);
        other.tags.insert(lost_tag);
        let (other_fragment, _) = revision_record(other).unwrap();
        let mut annotated = draft(author, "annotated", []);
        annotated.tags.extend([cover, lost_tag]);
        let (annotated_fragment, annotated_id) = revision_record(annotated).unwrap();
        // A cover head whose own title and content are not here.
        let mut unread = draft(author, "unread", []);
        unread.tags.insert(cover);
        let (unread_fragment, unread_id) = revision_record(unread).unwrap();
        let mut plain = draft(author, "plain", []);
        plain.tags.insert(cover);
        let (plain_fragment, plain_id) = revision_record(plain).unwrap();

        let mut fragment = author_fragment + cover_fragment;
        fragment += without_blobs(lost_tag_fragment);
        fragment += other_fragment;
        fragment += annotated_fragment;
        fragment += without_blobs(unread_fragment);
        fragment += plain_fragment;

        let mut pile = crate::storage::open_pile_strict_as(&path, signer.verifying_key()).unwrap();
        let collection =
            crate::collection_names::open(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key())
                .unwrap();
        pile.commit(collection, &signer, fragment).unwrap();
        // The facts as they are; their missing blobs matter only to the
        // cover read below.
        let reader = pile.snapshot().unwrap();
        let facts = reader
            .collection(collection)
            .unwrap()
            .view::<TribleSet>()
            .unwrap();
        let order = latest_index(&facts);
        // A strict read still refuses, and so never reports less than it saw.
        assert!(cover_fragments(&reader, &facts, &order, &|_| None).is_err());
        let CoverBeliefs {
            beliefs,
            unreadable,
        } = cover_fragments(&reader, &facts, &order, &|error| Some(format!("{error:#}"))).unwrap();
        assert_eq!(
            beliefs
                .iter()
                .map(|belief| (belief.title.as_str(), belief.revision))
                .collect::<Vec<_>>(),
            vec![("annotated", annotated_id), ("plain", plain_id)]
        );
        // The annotated belief's own text is here; what `wiki show` would
        // print for it (the unreadable tag name) is not, and that is said.
        assert!(beliefs[0].show.is_none());
        assert!(beliefs[1].show.as_deref().unwrap().contains("# plain"));
        let lost_tag = format!("{lost_tag:x}");
        let unread = format!("{unread_id:x}");
        let annotated = format!("{annotated_id:x}");
        assert!(unreadable
            .iter()
            .any(|line| line.starts_with(&format!("tag {lost_tag}"))));
        assert!(unreadable
            .iter()
            .any(|line| line.contains(&unread) && line.contains("title")));
        assert!(unreadable
            .iter()
            .any(|line| line.contains(&annotated) && line.contains("wiki show")));
        pile.close().unwrap();
    }

    #[test]
    fn curator_can_publish_another_authors_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wiki.pile");
        File::create(&path).unwrap();

        let author_key = SigningKey::from_bytes(&[13; 32]);
        let curator_key = SigningKey::from_bytes(&[14; 32]);
        let (author_fragment, author) = author_record(&author_key.verifying_key());
        let (revision_fragment, revision) = revision_record(draft(author, "shared", [])).unwrap();

        let mut pile =
            crate::storage::open_pile_strict_as(&path, curator_key.verifying_key()).unwrap();
        commit_collection(&mut pile, &curator_key, author_fragment + revision_fragment).unwrap();
        let (facts, _) = materialize_collection(&mut pile, &curator_key).unwrap();
        let authors: Vec<Option<Id>> = revision_records(&facts, revision)
            .iter()
            .map(|record| record.author)
            .collect();
        assert_eq!(authors, vec![Some(author)]);
        pile.close().unwrap();
    }

    /// The legacy anchor is nothing at all now — not an edge, not a selector.
    ///
    /// Its facts are still in the store, because the store is append-only, and
    /// the fixture keeps them for exactly that reason. Two versions carrying
    /// the same anchor and NO `supersedes` between them are two entries, and
    /// the anchor id itself resolves to no revision: lineage is read from the
    /// supersedes facts the migration wrote down, and from nothing else.
    #[test]
    fn a_shared_legacy_anchor_groups_nothing_and_names_nothing() {
        let fragment = genid().id;
        let first = genid().id;
        let second = genid().id;
        let mut facts = entity! { ExclusiveId::force_ref(&first) @
            metadata::tag: &KIND_VERSION_ID,
            attrs::fragment: fragment,
            attrs::title: "T".to_blob().get_handle(),
            attrs::content: "one".to_blob().get_handle(),
            metadata::created_at: at(1.0),
        }
        .into_facts();
        facts += entity! { ExclusiveId::force_ref(&second) @
            metadata::tag: &KIND_VERSION_ID,
            attrs::fragment: fragment,
            attrs::title: "T".to_blob().get_handle(),
            attrs::content: "two".to_blob().get_handle(),
            metadata::created_at: at(2.0),
        }
        .into_facts();
        let latest = latest_index(&facts);
        assert_eq!(entries(&facts, &latest).len(), 2);
        assert_ne!(
            entry(&facts, &latest, first).unwrap().members,
            entry(&facts, &latest, second).unwrap().members
        );
        assert!(
            revision_records(&facts, fragment).is_empty(),
            "an anchor id must resolve to no revision"
        );
        assert!(
            entry(&facts, &latest, fragment).is_none(),
            "an anchor id must belong to no entry"
        );
    }

    #[test]
    fn legacy_ids_and_facts_are_read_without_aliases() {
        let fragment = genid().id;
        let first = genid().id;
        let second = genid().id;
        let mut facts = entity! { ExclusiveId::force_ref(&first) @
            metadata::tag: &KIND_VERSION_ID,
            attrs::fragment: fragment,
            attrs::title: "T".to_blob().get_handle(),
            attrs::content: "one".to_blob().get_handle(),
            metadata::created_at: at(1.0),
        }
        .into_facts();
        facts += entity! { ExclusiveId::force_ref(&second) @
            metadata::tag: &KIND_VERSION_ID,
            attrs::fragment: fragment,
            attrs::title: "T".to_blob().get_handle(),
            attrs::content: "two".to_blob().get_handle(),
            metadata::created_at: at(2.0),
            metadata::supersedes: first,
        }
        .into_facts();
        let latest = latest_index(&facts);
        // Both legacy ids keep their identity and their lineage...
        assert_eq!(revision_records(&facts, first).len(), 1);
        assert_eq!(
            revision_records(&facts, second)[0]
                .supersedes
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![first]
        );
        assert!(!revision_records(&facts, first)[0].is_native());
        let entry = entry(&facts, &latest, second).unwrap();
        assert_eq!(
            entry
                .frontier
                .iter()
                .map(|head| head.id)
                .collect::<Vec<_>>(),
            vec![second]
        );
        // ...and the anchor they were written under names nothing.
        assert!(revision_records(&facts, fragment).is_empty());
    }

    #[test]
    fn legacy_reassertions_preserve_all_times_and_project_the_latest() {
        let fragment = genid().id;
        let version = genid().id;
        let observed = [at(1.0), at(3.0)];
        let facts = entity! { ExclusiveId::force_ref(&version) @
            metadata::tag: &KIND_VERSION_ID,
            attrs::fragment: fragment,
            attrs::title: "T".to_blob().get_handle(),
            attrs::content: "same state".to_blob().get_handle(),
            metadata::created_at*: observed.iter(),
        }
        .into_facts();

        let records = revision_records(&facts, version);
        let record = &records[0];
        assert_eq!(
            record.legacy_created_at,
            BTreeSet::from(observed),
            "no historical timestamp observation is discarded"
        );
        assert_eq!(
            record.authored_at(),
            Some(observed[1]),
            "legacy current-state semantics use the latest reassertion"
        );
    }

    /// THE NEGATIVE CONTROL. A checker that reports zero problems on a real
    /// corpus proves nothing unless a known-broken reference makes it fire and
    /// a known-good one does not, so this fixture contains one of each and
    /// pins every class between them.
    ///
    /// Built as one pile because the classifier reads content out of blobs:
    /// links are reproduced from admitted text, never asserted as facts.
    #[test]
    fn the_audit_separates_breakage_from_a_forward_reference() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audit.pile");
        File::create(&path).unwrap();
        let signer = SigningKey::from_bytes(&[21; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (archived_tag, archived, _) = tag_record("archived").unwrap();

        // A page that is still on the frontier: linking to it is FINE.
        let (live_fragment, live) = revision_record(draft(author, "live page", [])).unwrap();
        // A page whose only current state is archived: linking to it is the
        // one thing here that is actually broken.
        let mut retired_draft = draft(author, "retired page", []);
        retired_draft.tags.insert(archived);
        let (retired_fragment, retired) = revision_record(retired_draft).unwrap();
        // A legacy anchor: not a selector since 2026-08-18, but its facts are
        // still here, so a reference to it is reachable through the old path.
        let anchor = genid().id;
        let legacy_version = genid().id;
        let mut legacy_fragment = Fragment::empty();
        let legacy_title: TextHandle = legacy_fragment.put("legacy page".to_owned());
        let legacy_content: TextHandle = legacy_fragment.put("legacy body".to_owned());
        legacy_fragment += entity! { ExclusiveId::force_ref(&legacy_version) @
            metadata::tag: &KIND_VERSION_ID,
            attrs::fragment: anchor,
            attrs::title: legacy_title,
            attrs::content: legacy_content,
            metadata::created_at: at(1.0),
        };
        // An id nobody ever minted a fragment for.
        let unwritten = Id::from_hex("11111111111111111111111111111111").unwrap();

        let citing = format!(
            "ok #link(\"wiki:{live:x}\")[live]\n\
             broken #link(\"wiki:{retired:x}\")[retired]\n\
             legacy #link(\"wiki:{anchor:x}\")[anchor]\n\
             todo #link(\"wiki:{unwritten:x}\")[unwritten]\n\
             wrong kind #link(\"wiki:{archived:x}\")[a tag]\n"
        );
        let (citer_fragment, citer) = revision_record(RevisionDraft {
            title: "citing page".to_owned(),
            content: citing,
            tags: BTreeSet::new(),
            predecessors: BTreeSet::new(),
            author,
            authored_at: at(2.0),
        })
        .unwrap();

        let mut fragment = author_fragment + archived_tag;
        fragment += live_fragment;
        fragment += retired_fragment;
        fragment += legacy_fragment;
        fragment += citer_fragment;

        let mut pile = crate::storage::open_pile_strict_as(&path, signer.verifying_key()).unwrap();
        let collection =
            crate::collection_names::open(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key())
                .unwrap();
        pile.commit(collection, &signer, fragment).unwrap();
        let (facts, reader) = materialize_collection(&mut pile, &signer).unwrap();
        let order = latest_index(&facts);
        let model = FrontierModel::load(&reader, &facts, &order).unwrap();

        let index_of = |target: Id| match model.resolve(target) {
            LinkResolution::Unique(index) => index,
            other => panic!("expected one entry for {target:x}, got {other:?}"),
        };
        // The good reference does not fire, and the broken one does.
        assert_eq!(model.classify(live), LinkClass::Live(index_of(live)));
        assert!(!model.classify(live).is_breakage());
        assert_eq!(
            model.classify(retired),
            LinkClass::Retired(index_of(retired))
        );
        assert!(model.classify(retired).is_breakage());
        // Never written is a TODO, not a defect.
        assert_eq!(model.classify(unwritten), LinkClass::Unwritten(None));
        assert!(!model.classify(unwritten).is_breakage());
        // Naming a tag is a different mistake from naming nothing.
        assert_eq!(
            model.classify(archived),
            LinkClass::Unwritten(Some(OtherKind::Tag))
        );
        // The anchor is reachable only through the compatibility path.
        assert_eq!(
            model.classify(anchor),
            LinkClass::Legacy {
                entries: vec![index_of(legacy_version)],
                retired: false,
            }
        );

        let audit = model.audit();
        assert_eq!(audit.live, 1);
        assert_eq!(audit.retired.len(), 1);
        assert_eq!(audit.retired[0].source, citer);
        assert_eq!(audit.retired[0].target, retired);
        assert_eq!(audit.legacy.len(), 1);
        assert_eq!(audit.unwritten.len(), 2);
        assert!(audit.ambiguous.is_empty(), "one target, one entry, no fork");
        // The archived page is not a SOURCE either: only the live frontier is
        // walked, so retiring a page does not resurrect its own citations.
        assert_eq!(audit.states, 3, "live, legacy and citing pages only");

        // The orphan direction falls out of the same walk: the citer is cited
        // by nobody, and the live page it cites is not an orphan.
        let unreferenced: BTreeSet<Id> = model
            .unreferenced(&audit)
            .into_iter()
            .map(|index| model.entries[index].label)
            .collect();
        assert!(unreferenced.contains(&citer));
        assert!(!unreferenced.contains(&live));
        pile.close().unwrap();
    }

    /// Commit `fragment` to a fresh pile and read it the way a migration or a
    /// bootstrap test does: the facts as they are, plus their latest index.
    fn read_committed<T>(
        seed: u8,
        fragment: Fragment,
        read: impl FnOnce(&TribleSet, &PileSnapshot, &LatestIndex) -> T,
    ) -> Result<T> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("open-world.pile");
        File::create(&path).unwrap();
        let signer = SigningKey::from_bytes(&[seed; 32]);
        let mut pile = crate::storage::open_pile_strict_as(&path, signer.verifying_key()).unwrap();
        commit_collection(&mut pile, &signer, fragment).unwrap();
        let result = materialize_collection(&mut pile, &signer).map(|(facts, reader)| {
            let latest = latest_index(&facts);
            read(&facts, &reader, &latest)
        });
        pile.close().unwrap();
        result
    }

    fn without_blobs(fragment: Fragment) -> Fragment {
        let (_, facts, metafacts, _) = fragment.into_parts();
        Fragment::from_parts(facts, metafacts, MemoryBlobStore::new())
    }

    fn legacy_version(id: Id, title: &str, content: &str) -> Fragment {
        let mut fragment = Fragment::empty();
        let title: TextHandle = fragment.put(title.to_owned());
        let content: TextHandle = fragment.put(content.to_owned());
        fragment += entity! { ExclusiveId::force_ref(&id) @
            metadata::tag: &KIND_VERSION_ID,
            attrs::title: title,
            attrs::content: content,
            metadata::created_at: at(1.0),
        };
        fragment
    }

    fn frontier_ids(entry: &EntryRecord) -> BTreeSet<Id> {
        entry.frontier.iter().map(|record| record.id).collect()
    }

    /// One refusal case: committed, read, and checked, or named in `refused`.
    fn case(
        refused: &mut Vec<String>,
        name: &str,
        fragment: Fragment,
        check: impl FnOnce(&TribleSet, &PileSnapshot, &LatestIndex),
    ) {
        if let Err(error) = read_committed(41, fragment, check) {
            refused.push(format!("{name}: {error:#}"));
        }
    }

    /// Every fact the strict catalog used to refuse is an ordinary fact of an
    /// open-world union. Each case here is one refusal the catalog made,
    /// committed to a real pile; the facts are read as they are, and the
    /// reading the Wiki gives each is pinned beside it.
    #[test]
    fn open_world_wiki_facts_are_read_not_refused() {
        let signer = SigningKey::from_bytes(&[41; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (other_fragment, other_author) =
            author_record(&SigningKey::from_bytes(&[42; 32]).verifying_key());
        let base = |title: &str| {
            let (fragment, id) = revision_record(draft(author, title, [])).unwrap();
            (author_fragment.clone() + fragment, id)
        };
        let mut refused = Vec::new();

        // Several values for one attribute: each complete combination is its
        // own reading under the one id.
        let (mut fragment, revision) = base("one");
        let second: TextHandle = fragment.put("another title".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&revision) @ attrs::title: second };
        case(
            &mut refused,
            "two titles",
            fragment,
            |facts, reader, latest| {
                let records = revision_records(facts, revision);
                assert_eq!(records.len(), 2);
                let titles: BTreeSet<String> = records
                    .iter()
                    .map(|record| read_text(reader, record.title).unwrap())
                    .collect();
                assert_eq!(
                    titles,
                    BTreeSet::from(["one".to_owned(), "another title".to_owned()])
                );
                assert_eq!(entries(facts, latest).len(), 1);
            },
        );

        let (mut fragment, revision) = base("one");
        let second: TextHandle = fragment.put("another body".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&revision) @ attrs::content: second };
        case(&mut refused, "two contents", fragment, |facts, _, _| {
            assert_eq!(revision_records(facts, revision).len(), 2);
        });

        let (mut fragment, revision) = base("one");
        fragment += other_fragment.clone();
        fragment += entity! { ExclusiveId::force_ref(&revision) @ attrs::author: other_author };
        case(
            &mut refused,
            "two authors on a native revision",
            fragment,
            |facts, _, _| {
                let authors: BTreeSet<Option<Id>> = revision_records(facts, revision)
                    .iter()
                    .map(|record| record.author)
                    .collect();
                assert_eq!(authors, BTreeSet::from([Some(author), Some(other_author)]));
            },
        );

        let legacy = genid().id;
        let mut fragment = legacy_version(legacy, "legacy", "body");
        fragment += entity! { ExclusiveId::force_ref(&legacy) @
            attrs::author: author,
        };
        fragment += entity! { ExclusiveId::force_ref(&legacy) @
            attrs::author: other_author,
        };
        case(
            &mut refused,
            "two authors on a legacy version",
            fragment,
            |facts, _, _| {
                assert_eq!(revision_records(facts, legacy).len(), 2);
            },
        );

        let (mut fragment, revision) = base("both kinds");
        fragment += entity! { ExclusiveId::force_ref(&revision) @ metadata::tag: &KIND_VERSION_ID };
        case(
            &mut refused,
            "native and legacy kind on one id",
            fragment,
            |facts, _, _| {
                let kinds: BTreeSet<bool> = revision_records(facts, revision)
                    .iter()
                    .map(|record| record.native)
                    .collect();
                assert_eq!(kinds, BTreeSet::from([false, true]));
                assert!(revision_records(facts, revision)
                    .iter()
                    .all(|record| record.tags.is_empty()));
            },
        );

        let legacy = genid().id;
        let mut fragment = Fragment::empty();
        let title: TextHandle = fragment.put("untimed".to_owned());
        let content: TextHandle = fragment.put("body".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&legacy) @
            metadata::tag: &KIND_VERSION_ID,
            attrs::title: title,
            attrs::content: content,
        };
        case(
            &mut refused,
            "legacy version without created_at",
            fragment,
            |facts, _, _| {
                let records = revision_records(facts, legacy);
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].authored_at(), None);
            },
        );

        let legacy = genid().id;
        let mut fragment = legacy_version(legacy, "spanned", "body");
        fragment += entity! { ExclusiveId::force_ref(&legacy) @
            metadata::created_at: span(2.0, 3.0),
        };
        case(
            &mut refused,
            "legacy created_at that is not a point",
            fragment,
            |facts, _, _| {
                let records = revision_records(facts, legacy);
                assert_eq!(
                    records[0].legacy_created_at,
                    BTreeSet::from([at(1.0), span(2.0, 3.0)])
                );
            },
        );

        let (mut fragment, revision) = base("annotated");
        fragment += entity! { ExclusiveId::force_ref(&revision) @
            metadata::description: "an annotation the Wiki does not model",
        };
        case(
            &mut refused,
            "a fact outside a revision's core",
            fragment,
            |facts, _, _| {
                assert_eq!(revision_records(facts, revision).len(), 1);
            },
        );

        let forced = genid().id;
        let mut fragment = author_fragment.clone();
        let title: TextHandle = fragment.put("forced".to_owned());
        let content: TextHandle = fragment.put("body".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&forced) @
            metadata::tag: &KIND_REVISION,
            attrs::author: author,
            attrs::title: title,
            attrs::content: content,
        };
        case(
            &mut refused,
            "a native revision under a non-intrinsic id",
            fragment,
            |facts, _, latest| {
                let records = revision_records(facts, forced);
                assert_eq!(records.len(), 1);
                assert!(records[0].native);
                assert_eq!(records[0].authored_at(), None, "it has no authorship");
                assert_eq!(
                    frontier_ids(&entry(facts, latest, forced).unwrap()),
                    BTreeSet::from([forced])
                );
            },
        );

        let (mut fragment, revision) = base("present");
        let absent = genid().id;
        let stray = authorship_fragment(absent, Some(author), Some(at(1.0))).unwrap();
        let stray_id = stray.root().unwrap();
        fragment += stray;
        case(
            &mut refused,
            "an authorship naming a missing revision",
            fragment,
            |facts, reader, latest| {
                assert_eq!(revision_ids(facts), BTreeSet::from([revision]));
                let model = FrontierModel::load(reader, facts, latest).unwrap();
                assert_eq!(
                    model.classify(stray_id),
                    LinkClass::Unwritten(Some(OtherKind::Authorship))
                );
            },
        );

        let (mut fragment, revision) = base("timed twice");
        let authorship = authorship_fragment(revision, Some(author), Some(at(1.0)))
            .unwrap()
            .root()
            .unwrap();
        fragment += entity! { ExclusiveId::force_ref(&authorship) @ metadata::created_at: at(5.0) };
        case(
            &mut refused,
            "an authorship with two times",
            fragment,
            |facts, _, _| {
                let records = revision_records(facts, revision);
                assert_eq!(records[0].authorships.len(), 2);
                assert_eq!(
                    records[0].authored_at(),
                    Some(at(1.0)),
                    "the earliest time reads"
                );
            },
        );

        let (mut fragment, revision) = base("bare authorship");
        fragment += entity! {
            metadata::tag: &KIND_AUTHORSHIP,
            attrs::revision: revision,
        };
        fragment += entity! {
            metadata::tag: &KIND_AUTHORSHIP,
            attrs::revision: revision,
            metadata::created_at: span(0.5, 3.0),
        };
        case(
            &mut refused,
            "authorships with neither author nor a point time",
            fragment,
            |facts, _, _| {
                let records = revision_records(facts, revision);
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].authorships.len(), 3);
            },
        );

        let (mut fragment, revision) = base("claimed");
        fragment += authorship_fragment(revision, Some(other_author), Some(at(2.0))).unwrap();
        case(
            &mut refused,
            "an authorship naming another author",
            fragment,
            |facts, _, _| {
                let records = revision_records(facts, revision);
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].author, Some(author), "identity author stays");
                let claimed: BTreeSet<Option<Id>> = records[0]
                    .authorships
                    .iter()
                    .map(|record| record.author)
                    .collect();
                assert_eq!(claimed, BTreeSet::from([Some(author), Some(other_author)]));
            },
        );

        let bare = revision_fragment(author, "unattested", "body", &[], &[]);
        let bare_id = bare.root().unwrap();
        case(
            &mut refused,
            "a native revision with no authorship or author description",
            bare,
            |facts, _, latest| {
                assert_eq!(revision_records(facts, bare_id).len(), 1);
                assert_eq!(entries(facts, latest).len(), 1);
            },
        );

        let (mut fragment, _) = base("rekeyed");
        let stray_key: PublicKeyValue = SigningKey::from_bytes(&[43; 32])
            .verifying_key()
            .to_inline();
        fragment += entity! { ExclusiveId::force_ref(&author) @ attestation::signed_by: stray_key };
        case(
            &mut refused,
            "an author description with two keys",
            fragment,
            |facts, _, latest| {
                assert_eq!(entries(facts, latest).len(), 1);
            },
        );

        let (mut fragment, revision) = base("claimed by an outsider");
        let outsider = genid().id;
        fragment += entity! { ExclusiveId::force_ref(&outsider) @ metadata::supersedes: revision };
        case(
            &mut refused,
            "a supersedes fact from a non-revision",
            fragment,
            |facts, reader, latest| {
                // The index reads every supersedes target as superseded, whatever
                // claims it, so the entry has no current state.
                let entry = entry(facts, latest, revision).unwrap();
                assert_eq!(entry.members, vec![revision]);
                assert!(entry.frontier.is_empty());
                assert_eq!(entry_history(facts, &entry).len(), 1);
                let model = FrontierModel::load(reader, facts, latest).unwrap();
                assert_eq!(model.entries.len(), 1);
                assert!(!model.entries[0].active);
            },
        );

        let missing = genid().id;
        let (orphan_fragment, orphan) =
            revision_record(draft(author, "orphan", [missing])).unwrap();
        case(
            &mut refused,
            "a revision whose predecessor is absent",
            author_fragment.clone() + orphan_fragment,
            |facts, _, latest| {
                let entry = entry(facts, latest, orphan).unwrap();
                assert_eq!(entry.members, vec![orphan]);
                assert_eq!(entry.roots, vec![orphan]);
                assert_eq!(frontier_ids(&entry), BTreeSet::from([orphan]));
                assert_eq!(entry_history(facts, &entry).len(), 1);
            },
        );

        let (first, second) = (genid().id, genid().id);
        let mut fragment = legacy_version(first, "T", "one");
        fragment += legacy_version(second, "T", "two");
        fragment += entity! { ExclusiveId::force_ref(&first) @ metadata::supersedes: second };
        fragment += entity! { ExclusiveId::force_ref(&second) @ metadata::supersedes: first };
        case(
            &mut refused,
            "a supersedes cycle",
            fragment,
            |facts, reader, latest| {
                let entry = entry(facts, latest, first).unwrap();
                assert_eq!(entry.members.len(), 2);
                assert!(entry.roots.is_empty(), "every member supersedes another");
                assert!(entry.frontier.is_empty(), "every cycle member is retired");
                let history: Vec<Id> = entry_history(facts, &entry).iter().map(|r| r.id).collect();
                assert_eq!(history.len(), 2, "a cycle drops no member from history");
                let model = FrontierModel::load(reader, facts, latest).unwrap();
                assert_eq!(model.entries[0].label, *entry.members.first().unwrap());
            },
        );

        let looped = genid().id;
        let mut fragment = legacy_version(looped, "T", "self");
        fragment += entity! { ExclusiveId::force_ref(&looped) @ metadata::supersedes: looped };
        case(
            &mut refused,
            "a revision superseding itself",
            fragment,
            |facts, reader, latest| {
                let entry = entry(facts, latest, looped).unwrap();
                assert_eq!(entry_history(facts, &entry).len(), 1);
                FrontierModel::load(reader, facts, latest).unwrap();
            },
        );

        let (root_fragment, root) = revision_record(draft(author, "root", [])).unwrap();
        let (child_fragment, child) = revision_record(draft(author, "child", [root])).unwrap();
        let (redundant_fragment, redundant) =
            revision_record(draft(author, "redundant", [root, child])).unwrap();
        case(
            &mut refused,
            "redundant predecessors",
            author_fragment.clone() + root_fragment + child_fragment + redundant_fragment,
            |facts, _, latest| {
                let entry = entry(facts, latest, root).unwrap();
                assert_eq!(frontier_ids(&entry), BTreeSet::from([redundant]));
                let history: Vec<Id> = entry_history(facts, &entry).iter().map(|r| r.id).collect();
                assert_eq!(history, vec![root, child, redundant]);
            },
        );

        let tag = genid().id;
        let mut tagged = draft(author, "two names", []);
        tagged.tags.insert(tag);
        let (tagged_fragment, _) = revision_record(tagged).unwrap();
        let mut fragment = author_fragment.clone() + tagged_fragment;
        let alpha: TextHandle = fragment.put("alpha".to_owned());
        let beta: TextHandle = fragment.put("beta".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&tag) @ metadata::name: alpha };
        fragment += entity! { ExclusiveId::force_ref(&tag) @ metadata::name: beta };
        case(
            &mut refused,
            "a tag with two names",
            fragment,
            |facts, reader, _| {
                assert_eq!(
                    tag_display_name_from_facts(facts, reader, tag).unwrap(),
                    "alpha / beta"
                );
            },
        );

        let unnamed = genid().id;
        let mut tagged = draft(author, "unnamed tag", []);
        tagged.tags.insert(unnamed);
        let (tagged_fragment, _) = revision_record(tagged).unwrap();
        case(
            &mut refused,
            "an unnamed tag",
            author_fragment.clone() + tagged_fragment,
            |facts, reader, _| {
                assert_eq!(
                    tag_display_name_from_facts(facts, reader, unnamed).unwrap(),
                    format!("{unnamed:x}")
                );
            },
        );

        let mut fragment = author_fragment.clone();
        let mixed: TextHandle = fragment.put(" Mixed ".to_owned());
        let named = entity! { metadata::name: mixed };
        let mixed_tag = named.root().unwrap();
        fragment += named;
        let mut tagged = draft(author, "mixed", []);
        tagged.tags.insert(mixed_tag);
        fragment += revision_record(tagged).unwrap().0;
        case(
            &mut refused,
            "a tag name that is not normalized",
            fragment,
            |facts, reader, _| {
                assert_eq!(
                    tag_display_name_from_facts(facts, reader, mixed_tag).unwrap(),
                    " Mixed "
                );
            },
        );

        let mut fragment = author_fragment.clone();
        let retired: TextHandle = fragment.put("retired".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&TAG_ARCHIVED_ID) @ metadata::name: retired };
        let mut archived = draft(author, "archived by id", []);
        archived.tags.insert(TAG_ARCHIVED_ID);
        fragment += revision_record(archived).unwrap().0;
        case(
            &mut refused,
            "a built-in tag under another name",
            fragment,
            |facts, reader, latest| {
                assert_eq!(
                    tag_display_name_from_facts(facts, reader, TAG_ARCHIVED_ID).unwrap(),
                    "retired"
                );
                let model = FrontierModel::load(reader, facts, latest).unwrap();
                assert!(!model.entries[0].active, "archived is the id, not the name");
            },
        );

        let mut untitled = revision_fragment(author, "", "body", &[], &[]);
        let untitled_id = untitled.root().unwrap();
        untitled += authorship_fragment(untitled_id, Some(author), Some(at(1.0))).unwrap();
        case(
            &mut refused,
            "an empty title",
            author_fragment.clone() + untitled,
            |facts, reader, _| {
                let records = revision_records(facts, untitled_id);
                assert_eq!(read_text(reader, records[0].title).unwrap(), "");
            },
        );

        let (absent_fragment, absent_revision) =
            revision_record(draft(author, "elsewhere", [])).unwrap();
        let mut fragment = author_fragment.clone() + without_blobs(absent_fragment);
        let (tag_fragment, tag_id, _) = tag_record("held elsewhere").unwrap();
        fragment += without_blobs(tag_fragment);
        case(
            &mut refused,
            "texts that are not resident",
            fragment,
            |facts, reader, latest| {
                assert_eq!(revision_ids(facts), BTreeSet::from([absent_revision]));
                assert_eq!(entries(facts, latest).len(), 1);
                let record = &revision_records(facts, absent_revision)[0];
                assert!(
                    read_text(reader, record.title).is_err(),
                    "unreadable only when asked"
                );
                assert!(tag_display_name_from_facts(facts, reader, tag_id).is_err());
            },
        );

        assert!(
            refused.is_empty(),
            "refused open-world facts:\n{}",
            refused.join("\n")
        );
    }

    /// A page citing ITSELF is not a page anyone linked to.
    #[test]
    fn a_self_citation_does_not_make_an_entry_referenced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("self.pile");
        File::create(&path).unwrap();
        let signer = SigningKey::from_bytes(&[22; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (root_fragment, root) = revision_record(draft(author, "root", [])).unwrap();
        let (successor_fragment, _) = revision_record(RevisionDraft {
            title: "root".to_owned(),
            content: format!("see #link(\"wiki:{root:x}\")[my own first draft]\n"),
            tags: BTreeSet::new(),
            predecessors: BTreeSet::from([root]),
            author,
            authored_at: at(2.0),
        })
        .unwrap();

        let mut pile = crate::storage::open_pile_strict_as(&path, signer.verifying_key()).unwrap();
        let collection =
            crate::collection_names::open(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key())
                .unwrap();
        pile.commit(
            collection,
            &signer,
            author_fragment + root_fragment + successor_fragment,
        )
        .unwrap();
        let (facts, reader) = materialize_collection(&mut pile, &signer).unwrap();
        let order = latest_index(&facts);
        let model = FrontierModel::load(&reader, &facts, &order).unwrap();
        let audit = model.audit();
        assert_eq!(audit.live, 1, "the citation resolves");
        assert_eq!(audit.incoming, vec![0], "but not as an incoming reference");
        assert_eq!(model.unreferenced(&audit).len(), 1);
        pile.close().unwrap();
    }

    /// The audit is READ-ONLY, and this compares the pile's bytes to prove it.
    ///
    /// The real corpus is written by other windows while a report runs, so its
    /// size moving proves nothing either way; a pile nobody else holds is the
    /// only place the claim can actually be tested. Byte equality rather than a
    /// digest: it is a small file and an exact comparison cannot be fooled.
    #[test]
    fn the_audit_leaves_the_pile_byte_identical() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("readonly.pile");
        File::create(&path).unwrap();
        let signer = SigningKey::from_bytes(&[23; 32]);
        let (author_fragment, author) = author_record(&signer.verifying_key());
        let (root_fragment, root) = revision_record(draft(author, "root", [])).unwrap();
        let (citer_fragment, _) = revision_record(RevisionDraft {
            title: "citer".to_owned(),
            content: format!("#link(\"wiki:{root:x}\")[root]\n"),
            tags: BTreeSet::new(),
            predecessors: BTreeSet::new(),
            author,
            authored_at: at(2.0),
        })
        .unwrap();
        let mut pile = crate::storage::open_pile_strict_as(&path, signer.verifying_key()).unwrap();
        let collection =
            crate::collection_names::open(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key())
                .unwrap();
        pile.commit(
            collection,
            &signer,
            author_fragment + root_fragment + citer_fragment,
        )
        .unwrap();
        pile.close().unwrap();
        let before = std::fs::read(&path).unwrap();

        // Exactly what `wiki links` and `wiki check` do, and nothing else.
        let mut pile = crate::storage::open_pile_strict_as(&path, signer.verifying_key()).unwrap();
        let (facts, reader) = materialize_collection(&mut pile, &signer).unwrap();
        let order = latest_index(&facts);
        let model = FrontierModel::load(&reader, &facts, &order).unwrap();
        let audit = model.audit();
        let _ = model.unreferenced(&audit);
        pile.close().unwrap();

        assert_eq!(audit.live, 1, "the fixture must actually have been read");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "auditing links must not append a single byte"
        );
    }
}
