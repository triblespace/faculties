//! Collection-native Compass values and semantic views.
//!
//! Compass is deliberately a small event algebra, not a mutable board
//! snapshot protocol. Goals and notes are stable occurrence anchors; status
//! and priority changes are immutable events. The collection value is their
//! set union. Reads derive the current view with one deterministic rule:
//! timestamp first, event id second. Replica merge therefore cannot make the
//! visible board depend on insertion or iteration order.

pub mod cli;
pub mod mcp;
pub mod operations;
mod render;

pub use operations::{
    AddOptions, AddedGoal, AddedNote, Compass, ListOptions, MovedGoal, NoteOptions, PriorityChange,
};

use crate::storage::FactRead;
use std::collections::{BTreeMap, BTreeSet};

use anybytes::View;
use anyhow::{bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use triblespace::core::collection::lww_register::{LwwIndex, LwwQuery, LwwRegisterBlob};
use triblespace::core::collection::{
    CollectionCommit, CollectionRead, CollectionSnapshotExt, CollectionStoreExt,
};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::async_store::AsyncBlobStoreAcquire;
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::{BlobStoreGet, CapabilityProofRead, SnapshotSource};
use triblespace::macros::{entity, find, pattern};
use triblespace::prelude::*;

use crate::collection_names::open_configured;
use crate::schemas::compass::{
    board, DEFAULT_SCOPE_ID, KIND_DEPRIORITIZE_ID, KIND_GOAL_ID, KIND_NOTE_ID, KIND_PRIORITIZE_ID,
    KIND_SPECS, KIND_STATUS_ID,
};
use crate::storage::FactArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};

pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;

/// One coherent Compass source snapshot plus its maintained status register.
///
/// Facts, positive status membership, and blob reader are captured at one
/// immutable store observation without requiring equal support. Maintained
/// artifacts are cache exhaust, never additional semantic authority.
pub struct CompassSnapshot<R = PileSnapshot> {
    facts: FactArchive,
    store_snapshot: R,
    status: LwwQuery,
}

impl<R> CompassSnapshot<R> {
    /// Shard-preserving facts admitted by this exact snapshot.
    pub fn facts(&self) -> &FactArchive {
        &self.facts
    }

    /// Store snapshot captured with this exact collection view.
    pub fn store_snapshot(&self) -> &R {
        &self.store_snapshot
    }

    /// Known complete LWW winners attached from the same store observation.
    pub fn status_register(&self) -> &LwwQuery {
        &self.status
    }

    /// Consume the coherent snapshot into facts, store snapshot, and status index.
    pub fn into_parts(self) -> (FactArchive, R, LwwQuery) {
        (self.facts, self.store_snapshot, self.status)
    }
}

/// The exact maintained LWW projection used for current Compass status.
pub fn status_register_collection<S>(
    store: &mut S,
    authority: VerifyingKey,
) -> Result<Collection<LwwRegisterBlob>>
where
    S: CollectionStoreExt + SnapshotSource,
    <S as SnapshotSource>::Snapshot: BlobStoreGet + CapabilityProofRead + CollectionRead,
{
    let source = crate::collection_names::open_configured(store, DEFAULT_SCOPE_ID, authority)?;
    status_register_for_source(store, source)
}

pub(crate) fn status_register_for_source<S>(
    store: &mut S,
    source: Collection<blobencodings::SimpleArchive>,
) -> Result<Collection<LwwRegisterBlob>>
where
    S: CollectionStoreExt + SnapshotSource,
    <S as SnapshotSource>::Snapshot: BlobStoreGet + CapabilityProofRead,
{
    Ok(store
        .attach::<LwwRegisterBlob>(source, (board::status_of.id(), metadata::created_at.id()))?)
}

fn validate_label(label: &str, value: &str) -> Result<()> {
    if value.bytes().any(|byte| byte == 0) {
        bail!("{label} contains a NUL byte: {value}");
    }
    Ok(())
}

pub fn canonical_status(value: impl Into<String>) -> Result<String> {
    let value = value.into().trim().to_ascii_lowercase();
    validate_label("status", &value)?;
    Ok(value)
}

pub fn canonical_tags(values: impl IntoIterator<Item = String>) -> Result<Vec<String>> {
    let mut values: Vec<String> = values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .collect();
    for value in &values {
        validate_label("tag", value)?;
    }
    values.sort();
    values.dedup();
    Ok(values)
}

/// The `@label` tokens in a goal title or note body, in first-seen order and
/// deduplicated by [`crate::relations::lookup_key`].
///
/// A token starts at the beginning of the text or after a character that is
/// not alphanumeric, so `ada@example.org` mentions nobody. The label runs over
/// ASCII letters, digits, `_`, `.` and `-`, with trailing punctuation trimmed,
/// so `@ada.` names `ada`.
pub fn mention_labels(text: &str) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    let mut previous: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        let starts = ch == '@' && !previous.is_some_and(char::is_alphanumeric);
        previous = Some(ch);
        if !starts {
            continue;
        }
        let rest = &text[at + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
            .unwrap_or(rest.len());
        let label = rest[..end].trim_end_matches(['_', '.', '-']);
        if !label.is_empty()
            && !labels.iter().any(|seen| {
                crate::relations::lookup_key(seen) == crate::relations::lookup_key(label)
            })
        {
            labels.push(label.to_owned());
        }
    }
    labels
}

/// The Relations anchors `targets` that a goal or note's text mentioned, as
/// an annotation beside that entity's core. The entity id is whatever its own
/// constructor derived; mentions never enter it.
pub fn mention_fragment(entity: Id, targets: &[Id]) -> Fragment {
    let targets = sorted_ids(targets.iter().copied());
    if targets.is_empty() {
        return Fragment::empty();
    }
    entity! { ExclusiveId::force_ref(&entity) @ board::mention*: targets.iter() }
}

fn sorted_ids(values: impl IntoIterator<Item = Id>) -> Vec<Id> {
    let mut values: Vec<Id> = values.into_iter().collect();
    values.sort_unstable();
    values.dedup();
    values
}

fn sorted_strings(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values: Vec<String> = values.into_iter().collect();
    values.sort();
    values.dedup();
    values
}

/// Self-contained descriptions of Compass's published event kinds.
///
/// Adding these same facts to every authored action is harmless set
/// duplication and lets a fresh collection explain itself without a special
/// bootstrap transaction.
pub fn kind_catalog_fragment() -> Fragment {
    let mut fragment = Fragment::empty();
    for (id, label) in KIND_SPECS {
        let name = fragment.put::<blobencodings::UTF8String, _>(label.to_owned());
        fragment += entity! { ExclusiveId::force_ref(&id) @ metadata::name: name };
    }
    fragment
}

/// One immutable goal occurrence with an identity derived from all of its
/// defining fields. Exact replay converges; the creation time distinguishes
/// otherwise-equal occurrences.
pub fn goal_fragment(
    title: impl Into<String>,
    tags: Vec<String>,
    parent: Option<Id>,
    created_at: IntervalValue,
) -> Result<(Fragment, Id)> {
    goal_fragment_impl(None, title, tags, parent, created_at)
}

fn goal_fragment_impl(
    goal: Option<Id>,
    title: impl Into<String>,
    tags: Vec<String>,
    parent: Option<Id>,
    created_at: IntervalValue,
) -> Result<(Fragment, Id)> {
    let tags = canonical_tags(tags)?;
    let mut fragment = Fragment::empty();
    let title = fragment.put::<blobencodings::UTF8String, _>(title.into());
    let record = if let Some(goal) = goal {
        entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: title,
            metadata::created_at: created_at,
            board::parent?: parent.as_ref(),
            board::tag*: tags.iter().map(String::as_str),
        }
    } else {
        entity! {
            metadata::tag: &KIND_GOAL_ID,
            board::title: title,
            metadata::created_at: created_at,
            board::parent?: parent.as_ref(),
            board::tag*: tags.iter().map(String::as_str),
        }
    };
    let goal = record.root().expect("goal record exports one root");
    fragment += record;
    Ok((fragment, goal))
}

/// One intrinsic status event. Exact replay has the same entity id; two
/// independently timed actions remain distinct events.
///
/// The goal is named through `board::status_of`, not `board::task`: this
/// event is a state of *the status of* that goal, which is a narrower claim
/// than *attached to* it and is the register's identity. `board::task`
/// remains readable on events written before that identity existed, but is
/// no longer written or read for status.
pub fn status_fragment(
    goal: Id,
    status: impl Into<String>,
    by: Option<Id>,
    created_at: IntervalValue,
) -> Result<Fragment> {
    let status = canonical_status(status)?;
    Ok(entity! {
        metadata::tag: &KIND_STATUS_ID,
        board::status_of: &goal,
        board::status: status.as_str(),
        board::by?: by.as_ref(),
        metadata::created_at: created_at,
    })
}

/// One independent note occurrence whose identity includes every immutable
/// field. Creation time distinguishes repeated prose while exact retries
/// converge to one occurrence.
#[allow(clippy::too_many_arguments)]
pub fn note_fragment(
    goal: Id,
    body: impl Into<String>,
    tags: Vec<String>,
    references: Vec<String>,
    supersedes: Vec<Id>,
    by: Option<Id>,
    created_at: IntervalValue,
) -> Result<(Fragment, Id)> {
    note_fragment_impl(
        None, goal, body, tags, references, supersedes, by, created_at,
    )
}

#[allow(clippy::too_many_arguments)]
fn note_fragment_impl(
    note: Option<Id>,
    goal: Id,
    body: impl Into<String>,
    tags: Vec<String>,
    references: Vec<String>,
    supersedes: Vec<Id>,
    by: Option<Id>,
    created_at: IntervalValue,
) -> Result<(Fragment, Id)> {
    let tags = canonical_tags(tags)?;
    let references = sorted_strings(references);
    let supersedes = sorted_ids(supersedes);
    let mut fragment = Fragment::empty();
    let body = fragment.put::<blobencodings::UTF8String, _>(body.into());
    let references: Vec<TextHandle> = references
        .into_iter()
        .map(|value| fragment.put::<blobencodings::UTF8String, _>(value))
        .collect();
    let record = if let Some(note) = note {
        entity! { ExclusiveId::force_ref(&note) @
            metadata::tag: &KIND_NOTE_ID,
            board::task: &goal,
            board::note: body,
            board::by?: by.as_ref(),
            board::tag*: tags.iter().map(String::as_str),
            board::reference*: references.iter(),
            metadata::supersedes*: supersedes.iter(),
            metadata::created_at: created_at,
        }
    } else {
        entity! {
            metadata::tag: &KIND_NOTE_ID,
            board::task: &goal,
            board::note: body,
            board::by?: by.as_ref(),
            board::tag*: tags.iter().map(String::as_str),
            board::reference*: references.iter(),
            metadata::supersedes*: supersedes.iter(),
            metadata::created_at: created_at,
        }
    };
    let note = record.root().expect("note record exports one root");
    fragment += record;
    Ok((fragment, note))
}

/// Explicit-id constructors for migrations and fixtures which must replay a
/// preserved identity. Ordinary authoring must use [`goal_fragment`] and
/// [`note_fragment`], whose intrinsic roots make exact retries idempotent.
///
/// Readers deliberately do not distinguish records produced here from records
/// produced by the intrinsic constructors, and never recompute or compare an
/// id. The identifier substitution law is load-bearing: changing only an
/// entity id may affect idempotence, never meaning.
pub mod replay {
    use super::*;

    pub fn goal_fragment(
        goal: Id,
        title: impl Into<String>,
        tags: Vec<String>,
        parent: Option<Id>,
        created_at: IntervalValue,
    ) -> Result<Fragment> {
        super::goal_fragment_impl(Some(goal), title, tags, parent, created_at)
            .map(|(fragment, _)| fragment)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn note_fragment(
        note: Id,
        goal: Id,
        body: impl Into<String>,
        tags: Vec<String>,
        references: Vec<String>,
        supersedes: Vec<Id>,
        by: Option<Id>,
        created_at: IntervalValue,
    ) -> Result<Fragment> {
        super::note_fragment_impl(
            Some(note),
            goal,
            body,
            tags,
            references,
            supersedes,
            by,
            created_at,
        )
        .map(|(fragment, _)| fragment)
    }
}

/// One intrinsic priority assertion or retraction event.
pub fn priority_fragment(
    higher: Id,
    lower: Id,
    active: bool,
    created_at: IntervalValue,
) -> Fragment {
    let kind = if active {
        KIND_PRIORITIZE_ID
    } else {
        KIND_DEPRIORITIZE_ID
    };
    entity! {
        metadata::tag: &kind,
        board::higher: &higher,
        board::lower: &lower,
        metadata::created_at: created_at,
    }
}

pub fn goal_ids<P: TriblePattern>(facts: &P) -> BTreeSet<Id> {
    find!(
        goal: Id,
        pattern!(facts, [{ ?goal @ metadata::tag: &KIND_GOAL_ID }])
    )
    .collect()
}

pub fn note_ids<P: TriblePattern>(facts: &P) -> BTreeSet<Id> {
    find!(
        note: Id,
        pattern!(facts, [{ ?note @
            metadata::tag: &KIND_NOTE_ID,
            board::task: _?goal,
            board::note: _?body,
        }])
    )
    .collect()
}

/// Active explicit priority edges after deterministic last-event reduction.
///
/// Every complete reading of an event counts: an event naming two higher
/// goals speaks for both edges. An event whose time does not decode is not
/// one this reader can order, so it is skipped.
pub fn active_priority_edges<P: TriblePattern>(facts: &P) -> BTreeSet<(Id, Id)> {
    let mut latest: BTreeMap<(Id, Id), ((i128, Id), bool)> = BTreeMap::new();
    let mut absorb = |event: Id, higher: Id, lower: Id, at: IntervalValue, active: bool| {
        let Ok((lower_bound, _)): Result<(i128, i128), _> = at.try_from_inline() else {
            return;
        };
        let order = (lower_bound, event);
        latest
            .entry((higher, lower))
            .and_modify(|(current, value)| {
                if order > *current {
                    *current = order;
                    *value = active;
                }
            })
            .or_insert((order, active));
    };

    for (event, higher, lower, at) in find!(
        (event: Id, higher: Id, lower: Id, at: IntervalValue),
        pattern!(facts, [{ ?event @
            metadata::tag: &KIND_PRIORITIZE_ID,
            board::higher: ?higher,
            board::lower: ?lower,
            metadata::created_at: ?at,
        }])
    ) {
        absorb(event, higher, lower, at, true);
    }
    for (event, higher, lower, at) in find!(
        (event: Id, higher: Id, lower: Id, at: IntervalValue),
        pattern!(facts, [{ ?event @
            metadata::tag: &KIND_DEPRIORITIZE_ID,
            board::higher: ?higher,
            board::lower: ?lower,
            metadata::created_at: ?at,
        }])
    ) {
        absorb(event, higher, lower, at, false);
    }

    latest
        .into_iter()
        .filter_map(|(edge, (_, active))| active.then_some(edge))
        .collect()
}

/// The complete priority relation used by Compass views.
///
/// Explicit priority assertions are joined with the structural rule that a
/// child precedes its parent. Keeping this interpretation here makes every
/// presentation of the board read the same partial order.
pub fn goal_priority_edges<P: TriblePattern>(facts: &P) -> BTreeSet<(Id, Id)> {
    let goals = goal_ids(facts);
    let mut edges = active_priority_edges(facts);
    for (child, parent) in find!(
        (child: Id, parent: Id),
        pattern!(facts, [{
            ?child @
                metadata::tag: &KIND_GOAL_ID,
                board::parent: ?parent,
        }])
    ) {
        if goals.contains(&parent) {
            edges.insert((child, parent));
        }
    }
    edges
}

/// Return whether adding `higher > lower` would close a priority cycle.
pub fn would_create_priority_cycle(edges: &BTreeSet<(Id, Id)>, higher: Id, lower: Id) -> bool {
    let mut visited = BTreeSet::new();
    let mut pending = vec![lower];
    while let Some(node) = pending.pop() {
        if node == higher {
            return true;
        }
        if !visited.insert(node) {
            continue;
        }
        pending.extend(
            edges
                .iter()
                .filter_map(|(from, to)| (*from == node).then_some(*to)),
        );
    }
    false
}

/// Assign topological priority tiers (lower is more important).
///
/// All currently maximal, mutually unordered goals share a tier, so callers
/// can use recency or another presentation concern as a genuine tie-breaker.
/// A malformed cyclic remainder shares one final tier instead of making a
/// read fail; writers still reject cycles with [`would_create_priority_cycle`].
pub fn priority_ranks(
    goal_ids: impl IntoIterator<Item = Id>,
    edges: &BTreeSet<(Id, Id)>,
) -> BTreeMap<Id, usize> {
    let goals: BTreeSet<Id> = goal_ids.into_iter().collect();
    let mut outgoing = BTreeMap::<Id, BTreeSet<Id>>::new();
    let mut incoming = goals
        .iter()
        .copied()
        .map(|goal| (goal, 0usize))
        .collect::<BTreeMap<_, _>>();

    for &(higher, lower) in edges {
        if goals.contains(&higher)
            && goals.contains(&lower)
            && outgoing.entry(higher).or_default().insert(lower)
        {
            *incoming.get_mut(&lower).expect("known priority goal") += 1;
        }
    }

    let mut frontier: BTreeSet<Id> = incoming
        .iter()
        .filter_map(|(&goal, &count)| (count == 0).then_some(goal))
        .collect();
    let mut ranks = BTreeMap::new();
    let mut rank = 0usize;
    while !frontier.is_empty() {
        let current = std::mem::take(&mut frontier);
        for goal in &current {
            ranks.insert(*goal, rank);
        }
        for goal in current {
            if let Some(lower_goals) = outgoing.get(&goal) {
                for lower in lower_goals {
                    let count = incoming.get_mut(lower).expect("known priority goal");
                    *count -= 1;
                    if *count == 0 {
                        frontier.insert(*lower);
                    }
                }
            }
        }
        rank += 1;
    }

    for goal in goals {
        ranks.entry(goal).or_insert(rank);
    }
    ranks
}

pub fn read_text(reader: &impl BlobStoreGet, handle: TextHandle) -> Result<String> {
    let value: View<str> = reader.get(handle).context("load Compass text")?;
    Ok(value.to_string())
}

/// Read every Compass text that `facts` names through `reader`.
///
/// This is a writer confirming that what it just published is readable, at
/// its own write boundary, over its own facts. No reader calls it on a
/// collection: there a text that has not arrived is read, and reported, at
/// the point of use, and the rest of the view stays readable.
pub fn require_text_payloads(reader: &impl BlobStoreGet, facts: &TribleSet) -> Result<()> {
    let texts = [
        board::title.id(),
        board::note.id(),
        board::reference.id(),
        board::tag.id(),
        board::status.id(),
    ];
    for fact in facts {
        if texts.contains(fact.a()) {
            let handle = *fact.v::<inlineencodings::Handle<blobencodings::UTF8String>>();
            let _: View<str> = reader.get(handle).with_context(|| {
                format!(
                    "read Compass text payload {}",
                    hex::encode_upper(handle.raw)
                )
            })?;
        }
    }
    Ok(())
}

/// Materialize the complete WRITE-authorized Compass collection through an
/// already-open pile.
///
/// The view is the collection's facts, as they are: nothing is checked
/// against a closed shape, and a text that is not resident is only a missing
/// blob for whichever reader later asks for it.
pub fn materialize_collection(
    pile: &mut Pile,
    signer: &SigningKey,
) -> Result<(TribleSet, PileSnapshot)> {
    let collection = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
    let store_snapshot = pile.snapshot().context("freeze Compass store snapshot")?;
    let facts = store_snapshot
        .collection(collection)
        .context("attach Compass collection")?
        .view::<TribleSet>()
        .context("read Compass collection")?;
    Ok((facts, store_snapshot))
}

/// Capture Compass facts and the positive status LWW index through one store
/// observation. An admitted producer first maintains the available mapping
/// inputs; other readers attach resident targets without publishing equations.
/// Missing source payloads do not block an already-readable target view.
pub async fn materialize_indexed_collection<S>(
    pile: &mut S,
    signer: &SigningKey,
) -> Result<CompassSnapshot<S::Snapshot>>
where
    S: Store + AsyncBlobStoreAcquire + Send,
{
    let source = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
    materialize_indexed_source(pile, source).await
}

/// Attach the resident Compass views: the fact archive carried over the
/// source and the status register.
///
/// A read attaches what the maintenance worker has carried and never
/// maintains. Under one-of-three roots every host key is admitted everywhere,
/// so a read that maintained when admitted paid the whole chain's catch-up on
/// every call: measured on sky, 2026-09-20, 49 seconds against 13.5 seconds
/// for the attach-only branch, and 956 seconds for the first read after a
/// write. A write ensures its own images before it returns, so its author
/// reads it back at once; a commit that arrived by sync and that nobody has
/// imaged yet waits for the worker, like one nobody has synced.
async fn materialize_indexed_source<S>(
    pile: &mut S,
    source: Collection<blobencodings::SimpleArchive>,
) -> Result<CompassSnapshot<S::Snapshot>>
where
    S: Store + AsyncBlobStoreAcquire + Send,
{
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    let status_target = status_register_for_source(pile, source)?;
    let store_snapshot = pile.snapshot().context("freeze resident Compass targets")?;
    let fact_archive = store_snapshot
        .read_facts(rank9)
        .context("read Compass fact collection")?;
    // The register over the same foundations the facts read: its cover, and
    // every commit no attachment reaches built in memory, so a status the
    // facts hold is never missing from its winners.
    let status = store_snapshot
        .attached(status_target)
        .context("observe Compass status register")?
        .read::<LwwIndex>()
        .context("read Compass status register")?
        .into_value()
        .query()
        .context("prepare Compass status register query")?;
    Ok(CompassSnapshot {
        facts: fact_archive,
        store_snapshot,
        status,
    })
}

/// Publish one complete Compass action through an already-open pile.
///
/// This is the commit and nothing else. The write path that calls it then
/// ensures the views derived from the Compass source, so the action is
/// readable through them before the command returns; a failure there is
/// reported as "committed, but ensuring failed", because the commit is
/// durable and must not be retried.
pub fn commit_collection<S>(
    pile: &mut S,
    signer: &SigningKey,
    fragment: Fragment,
) -> Result<CollectionCommit>
where
    S: CollectionStoreExt + SnapshotSource,
    S::Snapshot: BlobStoreGet + CollectionRead,
{
    let collection = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
    pile.commit(collection, signer, fragment)
        .context("commit Compass collection fragment")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hifitime::Epoch;
    use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
    use triblespace::core::repo::memoryrepo::MemoryRepo;

    fn at(value: i128) -> IntervalValue {
        let value = Epoch::from_unix_seconds(value as f64);
        (value, value).try_to_inline().unwrap()
    }

    /// What the maintenance worker does between reads: carry the source's
    /// commits through the chain and the status register.
    async fn carry(
        store: &mut MemoryRepo,
        source: Collection<blobencodings::SimpleArchive>,
        signer: &SigningKey,
    ) {
        let succinct = store.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
        let rank9 = store
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
            .unwrap();
        let status = status_register_for_source(store, source).unwrap();
        drop(store.maintain_attached(succinct, signer).await.unwrap());
        drop(store.maintain_attached(rank9, signer).await.unwrap());
        drop(store.maintain_attached(status, signer).await.unwrap());
    }

    /// A host where nothing ever attached the status register -- no MAP
    /// names it, so no maintenance pass finds it -- reads every goal's
    /// status all the same, from the commits' own bytes.
    #[test]
    fn a_status_register_nothing_attached_reads_every_status() {
        pollster::block_on(async {
            let owner = SigningKey::from_bytes(&[25; 32]);
            let mut store = MemoryRepo::for_host(owner.verifying_key());
            let source =
                crate::collection_names::open(&mut store, DEFAULT_SCOPE_ID, owner.verifying_key())
                    .unwrap();
            let (mut first, goal) = goal_fragment("first", vec![], None, at(1)).unwrap();
            first += status_fragment(goal, "doing", None, at(1)).unwrap();
            store.commit(source, &owner, first).unwrap();
            let resident = materialize_indexed_source(&mut store, source)
                .await
                .unwrap();
            let register = status_register_for_source(&mut store, source).unwrap();
            assert!(store
                .snapshot()
                .unwrap()
                .attached(register)
                .unwrap()
                .cover()
                .is_empty());
            assert_eq!(
                crate::schemas::compass::latest_status_event(
                    resident.facts(),
                    resident.status_register(),
                    goal,
                )
                .unwrap()
                .1,
                crate::schemas::compass::text_handle("doing"),
            );
        });
    }

    #[test]
    fn indexed_reads_attach_what_the_worker_carried_and_never_publish() {
        pollster::block_on(async {
            let owner = SigningKey::from_bytes(&[23; 32]);
            let reader = SigningKey::from_bytes(&[24; 32]);
            let mut store = MemoryRepo::for_host(owner.verifying_key());
            let source =
                crate::collection_names::open(&mut store, DEFAULT_SCOPE_ID, owner.verifying_key())
                    .unwrap();
            let (mut first, goal) = goal_fragment("earlier", vec![], None, at(1)).unwrap();
            first += status_fragment(goal, "todo", None, at(1)).unwrap();
            store.commit(source, &owner, first).unwrap();
            carry(&mut store, source, &owner).await;
            let warm = materialize_indexed_source(&mut store, source)
                .await
                .unwrap();
            assert_eq!(
                crate::schemas::compass::latest_status_event(
                    warm.facts(),
                    warm.status_register(),
                    goal,
                )
                .unwrap()
                .1,
                crate::schemas::compass::text_handle("todo"),
            );

            let (mut later, new_goal) = goal_fragment("later", vec![], None, at(2)).unwrap();
            later += status_fragment(new_goal, "todo", None, at(2)).unwrap();
            later += status_fragment(goal, "done", None, at(2)).unwrap();
            store.commit(source, &owner, later).unwrap();
            let before = store
                .snapshot()
                .unwrap()
                .records()
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();

            // Neither an unadmitted reader nor the owner advances the chain on
            // a read, and neither publishes anything. The facts read the new
            // commit from its own bytes, and so does the status register: the
            // two answer for the same foundations, so the goal's newer status
            // and the new goal's status are winners before the worker
            // attaches the commit.
            for _signer in [&reader, &owner] {
                let resident = materialize_indexed_source(&mut store, source)
                    .await
                    .unwrap();
                assert_eq!(
                    find!(id: Id, pattern!(resident.facts(), [{ ?id @ metadata::tag: &KIND_GOAL_ID }]))
                        .collect::<BTreeSet<_>>(),
                    BTreeSet::from([goal, new_goal]),
                );
                assert_eq!(
                    crate::schemas::compass::latest_status_event(
                        resident.facts(),
                        resident.status_register(),
                        goal,
                    )
                    .unwrap()
                    .1,
                    crate::schemas::compass::text_handle("done"),
                );
                assert_eq!(
                    crate::schemas::compass::latest_status_event(
                        resident.facts(),
                        resident.status_register(),
                        new_goal,
                    )
                    .unwrap()
                    .1,
                    crate::schemas::compass::text_handle("todo"),
                );
                assert_eq!(
                    resident
                        .store_snapshot()
                        .records()
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                    before,
                    "a reader must not publish maintenance equations",
                );
            }

            carry(&mut store, source, &owner).await;
            let current = materialize_indexed_source(&mut store, source)
                .await
                .unwrap();
            assert_eq!(
                find!(id: Id, pattern!(current.facts(), [{ ?id @ metadata::tag: &KIND_GOAL_ID }]))
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([goal, new_goal]),
            );
            assert_eq!(
                crate::schemas::compass::latest_status_event(
                    current.facts(),
                    current.status_register(),
                    goal,
                )
                .unwrap()
                .1,
                crate::schemas::compass::text_handle("done"),
            );
        });
    }

    #[test]
    fn indexed_read_keeps_carried_targets_with_a_cold_new_source_member() {
        pollster::block_on(async {
            let owner = SigningKey::from_bytes(&[25; 32]);
            let mut store = MemoryRepo::for_host(owner.verifying_key());
            let source =
                crate::collection_names::open(&mut store, DEFAULT_SCOPE_ID, owner.verifying_key())
                    .unwrap();
            let (mut first, goal) = goal_fragment("resident goal", vec![], None, at(1)).unwrap();
            first += status_fragment(goal, "todo", None, at(1)).unwrap();
            store.commit(source, &owner, first).unwrap();
            carry(&mut store, source, &owner).await;

            let (mut later, new_goal) = goal_fragment("cold goal", vec![], None, at(2)).unwrap();
            later += status_fragment(new_goal, "todo", None, at(2)).unwrap();
            later += status_fragment(goal, "done", None, at(2)).unwrap();
            let mut remote = MemoryRepo::default();
            let arriving = remote.commit(source, &owner, later.clone()).unwrap();
            store
                .insert(triblespace::core::collection::CollectionRecord::Commit(
                    arriving,
                ))
                .unwrap();
            let cold =
                inlineencodings::Handle::<blobencodings::SimpleArchive>::from_hash(arriving.data());
            let before = store.snapshot().unwrap();
            assert!(!before.contains_blob(cold).unwrap());
            let records = before
                .records()
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();

            let resident = materialize_indexed_source(&mut store, source)
                .await
                .unwrap();
            assert_eq!(goal_ids(resident.facts()), BTreeSet::from([goal]));
            assert_eq!(
                crate::schemas::compass::latest_status_event(
                    resident.facts(),
                    resident.status_register(),
                    goal,
                )
                .unwrap()
                .1,
                crate::schemas::compass::text_handle("todo"),
            );
            assert!(!resident.store_snapshot().contains_blob(cold).unwrap());
            assert_eq!(
                resident
                    .store_snapshot()
                    .records()
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                records,
                "a cold source member must not require a new read-side equation",
            );

            // Normal catch-up remains available once these exact bytes arrive
            // and the worker has carried them.
            assert_eq!(store.commit(source, &owner, later).unwrap(), arriving);
            carry(&mut store, source, &owner).await;
            let current = materialize_indexed_source(&mut store, source)
                .await
                .unwrap();
            assert_eq!(goal_ids(current.facts()), BTreeSet::from([goal, new_goal]));
            assert_eq!(
                crate::schemas::compass::latest_status_event(
                    current.facts(),
                    current.status_register(),
                    goal,
                )
                .unwrap()
                .1,
                crate::schemas::compass::text_handle("done"),
            );
        });
    }

    #[test]
    fn status_register_is_attached_to_its_source_and_carries_no_policy() {
        let mut store = MemoryRepo::default();
        let authority = SigningKey::from_bytes(&[13; 32]).verifying_key();
        let policy =
            CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::delegable(authority));
        let source = store.collection("shared-compass", policy).unwrap();

        let register = status_register_for_source(&mut store, source).unwrap();
        let snapshot = store.snapshot().unwrap();
        let descriptor: TribleSet = snapshot.get(register.handle()).unwrap();

        assert_eq!(
            triblespace::core::collection::descriptor::parents(&descriptor).unwrap(),
            vec![source.handle()]
        );
        assert!(register.policy(&snapshot).is_err());
    }

    #[test]
    fn facts_ahead_of_status_register_do_not_admit_unknown_winners() {
        pollster::block_on(async {
            let signer = SigningKey::from_bytes(&[14; 32]);
            let goal = genid().id;
            let unseen_goal = genid().id;
            let initial = status_fragment(goal, "todo", None, at(1)).unwrap();
            let initial_id = initial.root().unwrap();
            let next = status_fragment(goal, "done", None, at(2)).unwrap();
            let next_id = next.root().unwrap();
            let unseen = status_fragment(unseen_goal, "doing", None, at(3)).unwrap();
            let unseen_id = unseen.root().unwrap();
            let mut store = MemoryRepo::for_host(signer.verifying_key());
            let source = store
                .collection(
                    "status-lag",
                    crate::collection_names::private_policy(signer.verifying_key()),
                )
                .unwrap();
            let target = status_register_for_source(&mut store, source).unwrap();
            store.commit(source, &signer, initial).unwrap();
            let ready = store.maintain_attached(target, &signer).await.unwrap();
            let lagging = ready
                .attached(target)
                .unwrap()
                .view::<LwwIndex>()
                .unwrap()
                .query()
                .unwrap();
            store.commit(source, &signer, next + unseen).unwrap();
            let snapshot = store.snapshot().unwrap();
            let facts = snapshot
                .collection(source)
                .unwrap()
                .view::<TribleSet>()
                .unwrap();
            assert_eq!(
                crate::schemas::compass::latest_status_event(&facts, &lagging, goal)
                    .unwrap()
                    .0,
                initial_id
            );
            assert_eq!(
                crate::schemas::compass::latest_status_event(&facts, &lagging, unseen_goal),
                None
            );

            let ready = store.maintain_attached(target, &signer).await.unwrap();
            let advanced = ready
                .attached(target)
                .unwrap()
                .view::<LwwIndex>()
                .unwrap()
                .query()
                .unwrap();
            assert_eq!(
                crate::schemas::compass::latest_status_event(&facts, &advanced, goal)
                    .unwrap()
                    .0,
                next_id
            );
            assert_eq!(
                crate::schemas::compass::latest_status_event(&facts, &advanced, unseen_goal)
                    .unwrap()
                    .0,
                unseen_id
            );
        });
    }

    /// Facts an open-world union can hold, each of which a reader of the whole
    /// collection once refused. Every one is now read: the view comes back and
    /// holds every fact that was committed.
    #[test]
    fn open_world_collection_facts_are_read_never_refused() {
        fn text(fragment: &mut Fragment, value: &str) -> TextHandle {
            fragment.put::<blobencodings::UTF8String, _>(value.to_owned())
        }
        let wide: IntervalValue = (Epoch::from_unix_seconds(1.0), Epoch::from_unix_seconds(2.0))
            .try_to_inline()
            .unwrap();
        let mut cases: Vec<(&str, Fragment)> = Vec::new();

        let goal = genid().id;
        let mut two_titles = Fragment::empty();
        let (alpha, beta) = (
            text(&mut two_titles, "alpha"),
            text(&mut two_titles, "beta"),
        );
        two_titles += entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: alpha,
            metadata::created_at: at(1),
        };
        two_titles += entity! { ExclusiveId::force_ref(&goal) @ board::title: beta };
        cases.push(("a goal with two titles", two_titles));

        let goal = genid().id;
        let mut two_times = Fragment::empty();
        let title = text(&mut two_times, "twice created");
        two_times += entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: title,
            metadata::created_at: at(1),
        };
        two_times += entity! { ExclusiveId::force_ref(&goal) @ metadata::created_at: at(2) };
        cases.push(("a goal with two creation times", two_times));

        let goal = genid().id;
        let mut two_parents = Fragment::empty();
        let title = text(&mut two_parents, "two parents");
        two_parents += entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: title,
            metadata::created_at: at(1),
            board::parent: &genid().id,
        };
        two_parents += entity! { ExclusiveId::force_ref(&goal) @ board::parent: &genid().id };
        cases.push(("a goal with two parents", two_parents));

        let goal = genid().id;
        let mut interval = Fragment::empty();
        let title = text(&mut interval, "wide");
        interval += entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: title,
            metadata::created_at: wide,
        };
        cases.push(("a goal created over an interval", interval));

        let goal = genid().id;
        let mut extra = Fragment::empty();
        let title = text(&mut extra, "carries a status");
        let doing = text(&mut extra, "doing");
        extra += entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: title,
            metadata::created_at: at(1),
            board::status: doing,
        };
        cases.push((
            "a goal carrying a Compass attribute outside its core",
            extra,
        ));

        let entity = genid().id;
        let mut two_kinds = Fragment::empty();
        let title = text(&mut two_kinds, "goal and note");
        two_kinds += entity! { ExclusiveId::force_ref(&entity) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: title,
            metadata::created_at: at(1),
        };
        two_kinds += entity! { ExclusiveId::force_ref(&entity) @ metadata::tag: &KIND_NOTE_ID };
        cases.push(("an entity with two Compass kinds", two_kinds));

        let note = genid().id;
        let mut two_bodies = Fragment::empty();
        let (first, second) = (
            text(&mut two_bodies, "first"),
            text(&mut two_bodies, "second"),
        );
        two_bodies += entity! { ExclusiveId::force_ref(&note) @
            metadata::tag: &KIND_NOTE_ID,
            board::task: &genid().id,
            board::note: first,
            metadata::created_at: at(1),
            board::by: &genid().id,
        };
        two_bodies += entity! { ExclusiveId::force_ref(&note) @
            board::note: second,
            board::task: &genid().id,
            board::by: &genid().id,
        };
        cases.push(("a note with two bodies, goals and authors", two_bodies));

        let note = genid().id;
        let mut no_body = Fragment::empty();
        no_body += entity! { ExclusiveId::force_ref(&note) @
            metadata::tag: &KIND_NOTE_ID,
            board::task: &genid().id,
            metadata::created_at: at(1),
        };
        cases.push(("a note whose body has not arrived", no_body));

        let event = genid().id;
        let mut two_words = Fragment::empty();
        let (todo, done) = (text(&mut two_words, "todo"), text(&mut two_words, "done"));
        two_words += entity! { ExclusiveId::force_ref(&event) @
            metadata::tag: &KIND_STATUS_ID,
            board::status_of: &genid().id,
            board::status: todo,
            metadata::created_at: at(1),
        };
        two_words += entity! { ExclusiveId::force_ref(&event) @
            board::status: done,
            board::status_of: &genid().id,
            metadata::created_at: at(2),
        };
        cases.push(("a status event with two words, goals and times", two_words));

        let event = genid().id;
        let goal = genid().id;
        let mut reflexive = entity! { ExclusiveId::force_ref(&event) @
            metadata::tag: &KIND_PRIORITIZE_ID,
            board::higher: &goal,
            board::lower: &goal,
            metadata::created_at: at(1),
        };
        reflexive += entity! { ExclusiveId::force_ref(&event) @ board::higher: &genid().id };
        cases.push((
            "a priority event relating a goal to itself, twice higher",
            reflexive,
        ));

        let goal = genid().id;
        let mut absent = Fragment::empty();
        let title = text(&mut absent, "title not resident");
        absent += entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: title,
            metadata::created_at: at(1),
        };
        absent.blobs_mut().keep([]);
        cases.push(("a goal whose title text is not resident", absent));

        let mut refused = Vec::new();
        for (case, fragment) in cases {
            let file = tempfile::NamedTempFile::new().unwrap();
            let signer = SigningKey::from_bytes(&[31; 32]);
            let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
            let expected = fragment.facts().clone();
            commit_collection(&mut pile, &signer, fragment).unwrap();
            match materialize_collection(&mut pile, &signer) {
                Ok((facts, _)) => assert!(
                    expected.difference(&facts).is_empty(),
                    "{case}: the view lost committed facts"
                ),
                Err(error) => refused.push(format!("{case}: {error:#}")),
            }
            pile.close().unwrap();
        }
        assert!(refused.is_empty(), "refused:\n{}", refused.join("\n"));
    }

    /// A priority event whose time does not decode is not an event this
    /// reader can order, so it is skipped instead of panicking the board.
    #[test]
    fn a_priority_event_whose_time_does_not_decode_is_skipped() {
        let mut raw = at(2_000).raw;
        raw[16..].copy_from_slice(&at(1_000).raw[16..]);
        let inverted = IntervalValue::new(raw);
        let decoded: Result<(i128, i128), _> = inverted.try_from_inline();
        assert!(decoded.is_err());
        let (high, low) = (genid().id, genid().id);
        let mut facts = TribleSet::new();
        facts += entity! {
            metadata::tag: &KIND_PRIORITIZE_ID,
            board::higher: &high,
            board::lower: &low,
            metadata::created_at: inverted,
        };
        facts += priority_fragment(low, high, true, at(1));
        assert_eq!(active_priority_edges(&facts), BTreeSet::from([(low, high)]));
    }

    #[test]
    fn equal_time_priority_events_use_entity_id_not_insertion_order() {
        let high = genid().id;
        let low = genid().id;
        let activate = priority_fragment(high, low, true, at(7));
        let deactivate = priority_fragment(high, low, false, at(7));
        let activate_id = activate.root().unwrap();
        let deactivate_id = deactivate.root().unwrap();

        let mut left = TribleSet::new();
        left += activate.clone();
        left += deactivate.clone();
        let mut right = TribleSet::new();
        right += deactivate;
        right += activate;

        assert_eq!(active_priority_edges(&left), active_priority_edges(&right));
        assert_eq!(
            active_priority_edges(&left).contains(&(high, low)),
            activate_id > deactivate_id
        );
    }

    #[test]
    fn priority_ranks_are_partial_order_tiers() {
        let highest = Id::new([1; 16]).unwrap();
        let middle = Id::new([2; 16]).unwrap();
        let lowest = Id::new([3; 16]).unwrap();
        let unrelated = Id::new([4; 16]).unwrap();
        let edges = BTreeSet::from([(highest, middle), (middle, lowest)]);

        let ranks = priority_ranks([highest, middle, lowest, unrelated], &edges);

        assert_eq!(ranks[&highest], 0);
        assert_eq!(ranks[&unrelated], 0);
        assert_eq!(ranks[&middle], 1);
        assert_eq!(ranks[&lowest], 2);
    }

    #[test]
    fn goal_priority_edges_include_child_before_parent() {
        let parent = Id::new([1; 16]).unwrap();
        let child = Id::new([2; 16]).unwrap();
        let mut facts = TribleSet::new();
        facts += entity! { ExclusiveId::force_ref(&parent) @ metadata::tag: &KIND_GOAL_ID };
        facts += entity! { ExclusiveId::force_ref(&child) @
            metadata::tag: &KIND_GOAL_ID,
            board::parent: &parent,
        };

        assert!(goal_priority_edges(&facts).contains(&(child, parent)));
    }

    #[test]
    fn priority_cycle_check_follows_transitive_edges() {
        let high = Id::new([1; 16]).unwrap();
        let middle = Id::new([2; 16]).unwrap();
        let low = Id::new([3; 16]).unwrap();
        let edges = BTreeSet::from([(high, middle), (middle, low)]);

        assert!(would_create_priority_cycle(&edges, low, high));
        assert!(!would_create_priority_cycle(&edges, high, low));
    }

    #[test]
    fn exact_status_replay_has_one_intrinsic_identity() {
        let goal = genid().id;
        let first = status_fragment(goal, "Doing", None, at(11)).unwrap();
        let second = status_fragment(goal, "doing", None, at(11)).unwrap();
        assert_eq!(first.root(), second.root());
        assert_eq!(first.facts(), second.facts());
    }

    #[test]
    fn exact_goal_and_note_retries_converge() {
        let (first_goal, first_goal_id) =
            goal_fragment("same", vec!["one".into()], None, at(4)).unwrap();
        let (second_goal, second_goal_id) =
            goal_fragment("same", vec!["one".into()], None, at(4)).unwrap();
        assert_eq!(first_goal.root(), Some(first_goal_id));
        assert_eq!(second_goal.root(), Some(second_goal_id));
        assert_eq!(first_goal_id, second_goal_id);
        assert_eq!(first_goal, second_goal);

        let (first_note, first_note_id) = note_fragment(
            first_goal_id,
            "same",
            vec!["one".into()],
            vec!["wiki:abc".into()],
            vec![],
            None,
            at(5),
        )
        .unwrap();
        let (second_note, second_note_id) = note_fragment(
            second_goal_id,
            "same",
            vec!["one".into()],
            vec!["wiki:abc".into()],
            vec![],
            None,
            at(5),
        )
        .unwrap();
        assert_eq!(first_note.root(), Some(first_note_id));
        assert_eq!(second_note.root(), Some(second_note_id));
        assert_eq!(first_note_id, second_note_id);
        assert_eq!(first_note, second_note);
    }

    #[test]
    fn creation_time_distinguishes_repeated_goal_and_note_occurrences() {
        let (_, first_goal) = goal_fragment("same", vec![], None, at(4)).unwrap();
        let (_, second_goal) = goal_fragment("same", vec![], None, at(5)).unwrap();
        assert_ne!(first_goal, second_goal);

        let (_, first_note) =
            note_fragment(first_goal, "same", vec![], vec![], vec![], None, at(6)).unwrap();
        let (_, second_note) =
            note_fragment(first_goal, "same", vec![], vec![], vec![], None, at(7)).unwrap();
        assert_ne!(first_note, second_note);
    }

    #[test]
    fn extrinsic_goal_and_note_ids_are_substitutable_in_queries() {
        let goal = genid().id;
        let note = genid().id;
        let mut records =
            replay::goal_fragment(goal, "preserved", vec!["legacy".into()], None, at(4)).unwrap();
        records += replay::note_fragment(
            note,
            goal,
            "preserved note",
            vec![],
            vec![],
            vec![],
            None,
            at(5),
        )
        .unwrap();

        assert_eq!(goal_ids(records.facts()), BTreeSet::from([goal]));
        assert_eq!(note_ids(records.facts()), BTreeSet::from([note]));
        assert_eq!(
            find!(task: Id, pattern!(records.facts(), [{ note @ board::task: ?task }]))
                .collect::<Vec<_>>(),
            vec![goal]
        );
    }
}
