//! Collection-native standing intentions.
//!
//! Habits are pulled: reading this model computes what is due now, and never
//! writes a notification queue. Each immutable definition, completion, and
//! state assertion is published as one complete [`Fragment`] to one fixed
//! collection. Pause/resume assertions form a predecessor DAG, so set union
//! keeps concurrent state visible instead of letting time or append order pick
//! a winner.
//!
//! Every reader here is an open-world typed query at its point of use. A fact
//! a reader does not model is ignored, a record it cannot decode is skipped,
//! and several values for one attribute are each read: the schema language has
//! no cardinality, and a union of piles can always supply a second value. No
//! reader refuses a view, and none first loads the collection into a catalog.

pub mod cli;
pub mod mcp;
pub mod operations;
mod render;

pub use operations::{
    AddedHabit, EvaluatedHabit, HabitCheck, HabitList, HabitObservation, HabitOccurrence,
    HabitStateChange, Habits,
};

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anybytes::View;
use anyhow::{anyhow, bail, Context, Result};
use triblespace::core::collection::{CollectionCommit, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::BlobStoreGet;
use triblespace::macros::{entity, find, pattern};
use triblespace::prelude::*;
use triblespace_paths::{PathExpr, PathIndex, Step};

#[cfg(test)]
use crate::collection_names::open_configured;
use crate::schemas::habit::{
    attrs, Condition, DEFAULT_SCOPE_ID, KIND_DONE_ID, KIND_HABIT_ID, KIND_STATE_ID, SCRIPT_TOKEN,
    STATE_ACTIVE, STATE_PAUSED,
};
use crate::storage::Storage;

pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type ScriptHandle = Inline<inlineencodings::Handle<blobencodings::RawBytes>>;
pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;

/// Content identity of the Habit collection this pile roots.
pub fn collection_handle(
    pile: &Path,
    key: Option<&Path>,
) -> Result<triblespace::core::collection::records::CollectionHandle> {
    collection_handle_with_storage(&Storage::new(pile.to_owned(), key.map(Path::to_owned)))
}

/// Select the collection through an explicitly owned storage lifetime.
pub fn collection_handle_with_storage(
    storage: &Storage,
) -> Result<triblespace::core::collection::records::CollectionHandle> {
    storage.with_store(|pile, signer, runtime| {
        storage
            .open_collection_read(pile, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)
            .map(|collection| collection.handle())
            .context("open Habit collection")
    })
}

/// One pile-resident executable carried by a standing intention.
///
/// The handle is the content hash, so it is simultaneously the pile address,
/// the identity recorded in the habit's own facts, and the local cache key.
/// Editing the script therefore yields a different habit at a different cache
/// path: no stale copy is reachable, because nothing is ever overwritten.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Script {
    pub handle: ScriptHandle,
    pub bytes: Vec<u8>,
}

/// Content hash a script will be addressed by, computed without publishing it.
///
/// Authoring surfaces need to name the attachment before the collection has
/// been written, and the address is a pure function of the bytes.
pub fn script_digest(bytes: &[u8]) -> String {
    let mut fragment = Fragment::empty();
    let handle = fragment.put::<blobencodings::RawBytes, _>(bytes.to_vec());
    hex::encode(handle.raw)
}

impl Script {
    /// Lowercase hex content hash — the cache key and the display form.
    pub fn digest(&self) -> String {
        hex::encode(self.handle.raw)
    }

    /// First eight hex digits, for one-line listings.
    pub fn short_digest(&self) -> String {
        self.digest()[..8].to_owned()
    }

    fn validate_identity(&self) -> std::result::Result<(), String> {
        let expected = self.digest();
        let actual = script_digest(&self.bytes);
        if actual != expected {
            return Err(format!(
                "Habit script handle {expected} does not address its carried bytes (actual {actual})"
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Habit {
    pub id: Id,
    /// Display name. Deliberately **not** an identity: several definitions may
    /// carry the same label, and none of them owns it.
    pub label: String,
    /// Author-written source (`every …`, `daily at …`, or `when …`).
    pub condition: String,
    pub nudge: String,
    /// Executable the definition carries, when the condition names `@script`.
    pub script: Option<Script>,
    /// Definitions this one replaces, by intrinsic id.
    pub supersedes: Vec<Id>,
    /// Exact notification targets. An empty set addresses everyone.
    pub personas: Vec<Id>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub id: Id,
    pub habit: Id,
    pub completed_at: IntervalValue,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DeclaredState {
    Active,
    Paused,
}

impl DeclaredState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => STATE_ACTIVE,
            Self::Paused => STATE_PAUSED,
        }
    }

    /// The state whose text has this content handle, if any. A state word
    /// this reader does not know is not a `DeclaredState`.
    fn from_handle(value: TextHandle) -> Option<Self> {
        [Self::Active, Self::Paused]
            .into_iter()
            .find(|state| state.as_str().to_owned().to_blob().get_handle() == value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateAssertion {
    pub id: Id,
    pub habit: Id,
    pub state: DeclaredState,
    pub predecessors: Vec<Id>,
    pub asserted_at: IntervalValue,
}

/// Fork-visible activation state for one standing intention.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Activation {
    /// No assertion is also active; the vector is then empty.
    Active(Vec<StateAssertion>),
    Paused(Vec<StateAssertion>),
    /// Maximal assertions disagree. Every head remains available to a later
    /// reconciliation assertion.
    Forked(Vec<StateAssertion>),
}

impl Activation {
    pub fn head_ids(&self) -> Vec<Id> {
        match self {
            Self::Active(heads) | Self::Paused(heads) | Self::Forked(heads) => {
                heads.iter().map(|head| head.id).collect()
            }
        }
    }

    pub fn declared(&self) -> Option<DeclaredState> {
        match self {
            Self::Active(_) => Some(DeclaredState::Active),
            Self::Paused(_) => Some(DeclaredState::Paused),
            Self::Forked(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HabitRow {
    pub id: Id,
    pub label: String,
    pub condition: String,
    pub nudge: String,
    pub script: Option<Script>,
    pub activation: Activation,
    /// Complete set of completion points, represented in integral TAI seconds.
    pub completed_at: Vec<i64>,
}

impl HabitRow {
    pub fn last_done(&self) -> Option<i64> {
        self.completed_at.iter().copied().max()
    }

    /// Earliest wall-clock second at which the current completion set can stop
    /// cooling. It is scheduling information, not a state winner.
    pub fn next_cooldown_at(&self) -> Result<Option<i64>, String> {
        let condition = Condition::parse(&self.condition)?;
        Ok(self
            .last_done()
            .map(|done| done.saturating_add(condition.cooldown_secs)))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum State {
    Due,
    Cooling,
    Waiting,
    Paused,
    Forked(Vec<(Id, DeclaredState)>),
    Unparseable(String),
    Failed(String),
}

impl State {
    pub const fn is_due(&self) -> bool {
        matches!(self, Self::Due)
    }

    pub const fn word(&self) -> &'static str {
        match self {
            Self::Due => "DUE",
            Self::Cooling => "cooling",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Forked(_) => "FORKED",
            Self::Unparseable(_) => "BROKEN",
            Self::Failed(_) => "ERROR",
        }
    }
}

fn canonical_required(value: impl Into<String>, field: &str) -> Result<String> {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{field} is empty");
    }
    if trimmed.bytes().any(|byte| byte == 0) {
        bail!("{field} contains a NUL byte");
    }
    Ok(trimmed.to_owned())
}

fn canonical_label(value: impl Into<String>) -> Result<String> {
    canonical_required(value, "Habit label")
}

fn sorted_ids(values: impl IntoIterator<Item = Id>) -> Vec<Id> {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort_unstable();
    values.dedup();
    values
}

fn point_interval(value: IntervalValue, field: &str) -> Result<()> {
    let (lower, upper): (i128, i128) = value
        .try_from_inline()
        .map_err(|error| anyhow!("decode {field}: {error:?}"))?;
    if lower != upper {
        bail!("{field} must be a point interval");
    }
    Ok(())
}

/// The integral TAI second at which an interval starts, if the value decodes.
///
/// Queries are types: a value that does not decode as an interval (an
/// inverted pair) is not a time this reader can use, so callers skip it
/// rather than fail. A wider interval is read at its lower bound, the earliest
/// moment it admits. Habits and Orient read completion times through this one
/// function, so the two cannot disagree about what a completion says.
pub fn interval_second(value: IntervalValue) -> Option<i64> {
    let (lower, _): (i128, i128) = value.try_from_inline().ok()?;
    i64::try_from(lower / 1_000_000_000).ok()
}

fn habit_record(
    label: TextHandle,
    condition: TextHandle,
    nudge: TextHandle,
    script: Option<ScriptHandle>,
    supersedes: &[Id],
    personas: &[Id],
) -> Fragment {
    entity! { _ @
        metadata::tag: &KIND_HABIT_ID,
        attrs::label: label,
        attrs::condition: condition,
        attrs::nudge: nudge,
        attrs::script?: script,
        attrs::persona*: personas.iter(),
        metadata::supersedes*: supersedes.iter(),
    }
}

/// A condition and its attachment must agree in both directions.
///
/// Either mismatch describes a habit that cannot do what it says: a `@script`
/// with nothing to run, or an executable no condition will ever reach. Both are
/// caught where the definition is built, not where it silently stops firing.
fn check_script_agreement(condition: &Condition, has_script: bool, subject: &str) -> Result<()> {
    match (condition.uses_script(), has_script) {
        (true, false) => bail!(
            "{subject} condition names `{SCRIPT_TOKEN}` but carries no script; \
             attach one with `--script <path>`"
        ),
        (false, true) => bail!(
            "{subject} carries a script no condition reaches; \
             write the condition as `when {SCRIPT_TOKEN} <args>`"
        ),
        _ => Ok(()),
    }
}

fn completion_record(habit: Id, completed_at: IntervalValue) -> Fragment {
    entity! { _ @
        metadata::tag: &KIND_DONE_ID,
        attrs::of: &habit,
        metadata::created_at: completed_at,
    }
}

fn state_record(
    habit: Id,
    state: DeclaredState,
    predecessors: &[Id],
    asserted_at: IntervalValue,
) -> Fragment {
    entity! { _ @
        metadata::tag: &KIND_STATE_ID,
        attrs::of: &habit,
        attrs::state: state.as_str(),
        metadata::supersedes*: predecessors.iter(),
        metadata::created_at: asserted_at,
    }
}

/// Build one complete, intrinsically identified standing intention.
///
/// When `script` is present its bytes travel inside the fragment, exactly as
/// the condition and nudge payloads do. That is what makes the definition
/// self-contained: whoever receives this one collection commit receives the
/// executable too, and does not have to already hold some other collection —
/// or some machine-local path — for the intention to mean anything.
///
/// `supersedes` names the definitions this one replaces, by intrinsic id. The
/// edge is part of the record, so two windows authoring the same revision of
/// the same intention produce byte-identical facts and therefore the same id:
/// they converge on one definition instead of forking. Concurrent authoring
/// *without* an edge is not an error — it is two live definitions that happen
/// to share a display name, which is the truth, and a later revision citing
/// both resolves it.
///
/// `personas` is an optional set of exact notification targets. No targets
/// means global; it never defaults to the writer or the ambient persona.
pub fn habit_fragment(
    label: impl Into<String>,
    condition: impl Into<String>,
    nudge: impl Into<String>,
    script: Option<Vec<u8>>,
    supersedes: &[Id],
    personas: &[Id],
) -> Result<(Fragment, Id)> {
    let label = canonical_label(label)?;
    let condition = canonical_required(condition, "Habit condition")?;
    let parsed = Condition::parse(&condition).map_err(|error| anyhow!(error))?;
    let nudge = canonical_required(nudge, "Habit nudge")?;
    if let Some(bytes) = &script {
        if bytes.is_empty() {
            bail!("Habit script is empty");
        }
    }
    check_script_agreement(&parsed, script.is_some(), &format!("Habit {label:?}"))?;

    let supersedes = sorted_ids(supersedes.iter().copied());
    let personas = sorted_ids(personas.iter().copied());
    let mut fragment = Fragment::empty();
    let label = fragment.put(label);
    let condition = fragment.put(condition);
    let nudge = fragment.put(nudge);
    let script = script.map(|bytes| fragment.put::<blobencodings::RawBytes, _>(bytes));
    let record = habit_record(label, condition, nudge, script, &supersedes, &personas);
    let id = record
        .root()
        .expect("Habit definition has one intrinsic root");
    fragment += record;
    Ok((fragment, id))
}

/// Build one complete intrinsic completion occurrence.
pub fn completion_fragment(habit: Id, completed_at: IntervalValue) -> Result<(Fragment, Id)> {
    point_interval(completed_at, "Habit completion time")?;
    let fragment = completion_record(habit, completed_at);
    let id = fragment
        .root()
        .expect("Habit completion has one intrinsic root");
    Ok((fragment, id))
}

/// Build one complete intrinsic state assertion. Callers normally cite every
/// currently maximal assertion; racing callers may still create a visible
/// fork, which a later assertion can reconcile by citing all heads.
pub fn state_fragment(
    habit: Id,
    state: DeclaredState,
    predecessors: &[Id],
    asserted_at: IntervalValue,
) -> Result<(Fragment, Id)> {
    point_interval(asserted_at, "Habit state assertion time")?;
    let predecessors = sorted_ids(predecessors.iter().copied());
    let fragment = state_record(habit, state, &predecessors, asserted_at);
    let id = fragment
        .root()
        .expect("Habit state assertion has one intrinsic root");
    Ok((fragment, id))
}

fn ids_of_kind<P>(facts: &P, kind: Id) -> BTreeSet<Id>
where
    P: TriblePattern + ?Sized,
{
    find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: kind }])).collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RawHabit {
    id: Id,
    label: TextHandle,
    condition: TextHandle,
    nudge: TextHandle,
    script: Option<ScriptHandle>,
    supersedes: Vec<Id>,
    personas: Vec<Id>,
}

fn load_text<Reader>(reader: &Reader, handle: TextHandle, field: &str) -> Result<String>
where
    Reader: BlobStoreGet + ?Sized,
{
    let value: View<str> = reader
        .get(handle)
        .with_context(|| format!("read Habit {field} payload {}", hex::encode(handle.raw)))?;
    Ok(value.to_string())
}

fn load_script<Reader>(reader: &Reader, handle: ScriptHandle, habit: Id) -> Result<Script>
where
    Reader: BlobStoreGet + ?Sized,
{
    let bytes: anybytes::Bytes = reader.get(handle).map_err(|_| {
        anyhow!(
            "Habit {habit:x} script blob {} is not in this snapshot",
            hex::encode(handle.raw)
        )
    })?;
    Ok(Script {
        handle,
        bytes: bytes.to_vec(),
    })
}

/// Project every complete Habit definition understood by this reader.
///
/// This is an open-world relation projection, not a catalog validation pass.
/// Unknown facts and incomplete or undecodable records do not inhabit the
/// result type. If one entity has several values for a modeled attribute, each
/// complete tuple remains visible instead of imposing an unenforceable
/// cardinality rule. Repeated `supersedes` edges remain one set on each tuple.
fn projected_definitions<P>(facts: &P) -> Vec<RawHabit>
where
    P: TriblePattern + ?Sized,
{
    let mut definitions = Vec::new();
    for (id, label, condition, nudge) in find!(
        (id: Id, label: TextHandle, condition: TextHandle, nudge: TextHandle),
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: ?label,
            attrs::condition: ?condition,
            attrs::nudge: ?nudge,
        }])
    ) {
        let supersedes = sorted_ids(find!(
            predecessor: Id,
            pattern!(facts, [{ id @ metadata::supersedes: ?predecessor }])
        ));
        let personas = sorted_ids(find!(
            persona: Id,
            pattern!(facts, [{ id @ attrs::persona: ?persona }])
        ));
        let mut scripts: Vec<ScriptHandle> = find!(
            script: ScriptHandle,
            pattern!(facts, [{ id @ attrs::script: ?script }])
        )
        .collect();
        scripts.sort_by_key(|script| script.raw);
        scripts.dedup();

        if scripts.is_empty() {
            definitions.push(RawHabit {
                id,
                label,
                condition,
                nudge,
                script: None,
                supersedes,
                personas,
            });
        } else {
            definitions.extend(scripts.into_iter().map(|script| RawHabit {
                id,
                label,
                condition,
                nudge,
                script: Some(script),
                supersedes: supersedes.clone(),
                personas: personas.clone(),
            }));
        }
    }

    definitions.sort_by(|left, right| {
        (
            left.id,
            left.label.raw,
            left.condition.raw,
            left.nudge.raw,
            left.script.map(|script| script.raw),
            &left.supersedes,
        )
            .cmp(&(
                right.id,
                right.label.raw,
                right.condition.raw,
                right.nudge.raw,
                right.script.map(|script| script.raw),
                &right.supersedes,
            ))
    });
    definitions.dedup();
    definitions
}

/// Every entity with a complete, decodable Habit-definition projection.
pub fn definition_ids<P>(facts: &P) -> BTreeSet<Id>
where
    P: TriblePattern + ?Sized,
{
    find!(
        id: Id,
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: _?label,
            attrs::condition: _?condition,
            attrs::nudge: _?nudge,
        }])
    )
    .collect()
}

/// IDs which a complete, understood Habit definition explicitly supersedes.
///
/// The predecessor itself need not be present. A successor may arrive first,
/// and the edge will retire its predecessor monotonically if that definition
/// appears later.
pub fn superseded_definition_ids<P>(facts: &P) -> BTreeSet<Id>
where
    P: TriblePattern + ?Sized,
{
    find!(
        predecessor: Id,
        pattern!(facts, [{ _?successor @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: _?label,
            attrs::condition: _?condition,
            attrs::nudge: _?nudge,
            metadata::supersedes: ?predecessor,
        }])
    )
    .collect()
}

/// Whether an understood successor retires this definition in this view.
pub fn is_superseded<P>(facts: &P, id: Id) -> bool
where
    P: TriblePattern + ?Sized,
{
    superseded_definition_ids(facts).contains(&id)
}

fn decode_definitions<Reader>(
    reader: &Reader,
    projected: impl IntoIterator<Item = RawHabit>,
) -> Result<Vec<Habit>>
where
    Reader: BlobStoreGet + ?Sized,
{
    let mut texts = HashMap::<[u8; 32], String>::new();
    let mut definitions = Vec::new();
    for raw in projected {
        let label = match texts.get(&raw.label.raw) {
            Some(value) => value.clone(),
            None => {
                let value = load_text(reader, raw.label, "label")?;
                texts.insert(raw.label.raw, value.clone());
                value
            }
        };
        let condition = match texts.get(&raw.condition.raw) {
            Some(value) => value.clone(),
            None => {
                let value = load_text(reader, raw.condition, "condition")?;
                texts.insert(raw.condition.raw, value.clone());
                value
            }
        };
        let nudge = match texts.get(&raw.nudge.raw) {
            Some(value) => value.clone(),
            None => {
                let value = load_text(reader, raw.nudge, "nudge")?;
                texts.insert(raw.nudge.raw, value.clone());
                value
            }
        };
        let script = raw
            .script
            .map(|handle| load_script(reader, handle, raw.id))
            .transpose()?;
        definitions.push(Habit {
            id: raw.id,
            label,
            condition,
            nudge,
            script,
            supersedes: raw.supersedes,
            personas: raw.personas,
        });
    }
    definitions.sort_by(|left, right| {
        (&left.label, left.id, &left.condition, &left.nudge).cmp(&(
            &right.label,
            right.id,
            &right.condition,
            &right.nudge,
        ))
    });
    Ok(definitions)
}

/// Decode every complete Habit definition through a typed query.
///
/// Payloads are read from the same immutable store snapshot which owns the
/// queried archive. Missing attachments remain loud because an intention that
/// silently stops firing is worse than one which names its missing content.
pub fn definitions<Reader, P>(reader: &Reader, facts: &P) -> Result<Vec<Habit>>
where
    Reader: BlobStoreGet + ?Sized,
    P: TriblePattern + ?Sized,
{
    decode_definitions(reader, projected_definitions(facts))
}

/// Decode only definitions which are maximal in the explicit supersession DAG.
pub fn live_definitions<Reader, P>(reader: &Reader, facts: &P) -> Result<Vec<Habit>>
where
    Reader: BlobStoreGet + ?Sized,
    P: TriblePattern + ?Sized,
{
    let superseded = superseded_definition_ids(facts);
    decode_definitions(
        reader,
        projected_definitions(facts)
            .into_iter()
            .filter(|habit| !superseded.contains(&habit.id)),
    )
}

/// Project the recognized state assertions governing one Habit.
///
/// Unknown state words do not inhabit `DeclaredState` and are skipped like any
/// other failed typed projection. Missing predecessors are ordinary in a
/// partial view: the successor already proves that they cannot be maximal if
/// they arrive later.
pub fn state_assertions<P>(facts: &P, habit: Id) -> Vec<StateAssertion>
where
    P: TriblePattern + ?Sized,
{
    let mut assertions = Vec::new();
    for (id, state, asserted_at) in find!(
        (id: Id, state: TextHandle, asserted_at: IntervalValue),
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_STATE_ID,
            attrs::of: &habit,
            attrs::state: ?state,
            metadata::created_at: ?asserted_at,
        }])
    ) {
        let Some(state) = DeclaredState::from_handle(state) else {
            continue;
        };
        assertions.push(StateAssertion {
            id,
            habit,
            state,
            predecessors: sorted_ids(find!(
                predecessor: Id,
                pattern!(facts, [{ id @ metadata::supersedes: ?predecessor }])
            )),
            asserted_at,
        });
    }
    assertions.sort_by(|left, right| {
        (
            left.id,
            left.state,
            left.asserted_at.raw,
            &left.predecessors,
        )
            .cmp(&(
                right.id,
                right.state,
                right.asserted_at.raw,
                &right.predecessors,
            ))
    });
    assertions.dedup();
    assertions
}

/// Resolve the maximal, fork-visible activation assertions for one Habit.
///
/// A track with no maximal assertion (a predecessor cycle, which opaque ids
/// can form and intrinsic ones cannot) declares nothing, exactly like an
/// empty track: the intention is active with no heads until an assertion
/// citing the cycle reconciles it. Orient reads such a track the same way.
pub fn activation<P>(facts: &P, habit: Id) -> Activation
where
    P: TriblePattern + ?Sized,
{
    let assertions = state_assertions(facts, habit);
    let superseded: BTreeSet<Id> = assertions
        .iter()
        .flat_map(|assertion| assertion.predecessors.iter().copied())
        .collect();
    let heads: Vec<_> = assertions
        .into_iter()
        .filter(|assertion| !superseded.contains(&assertion.id))
        .collect();
    let Some(first) = heads.first().map(|head| head.state) else {
        return Activation::Active(Vec::new());
    };
    if heads.iter().all(|head| head.state == first) {
        return match first {
            DeclaredState::Active => Activation::Active(heads),
            DeclaredState::Paused => Activation::Paused(heads),
        };
    }
    Activation::Forked(heads)
}

/// Project every understood completion occurrence for one Habit.
pub fn completions<P>(facts: &P, habit: Id) -> Vec<Completion>
where
    P: TriblePattern + ?Sized,
{
    let mut completions: Vec<_> = find!(
        (id: Id, completed_at: IntervalValue),
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_DONE_ID,
            attrs::of: &habit,
            metadata::created_at: ?completed_at,
        }])
    )
    .map(|(id, completed_at)| Completion {
        id,
        habit,
        completed_at,
    })
    .collect();
    completions.sort_by_key(|completion| (completion.id, completion.completed_at.raw));
    completions.dedup();
    completions
}

/// Revision ancestry of Habit definitions: the `supersedes+` relation between
/// complete, understood definitions.
///
/// A superseding definition is the same standing intention, revised, so its
/// history is the union of its predecessors' histories. That is derived here
/// at read time from the edges alone, never copied onto a successor when it is
/// written, so a predecessor's completion that arrives later (from another
/// pile, say) counts the moment it arrives. Branches and merges are ordinary
/// paths, and the fixpoint terminates on cycles, which opaque ids can form.
pub fn revision_ancestry<P>(facts: &P) -> Result<PathIndex>
where
    P: TriblePattern + ?Sized,
{
    let edges: TribleSet = find!(
        (successor: Id, predecessor: Id),
        pattern!(facts, [{ ?successor @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: _?label,
            attrs::condition: _?condition,
            attrs::nudge: _?nudge,
            metadata::supersedes: ?predecessor,
        }])
    )
    .map(|(successor, predecessor)| {
        let predecessor: Inline<inlineencodings::GenId> = predecessor.to_inline();
        Trible::force(&successor, &metadata::supersedes.id(), &predecessor)
    })
    .collect();
    let supersedes_plus = PathExpr::from(Step::Forward(metadata::supersedes.id().into()))
        .plus()
        .compile();
    PathIndex::from_tribles(supersedes_plus, edges.iter())
        .map_err(|error| anyhow!("materialize Habit revision ancestry: {error}"))
}

/// Every understood completion of the standing intention `habit`: its own,
/// and each one recorded against a definition it transitively supersedes in
/// `ancestry` (from [`revision_ancestry`] over the same facts). Each
/// occurrence keeps the definition it actually names in [`Completion::habit`].
pub fn standing_completions<P>(facts: &P, ancestry: &PathIndex, habit: Id) -> Vec<Completion>
where
    P: TriblePattern + ?Sized,
{
    let start: Inline<inlineencodings::GenId> = habit.to_inline();
    let mut completions = completions(facts, habit);
    completions.extend(
        find!(
            (id: Id, predecessor: Id, completed_at: IntervalValue),
            and!(
                ancestry.constraint(start, predecessor),
                pattern!(facts, [{ ?id @
                    metadata::tag: &KIND_DONE_ID,
                    attrs::of: ?predecessor,
                    metadata::created_at: ?completed_at,
                }])
            )
        )
        .map(|(id, habit, completed_at)| Completion {
            id,
            habit,
            completed_at,
        }),
    );
    completions
        .sort_by_key(|completion| (completion.id, completion.habit, completion.completed_at.raw));
    completions.dedup();
    completions
}

/// Build the command-facing rows for every live definition without first
/// materializing the full relation into a catalog shadow model. Completion
/// history is read across each definition's revision ancestry.
///
/// A definition with several values for one modeled attribute (two
/// conditions, say) yields one row per complete combination, all under the
/// one id. Each row is a reading of the same intention and is evaluated on its
/// own; the intention is due when any of its readings is. That needs no rule
/// for choosing between values, so no window can disagree about which won.
pub fn rows<Reader, P>(reader: &Reader, facts: &P) -> Result<Vec<HabitRow>>
where
    Reader: BlobStoreGet + ?Sized,
    P: TriblePattern + ?Sized,
{
    let ancestry = revision_ancestry(facts)?;
    let mut rows = Vec::new();
    for habit in live_definitions(reader, facts)? {
        let mut completed_at: Vec<i64> = standing_completions(facts, &ancestry, habit.id)
            .into_iter()
            .filter_map(|completion| interval_second(completion.completed_at))
            .collect();
        completed_at.sort_unstable();
        completed_at.dedup();
        rows.push(HabitRow {
            id: habit.id,
            label: habit.label,
            condition: habit.condition,
            nudge: habit.nudge,
            script: habit.script,
            activation: activation(facts, habit.id),
            completed_at,
        });
    }
    rows.sort_by(|left, right| (&left.label, left.id).cmp(&(&right.label, right.id)));
    Ok(rows)
}

/// What `habit check` observed in one view of the collection.
///
/// Counts, plus observations about facts this view holds. An observation is
/// never a verdict: every fact it names is ordinary in an open-world union,
/// and each one says how the ordinary readers treat it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Audit {
    /// Distinct entities with a complete definition projection.
    pub definitions: usize,
    /// Of those, the ones no complete successor supersedes.
    pub live: usize,
    /// Definitions carrying at least one script attachment.
    pub scripted: usize,
    /// Distinct completion occurrences.
    pub completions: usize,
    /// Distinct state assertions.
    pub assertions: usize,
    pub notes: Vec<String>,
}

/// Read the whole Habit relation in `facts` and report what it holds.
///
/// Every question is a query asked where it is needed; nothing is first
/// loaded into a catalog, and nothing here refuses a fact. Payloads are read
/// through `reader`, so an attachment that is not resident is reported, with
/// the definition and blob it belongs to.
pub fn audit<Reader, P>(reader: &Reader, facts: &P) -> Audit
where
    Reader: BlobStoreGet + ?Sized,
    P: TriblePattern + ?Sized,
{
    let mut notes = Vec::new();
    let known = definition_ids(facts);
    let superseded = superseded_definition_ids(facts);

    let unread = ids_of_kind(facts, KIND_HABIT_ID).difference(&known).count();
    if unread > 0 {
        notes.push(format!(
            "{unread} Habit-tagged entit(ies) carry no complete text label, condition and \
             nudge, so no reader takes them for definitions"
        ));
    }

    let mut scripted = 0;
    for &habit in &known {
        let labels: BTreeSet<[u8; 32]> = find!(
            value: TextHandle,
            pattern!(facts, [{ habit @ attrs::label: ?value }])
        )
        .map(|value| value.raw)
        .collect();
        let conditions: BTreeSet<[u8; 32]> = find!(
            value: TextHandle,
            pattern!(facts, [{ habit @ attrs::condition: ?value }])
        )
        .map(|value| value.raw)
        .collect();
        let nudges: BTreeSet<[u8; 32]> = find!(
            value: TextHandle,
            pattern!(facts, [{ habit @ attrs::nudge: ?value }])
        )
        .map(|value| value.raw)
        .collect();
        let scripts: BTreeSet<[u8; 32]> = find!(
            value: ScriptHandle,
            pattern!(facts, [{ habit @ attrs::script: ?value }])
        )
        .map(|value| value.raw)
        .collect();
        if !scripts.is_empty() {
            scripted += 1;
        }
        for (count, field) in [
            (labels.len(), "labels"),
            (conditions.len(), "conditions"),
            (nudges.len(), "nudges"),
            (scripts.len(), "scripts"),
        ] {
            if count > 1 {
                notes.push(format!(
                    "definition {habit:x} has {count} {field}; each combination is read as \
                     its own reading, and the intention is due when any reading is"
                ));
            }
        }
    }

    for raw in projected_definitions(facts) {
        let id = raw.id;
        let history = if superseded.contains(&id) {
            " (superseded)"
        } else {
            ""
        };
        let habit = match decode_definitions(reader, [raw]) {
            Ok(mut decoded) => decoded.remove(0),
            Err(error) => {
                notes.push(format!(
                    "definition {id:x}{history} cannot be read here: {error:#}"
                ));
                continue;
            }
        };
        match Condition::parse(&habit.condition) {
            Ok(condition) => {
                if let Err(error) = check_script_agreement(
                    &condition,
                    habit.script.is_some(),
                    &format!("definition {id:x}{history}"),
                ) {
                    notes.push(format!("{error:#}; `habit list` reports it as an error"));
                }
            }
            Err(error) => notes.push(format!(
                "definition {id:x}{history} condition {:?} does not parse ({error}); \
                 `habit list` shows it BROKEN",
                habit.condition
            )),
        }
    }

    // A completion or assertion may name a definition this view has not
    // received yet. One named only by a supersedes edge is already read
    // through the successor's ancestry, so it is not waiting on anything.
    let reachable = |habit: &Id| known.contains(habit) || superseded.contains(habit);
    let mut completions = BTreeSet::new();
    let mut waiting = BTreeMap::<Id, BTreeSet<Id>>::new();
    let mut untimed = BTreeSet::new();
    for (id, habit, completed_at) in find!(
        (id: Id, habit: Id, completed_at: IntervalValue),
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_DONE_ID,
            attrs::of: ?habit,
            metadata::created_at: ?completed_at,
        }])
    ) {
        completions.insert(id);
        if interval_second(completed_at).is_none() {
            untimed.insert(id);
        }
        if !reachable(&habit) {
            waiting.entry(habit).or_default().insert(id);
        }
    }
    for (habit, events) in &waiting {
        notes.push(format!(
            "{} completion(s) name {habit:x}, no known definition yet; they count once it \
             arrives",
            events.len()
        ));
    }
    for id in &untimed {
        notes.push(format!(
            "completion {id:x} has a time that does not decode as an interval; readers skip it"
        ));
    }

    let mut assertions = BTreeSet::new();
    let mut tracks = BTreeSet::new();
    let mut waiting = BTreeMap::<Id, BTreeSet<Id>>::new();
    let mut unknown = BTreeSet::new();
    for (id, habit, state) in find!(
        (id: Id, habit: Id, state: TextHandle),
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_STATE_ID,
            attrs::of: ?habit,
            attrs::state: ?state,
            metadata::created_at: _?asserted_at,
        }])
    ) {
        assertions.insert(id);
        tracks.insert(habit);
        if DeclaredState::from_handle(state).is_none() {
            unknown.insert(id);
        }
        if !reachable(&habit) {
            waiting.entry(habit).or_default().insert(id);
        }
    }
    for (habit, events) in &waiting {
        notes.push(format!(
            "{} state assertion(s) name {habit:x}, no known definition yet; they apply once \
             it arrives",
            events.len()
        ));
    }
    for id in &unknown {
        notes.push(format!(
            "state assertion {id:x} declares a state this reader does not know; readers skip it"
        ));
    }
    for habit in &tracks {
        let track: BTreeSet<Id> = state_assertions(facts, *habit)
            .iter()
            .map(|assertion| assertion.id)
            .collect();
        for assertion in state_assertions(facts, *habit) {
            for predecessor in &assertion.predecessors {
                if !track.contains(predecessor) {
                    notes.push(format!(
                        "state assertion {:x} cites predecessor {predecessor:x}, not in this \
                         view; it is already superseded if it arrives",
                        assertion.id
                    ));
                }
            }
        }
        if !track.is_empty() && activation(facts, *habit).head_ids().is_empty() {
            notes.push(format!(
                "state track for {habit:x} has no maximal assertion (a predecessor cycle); it \
                 declares nothing until an assertion cites it"
            ));
        }
    }

    Audit {
        definitions: known.len(),
        live: known.difference(&superseded).count(),
        scripted,
        completions: completions.len(),
        assertions: assertions.len(),
        notes,
    }
}

/// Publish one caller-constructed Habit fragment to the fixed collection.
///
/// Constructors in this module produce complete records by construction. This
/// ordinary write path deliberately performs no ambient catalog preflight and
/// does not re-hash a derived entity id: readers are open-world, so a fact
/// they cannot model is ignored rather than guarded against here.
pub fn publish(
    pile_path: &Path,
    key_path: Option<&Path>,
    fragment: Fragment,
) -> Result<CollectionCommit> {
    publish_with_storage(
        &Storage::new(pile_path.to_owned(), key_path.map(Path::to_owned)),
        fragment,
    )
}

/// Publish one Habit fragment through an explicitly owned storage lifetime.
/// Targets this signer is admitted to maintain are carried before success is
/// returned. If that upkeep fails, the error identifies the already-committed
/// fragment; it does not imply that publication was rolled back.
pub fn publish_with_storage(storage: &Storage, fragment: Fragment) -> Result<CollectionCommit> {
    storage.with_store(|pile, signer, runtime| {
        let collection = storage.open_collection_write(
            pile,
            DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            runtime,
        )?;
        let commit = pile
            .commit(collection, signer, fragment)
            .context("commit Habit fragment")?;
        runtime
            .block_on(crate::storage::ensure_downstream(pile, collection, signer))
            .context("Habit facts were committed, but ensuring their derived views failed")?;
        Ok(commit)
    })
}

/// Local cache root holding materialized habit scripts.
///
/// Content-addressed, so nothing in it is ever invalidated — only added to. The
/// override exists for sandboxes with a read-only home, and for tests.
pub fn script_cache_dir() -> std::result::Result<std::path::PathBuf, String> {
    if let Some(dir) = std::env::var_os("FACULTIES_HABIT_SCRIPT_CACHE") {
        return Ok(std::path::PathBuf::from(dir));
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .ok_or_else(|| {
            "neither FACULTIES_HABIT_SCRIPT_CACHE, XDG_CACHE_HOME nor HOME is set, so there is \
             nowhere to materialize the habit script"
                .to_owned()
        })?;
    Ok(base.join("faculties").join("habit"))
}

/// Whether the cache already holds this exact script, executable.
///
/// The path is the content hash and files arrive there only by rename, but the
/// cache is still ordinary user-writable state. Check the bytes as well as the
/// length and mode so a same-length edit can never run under somebody else's
/// digest.
fn cached_copy_is_usable(path: &Path, script: &Script) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => {
            meta.is_file()
                && meta.len() == script.bytes.len() as u64
                && meta.permissions().mode() & 0o100 != 0
                && std::fs::read(path)
                    .map(|bytes| bytes == script.bytes)
                    .unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// Write a carried script into the content-addressed cache and return its path.
///
/// Staging plus rename is what makes the cache safe to trust: the hash-named
/// path only ever appears complete, so a crashed writer cannot leave a
/// truncated executable behind for the next evaluation to run.
pub fn materialize_script(script: &Script) -> std::result::Result<std::path::PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;

    script.validate_identity()?;
    let directory = script_cache_dir()?;
    let path = directory.join(script.digest());
    if cached_copy_is_usable(&path, script) {
        return Ok(path);
    }
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create habit script cache {}: {error}", directory.display()))?;
    // The staging name is unique per *attempt*, not per process: two
    // evaluations of the same script inside one process would otherwise share
    // a staging path and rename it out from under each other.
    static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = directory.join(format!(
        ".{}.{}.{attempt}.staging",
        script.digest(),
        std::process::id()
    ));
    std::fs::write(&staging, &script.bytes)
        .map_err(|error| format!("write habit script {}: {error}", staging.display()))?;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700)).map_err(
        |error| {
            let _ = std::fs::remove_file(&staging);
            format!(
                "make habit script {} executable: {error}",
                staging.display()
            )
        },
    )?;
    std::fs::rename(&staging, &path).map_err(|error| {
        let _ = std::fs::remove_file(&staging);
        format!("install habit script {}: {error}", path.display())
    })?;
    Ok(path)
}

/// Quote one path as a single shell word.
///
/// The cache path is generated, but its root comes from the environment, so
/// quoting is not optional.
fn shell_word(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// Resolve a parsed condition into the exact shell command to run.
///
/// A leading `@script` command word is rewritten to the local path of the
/// materialized blob; everything else passes through untouched.
pub fn resolve_command(
    condition: &Condition,
    script: Option<&Script>,
) -> std::result::Result<String, String> {
    let Some(suffix) = condition.script_suffix() else {
        return Ok(condition.command.clone());
    };
    let script = script.ok_or_else(|| {
        format!("condition names `{SCRIPT_TOKEN}` but this Habit carries no script")
    })?;
    let path = materialize_script(script)?;
    Ok(format!("{}{suffix}", shell_word(&path)))
}

/// Run the predicate from the fixed directory supplied by the caller.
pub fn condition_holds(command: &str, at: &Path) -> std::result::Result<bool, String> {
    // A timer-driven predicate must not stream arbitrary output into whoever
    // happens to be reading Orient. Capture it, and retain stderr only for the
    // one shell outcome which means the predicate itself is broken.
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(at)
        .output()
        .map_err(|error| format!("running {command:?}: {error}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(code @ (126 | 127)) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            Err(if detail.is_empty() {
                format!("condition command exited {code}")
            } else {
                format!("condition command exited {code}: {detail}")
            })
        }
        Some(_) => Ok(false),
        None => Err(format!(
            "condition command terminated by signal: {command:?}"
        )),
    }
}

/// Evaluate one standing intention against a wall-clock second.
pub fn evaluate(row: &HabitRow, now_secs: i64, at: &Path) -> State {
    match &row.activation {
        Activation::Paused(_) => return State::Paused,
        Activation::Forked(heads) => {
            return State::Forked(heads.iter().map(|head| (head.id, head.state)).collect())
        }
        Activation::Active(_) => {}
    }
    let condition = match Condition::parse(&row.condition) {
        Ok(condition) => condition,
        Err(error) => return State::Unparseable(error),
    };
    // Cooldown first: it avoids spawning a predicate while a recent
    // completion already proves the intention satisfied.
    if !condition.cooled_down(now_secs, row.completed_at.iter().copied()) {
        return State::Cooling;
    }
    // A predicate that cannot be resolved is an error, never a quiet "not
    // due" — the whole point of carrying the script is that this cannot
    // depend on which machine is asking.
    let command = match resolve_command(&condition, row.script.as_ref()) {
        Ok(command) => command,
        Err(error) => return State::Failed(error),
    };
    match condition_holds(&command, at) {
        Ok(true) => State::Due,
        Ok(false) => State::Waiting,
        Err(error) => State::Failed(error),
    }
}

/// Shell predicates are evaluated relative to the directory containing the
/// pile, not whichever working directory happened to launch a watcher.
pub fn evaluation_dir(pile: &Path) -> std::path::PathBuf {
    pile.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_owned()
}

#[cfg(test)]
mod tests {
    use crate::storage::FactRead;
    use std::fs::File;
    use std::path::PathBuf;

    use hifitime::Epoch;
    use triblespace::core::blob::encodings::succinctarchive::{
        Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
    };
    use triblespace::core::collection::{Collection, CollectionStoreExt};

    use triblespace::core::repo::pile::PileSnapshot;
    use triblespace::core::repo::SnapshotSource;

    use crate::storage::{load_signer, open_pile_strict, open_pile_strict_as, FactArchive};
    use crate::test_support::initialize_open_collection_fixture;

    use super::*;

    fn at(seconds: f64) -> IntervalValue {
        let epoch = Epoch::from_unix_seconds(seconds);
        (epoch, epoch).try_to_inline().unwrap()
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        pile: PathBuf,
        key: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let pile = directory.path().join("habit.pile");
            let key = directory.path().join("habit.key");
            File::create(&pile).unwrap();
            initialize_open_collection_fixture(&pile, Some(&key));
            Self {
                _directory: directory,
                pile,
                key,
            }
        }

        fn publish(&self, fragment: Fragment) -> CollectionCommit {
            publish(&self.pile, Some(&self.key), fragment).unwrap()
        }

        /// One prepared view of the collection, read the way commands read it.
        fn view(&self) -> (PileSnapshot, FactArchive) {
            let (succinct, rank9) = self.targets();
            let (reader, _, facts) = self.prepared_targets(succinct, rank9);
            (reader, facts)
        }

        fn definitions(&self) -> Vec<Habit> {
            let (reader, facts) = self.view();
            definitions(&reader, &facts).unwrap()
        }

        /// Ids no complete successor supersedes, each once.
        fn live_ids(&self) -> Vec<Id> {
            let (reader, facts) = self.view();
            sorted_ids(
                live_definitions(&reader, &facts)
                    .unwrap()
                    .into_iter()
                    .map(|habit| habit.id),
            )
        }

        fn definition_count(&self) -> usize {
            definition_ids(&self.view().1).len()
        }

        // Name the targets before exercising a writer, so the reader below
        // prepares exactly these and never silently registers new ones.
        fn targets(
            &self,
        ) -> (
            Collection<SuccinctArchiveBlob>,
            Collection<Rank9AcceleratedSuccinctArchiveBlob>,
        ) {
            let signer = load_signer(&self.pile, Some(&self.key)).unwrap();
            let mut pile = open_pile_strict_as(&self.pile, signer.verifying_key()).unwrap();
            let source =
                open_configured(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
            let succinct = pile.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
            let rank9 = pile
                .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
                .unwrap();
            pile.close().unwrap();
            (succinct, rank9)
        }

        /// Prepare both derived targets the way an ordinary reader does, then
        /// attach them through the one resulting store boundary.
        fn prepared_targets(
            &self,
            succinct: Collection<SuccinctArchiveBlob>,
            rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
        ) -> (PileSnapshot, FactArchive, FactArchive) {
            let signer = load_signer(&self.pile, Some(&self.key)).unwrap();
            let mut pile = open_pile_strict_as(&self.pile, signer.verifying_key()).unwrap();
            let snapshot = pollster::block_on(async {
                drop(pile.maintain_attached(succinct, &signer).await?);
                pile.maintain_attached(rank9, &signer).await
            })
            .unwrap();
            let succinct_facts = snapshot
                .attached(succinct)
                .unwrap()
                .view::<FactArchive>()
                .unwrap();
            let rank9_facts = snapshot.read_facts(rank9).unwrap();
            pile.close().unwrap();
            (snapshot, succinct_facts, rank9_facts)
        }
    }

    fn row(condition: &str, completed_at: &[i64], activation: Activation) -> HabitRow {
        HabitRow {
            id: Id::new([1; 16]).unwrap(),
            label: "test".to_owned(),
            condition: condition.to_owned(),
            nudge: "do it".to_owned(),
            script: None,
            activation,
            completed_at: completed_at.to_vec(),
        }
    }

    /// One content-addressed cache for the whole test binary.
    ///
    /// The cache root is process-global state, so tests share it rather than
    /// racing to set it; content addressing makes sharing safe.
    fn shared_cache() -> &'static Path {
        static CACHE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let directory = CACHE.get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            std::env::set_var("FACULTIES_HABIT_SCRIPT_CACHE", directory.path());
            directory
        });
        directory.path()
    }

    /// A carried script, addressed exactly as publication would address it.
    fn script(source: &str) -> Script {
        let mut fragment = Fragment::empty();
        let handle = fragment.put::<blobencodings::RawBytes, _>(source.as_bytes().to_vec());
        let script = Script {
            handle,
            bytes: source.as_bytes().to_vec(),
        };
        // The pre-publication digest an authoring surface prints must be the
        // address the published definition actually carries.
        assert_eq!(script_digest(source.as_bytes()), script.digest());
        script
    }

    #[test]
    fn new_true_and_false_conditions_are_due_and_not_due() {
        let active = Activation::Active(Vec::new());
        assert_eq!(
            evaluate(
                &row("every 1h", &[], active.clone()),
                10_000,
                Path::new(".")
            ),
            State::Due
        );
        assert_eq!(
            evaluate(&row("when exit 1", &[], active), 10_000, Path::new(".")),
            State::Waiting
        );
    }

    #[test]
    fn cooldown_is_measured_from_completion_not_observation() {
        let row = row("every 1h", &[1_000, 9_000], Activation::Active(Vec::new()));
        for _ in 0..3 {
            assert_eq!(evaluate(&row, 10_000, Path::new(".")), State::Cooling);
        }
        assert_eq!(evaluate(&row, 12_600, Path::new(".")), State::Due);
        assert_eq!(row.next_cooldown_at().unwrap(), Some(12_600));
    }

    #[test]
    fn successive_mutations_advance_the_targets_a_reader_prepares() {
        let fixture = Fixture::new();
        let (succinct, rank9) = fixture.targets();
        let api = Habits::new(fixture.pile.clone(), Some(fixture.key.clone()));
        let added = api
            .add("eager habit", "every 1h", "observe it", None, &[], &[])
            .unwrap();
        let selector = format!("{:x}", added.id);
        let (before_done, old_succinct, old_rank9) = fixture.prepared_targets(succinct, rank9);
        for facts in [&old_succinct, &old_rank9] {
            assert_eq!(definition_ids(facts), BTreeSet::from([added.id]));
            assert!(completions(facts, added.id).is_empty());
        }

        let done = api.done(&selector).unwrap();
        let (reader, succinct_facts, rank9_facts) = fixture.prepared_targets(succinct, rank9);
        for facts in [&succinct_facts, &rank9_facts] {
            let completed = completions(facts, added.id);
            assert_eq!(completed.len(), 1);
            assert_eq!(completed[0].id, done.event);
            let current = rows(&reader, facts).unwrap().remove(0);
            assert_eq!(
                evaluate(
                    &current,
                    current.last_done().unwrap(),
                    fixture._directory.path()
                ),
                State::Cooling
            );
        }
        // A later maintenance pass must not mutate a previously selected view.
        for facts in [&old_succinct, &old_rank9] {
            assert!(completions(facts, added.id).is_empty());
            assert!(rows(&before_done, facts).unwrap()[0]
                .completed_at
                .is_empty());
        }

        for state in [DeclaredState::Paused, DeclaredState::Active] {
            let changed = api.set_state(&selector, state).unwrap();
            let event = changed
                .event
                .expect("each transition authors a state event");
            let (_, succinct_facts, rank9_facts) = fixture.prepared_targets(succinct, rank9);
            for facts in [&succinct_facts, &rank9_facts] {
                let current = activation(facts, added.id);
                assert_eq!(current.declared(), Some(state));
                assert_eq!(current.head_ids(), [event]);
            }
        }
        assert!(api
            .set_state(&selector, DeclaredState::Active)
            .unwrap()
            .event
            .is_none());
    }

    #[test]
    fn direct_publication_advances_the_targets_a_reader_prepares() {
        let fixture = Fixture::new();
        let (succinct, rank9) = fixture.targets();
        let (definition, habit) = habit_fragment(
            "direct eager habit",
            "every 1h",
            "observe it",
            None,
            &[],
            &[],
        )
        .unwrap();
        fixture.publish(definition);
        let (_, succinct_facts, rank9_facts) = fixture.prepared_targets(succinct, rank9);
        for facts in [&succinct_facts, &rank9_facts] {
            assert_eq!(definition_ids(facts), BTreeSet::from([habit]));
        }
        let (completion, event) = completion_fragment(habit, at(10.0)).unwrap();
        fixture.publish(completion);
        let (_, succinct_facts, rank9_facts) = fixture.prepared_targets(succinct, rank9);
        for facts in [&succinct_facts, &rank9_facts] {
            assert_eq!(
                completions(facts, habit)
                    .iter()
                    .map(|done| done.id)
                    .collect::<Vec<_>>(),
                [event]
            );
        }
    }

    #[test]
    fn intrinsic_definition_and_exact_retry_are_idempotent() {
        let fixture = Fixture::new();
        let (fragment, id) =
            habit_fragment("journal", "every 1d", "write the journal", None, &[], &[]).unwrap();
        let first = fixture.publish(fragment.clone());
        let after_first = std::fs::metadata(&fixture.pile).unwrap().len();
        let replay = fixture.publish(fragment);
        assert_eq!(replay, first);
        assert_eq!(std::fs::metadata(&fixture.pile).unwrap().len(), after_first);
        assert_eq!(
            fixture
                .definitions()
                .iter()
                .map(|habit| habit.id)
                .collect::<Vec<_>>(),
            [id]
        );

        let signer = load_signer(&fixture.pile, Some(&fixture.key)).unwrap();
        let mut pile = open_pile_strict(&fixture.pile).unwrap();
        let collection =
            open_configured(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        assert_eq!(
            collection,
            crate::collection_names::open(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key(),)
                .unwrap()
        );
        pile.close().unwrap();
    }

    #[test]
    fn persona_targets_are_optional_repeated_identity_facts() {
        let a = Id::new([0xB1; 16]).unwrap();
        let b = Id::new([0xB2; 16]).unwrap();
        let (global, global_id) =
            habit_fragment("tick", "every 1h", "notice", None, &[], &[]).unwrap();
        let raw = projected_definitions(global.facts()).remove(0);
        assert_eq!(raw.id, global_id);
        assert!(raw.personas.is_empty());
        // Adding the optional empty field preserves the old unscoped identity.
        let old_record = entity! { _ @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: "tick",
            attrs::condition: raw.condition,
            attrs::nudge: raw.nudge,
        };
        assert_eq!(old_record.root(), Some(global_id));
        let (scoped, scoped_id) =
            habit_fragment("tick", "every 1h", "notice", None, &[], &[b, a, a]).unwrap();
        let (reordered, reordered_id) =
            habit_fragment("tick", "every 1h", "notice", None, &[], &[a, b]).unwrap();
        assert_eq!(scoped_id, reordered_id);
        assert_eq!(scoped.facts(), reordered.facts());
        assert_ne!(global_id, scoped_id);
        assert_eq!(
            projected_definitions(scoped.facts()).remove(0).personas,
            vec![a, b]
        );
        let fixture = Fixture::new();
        fixture.publish(global);
        fixture.publish(scoped);
        assert_eq!(
            fixture
                .definitions()
                .into_iter()
                .find(|habit| habit.id == scoped_id)
                .unwrap()
                .personas,
            vec![a, b]
        );
    }

    #[test]
    fn ordinary_projection_treats_ids_as_opaque_and_ignores_extra_facts() {
        let id = Id::new([0xA6; 16]).unwrap();
        let mut fragment = Fragment::empty();
        let condition = fragment.put("every 1h".to_owned());
        let nudge = fragment.put("inspect the worktree".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&id) @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: "hygiene",
            attrs::condition: condition,
            attrs::nudge: nudge,
            metadata::created_at: at(1.0),
        };

        let mut stored = fragment.clone();
        let reader = stored.blobs_mut().snapshot().unwrap();
        let projected = definitions(&reader, fragment.facts()).unwrap();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].id, id);
        assert_eq!(projected[0].condition, "every 1h");
        assert_eq!(projected[0].nudge, "inspect the worktree");
    }

    #[test]
    fn superseded_missing_payloads_do_not_hold_the_live_view_hostage() {
        let retired = Id::new([0xA7; 16]).unwrap();
        let unavailable_condition = TextHandle::new([0xC1; 32]);
        let unavailable_nudge = TextHandle::new([0xC2; 32]);
        let retired_facts = entity! { ExclusiveId::force_ref(&retired) @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: "old",
            attrs::condition: unavailable_condition,
            attrs::nudge: unavailable_nudge,
        }
        .into_facts();

        let (successor, successor_id) =
            habit_fragment("current", "every 1h", "do it", None, &[retired], &[]).unwrap();
        let mut facts = retired_facts;
        facts += successor.facts().clone();
        let mut stored = successor.clone();
        let reader = stored.blobs_mut().snapshot().unwrap();

        let live = live_definitions(&reader, &facts).unwrap();
        assert_eq!(
            live.iter().map(|habit| habit.id).collect::<Vec<_>>(),
            [successor_id]
        );
    }

    #[test]
    fn missing_state_predecessors_are_valid_in_a_partial_view() {
        let habit = Id::new([0xA8; 16]).unwrap();
        let unseen = Id::new([0xA9; 16]).unwrap();
        let (assertion, assertion_id) =
            state_fragment(habit, DeclaredState::Paused, &[unseen], at(1.0)).unwrap();
        assert!(matches!(
            activation(assertion.facts(), habit),
            Activation::Paused(ref heads)
                if heads.len() == 1
                    && heads[0].id == assertion_id
                    && heads[0].predecessors == [unseen]
        ));
    }

    #[test]
    fn concurrent_state_assertions_stay_forked_until_reconciled() {
        let fixture = Fixture::new();
        let (definition, habit) =
            habit_fragment("journal", "every 1h", "write", None, &[], &[]).unwrap();
        fixture.publish(definition);

        // Two writers observed the same empty frontier and asserted different
        // states. Sequential publication here simulates their later set union.
        let (paused, _) = state_fragment(habit, DeclaredState::Paused, &[], at(1.0)).unwrap();
        let (active, _) = state_fragment(habit, DeclaredState::Active, &[], at(2.0)).unwrap();
        fixture.publish(paused);
        fixture.publish(active);
        let fork = activation(&fixture.view().1, habit);
        let heads = fork.head_ids();
        assert!(matches!(fork, Activation::Forked(ref values) if values.len() == 2));

        let (joined, _) = state_fragment(habit, DeclaredState::Active, &heads, at(3.0)).unwrap();
        fixture.publish(joined);
        assert!(matches!(
            activation(&fixture.view().1, habit),
            Activation::Active(ref values) if values.len() == 1 && values[0].predecessors == heads
        ));
    }

    /// Rows over an in-memory union of complete fragments, payloads included.
    fn rows_of(fragments: impl IntoIterator<Item = Fragment>) -> Vec<HabitRow> {
        let mut union = Fragment::empty();
        for fragment in fragments {
            union += fragment;
        }
        let facts = union.facts().clone();
        let reader = union.blobs_mut().snapshot().unwrap();
        rows(&reader, &facts).unwrap()
    }

    fn done_at(habit: Id, seconds: f64) -> Fragment {
        completion_fragment(habit, at(seconds)).unwrap().0
    }

    /// The integral TAI second a row reports for the Unix second `at` takes.
    fn tai(seconds: f64) -> i64 {
        interval_second(at(seconds)).unwrap()
    }

    /// A reworded revision is the same standing intention, so a completion
    /// recorded against the old wording minutes ago still satisfies it. This
    /// goes through the operations every frontend uses, the direct rows over
    /// one prepared view, and the check.
    #[test]
    fn a_reworded_revision_is_not_due_after_its_predecessor_was_done() {
        let fixture = Fixture::new();
        let api = Habits::new(fixture.pile.clone(), Some(fixture.key.clone()));
        let original = api
            .add("comb", "every 1h", "comb upward", None, &[], &[])
            .unwrap();
        let done = api.done(&format!("{:x}", original.id)).unwrap();
        let revised = api
            .add(
                "comb",
                "every 1h",
                "comb upward, reworded",
                None,
                &[format!("{:x}", original.id)],
                &[],
            )
            .unwrap();
        assert_ne!(revised.id, original.id);

        let listed = api.list(true).unwrap();
        assert_eq!(listed.entries.len(), 1);
        let entry = &listed.entries[0];
        assert_eq!(entry.row.id, revised.id);
        assert!(entry.row.last_done().is_some(), "done never");
        assert_eq!(entry.state, Some(State::Cooling));

        let (reader, facts) = fixture.view();
        let completed = completions(&facts, original.id)
            .into_iter()
            .find(|completion| completion.id == done.event)
            .unwrap();
        let expected = interval_second(completed.completed_at).unwrap();
        let direct = rows(&reader, &facts).unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].id, revised.id);
        assert_eq!(direct[0].completed_at, [expected]);
        assert_eq!(entry.row.completed_at, [expected]);
        let checked = audit(&reader, &facts);
        assert_eq!(
            (checked.definitions, checked.live, checked.completions),
            (2, 1, 1)
        );
        assert!(checked.notes.is_empty(), "{:?}", checked.notes);
    }

    #[test]
    fn completions_follow_a_two_step_revision_chain() {
        let (a, a_id) = habit_fragment("chain", "every 1h", "first", None, &[], &[]).unwrap();
        let (b, b_id) = habit_fragment("chain", "every 1h", "second", None, &[a_id], &[]).unwrap();
        let (c, c_id) = habit_fragment("chain", "every 1h", "third", None, &[b_id], &[]).unwrap();
        let rows = rows_of([a, b, c, done_at(a_id, 1_000.0), done_at(b_id, 2_000.0)]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, c_id);
        assert_eq!(rows[0].completed_at, [tai(1_000.0), tai(2_000.0)]);
        assert_eq!(rows[0].last_done(), Some(tai(2_000.0)));
        assert_eq!(rows[0].next_cooldown_at().unwrap(), Some(tai(5_600.0)));
        assert_eq!(
            evaluate(&rows[0], tai(3_000.0), Path::new(".")),
            State::Cooling
        );
    }

    /// Merges take the union of every predecessor's history and the latest
    /// completion wins. Branches each inherit the shared history, but a
    /// completion of one sibling is not a completion of the other.
    #[test]
    fn revision_merges_and_branches_read_the_union_of_their_ancestry() {
        let (a, a_id) = habit_fragment("merge", "every 1h", "left", None, &[], &[]).unwrap();
        let (b, b_id) = habit_fragment("merge", "every 1h", "right", None, &[], &[]).unwrap();
        let (c, c_id) =
            habit_fragment("merge", "every 1h", "joined", None, &[a_id, b_id], &[]).unwrap();
        let merged = rows_of([a, b, c, done_at(a_id, 5_000.0), done_at(b_id, 3_000.0)]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, c_id);
        assert_eq!(merged[0].completed_at, [tai(3_000.0), tai(5_000.0)]);
        assert_eq!(merged[0].last_done(), Some(tai(5_000.0)));

        let (root, root_id) = habit_fragment("fork", "every 1h", "root", None, &[], &[]).unwrap();
        let (d, d_id) =
            habit_fragment("fork", "every 1h", "one way", None, &[root_id], &[]).unwrap();
        let (e, e_id) =
            habit_fragment("fork", "every 1h", "other way", None, &[root_id], &[]).unwrap();
        let forked = rows_of([root, d, e, done_at(root_id, 100.0), done_at(d_id, 900.0)]);
        let by_id: BTreeMap<Id, Vec<i64>> = forked
            .into_iter()
            .map(|row| (row.id, row.completed_at))
            .collect();
        assert_eq!(by_id.len(), 2);
        assert_eq!(by_id[&d_id], [tai(100.0), tai(900.0)]);
        assert_eq!(by_id[&e_id], [tai(100.0)]);
    }

    /// Intrinsic ids cannot form a cycle, but opaque ones can and the store
    /// cannot forbid it. Reading such history must terminate.
    #[test]
    fn a_supersession_cycle_terminates() {
        let x = Id::new([0xB1; 16]).unwrap();
        let y = Id::new([0xB2; 16]).unwrap();
        let absent = TextHandle::new([0xB3; 32]);
        let mut cycle = Fragment::empty();
        cycle += entity! { ExclusiveId::force_ref(&x) @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: absent,
            attrs::condition: absent,
            attrs::nudge: absent,
            metadata::supersedes: &y,
        };
        cycle += entity! { ExclusiveId::force_ref(&y) @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: absent,
            attrs::condition: absent,
            attrs::nudge: absent,
            metadata::supersedes: &x,
        };
        // Every member of a closed cycle is superseded, so nothing is live.
        assert!(rows_of([cycle.clone(), done_at(x, 10.0)]).is_empty());

        let (z, z_id) = habit_fragment("cycle", "every 1h", "escape", None, &[x], &[]).unwrap();
        let rows = rows_of([cycle, z, done_at(x, 10.0), done_at(y, 20.0)]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, z_id);
        assert_eq!(rows[0].completed_at, [tai(10.0), tai(20.0)]);
    }

    /// The history is derived at read time from the edges, never copied onto
    /// the successor, so a predecessor's completion that arrives after the
    /// revision (from another pile, say) still counts.
    #[test]
    fn a_predecessor_completion_arriving_late_still_counts() {
        let (a, a_id) = habit_fragment("late", "every 1h", "before", None, &[], &[]).unwrap();
        let (b, b_id) = habit_fragment("late", "every 1h", "after", None, &[a_id], &[]).unwrap();
        let before = rows_of([a.clone(), b.clone()]);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].id, b_id);
        assert!(before[0].completed_at.is_empty());
        assert_eq!(
            evaluate(&before[0], tai(4_000.0), Path::new(".")),
            State::Due
        );

        let after = rows_of([a, b, done_at(a_id, 3_500.0)]);
        assert_eq!(after[0].id, b_id);
        assert_eq!(after[0].completed_at, [tai(3_500.0)]);
        assert_eq!(
            evaluate(&after[0], tai(4_000.0), Path::new(".")),
            State::Cooling
        );
    }

    /// Revision is an explicit id edge, so the upgrade path exists without the
    /// label ever behaving like a key.
    #[test]
    fn a_successor_retires_its_predecessor_by_id_not_by_name() {
        let fixture = Fixture::new();
        let (original, original_id) = habit_fragment(
            "sweep",
            "when /usr/local/bin/sweep --due",
            "sweep",
            None,
            &[],
            &[],
        )
        .unwrap();
        fixture.publish(original);
        assert_eq!(fixture.live_ids(), vec![original_id]);

        let (successor, successor_id) = habit_fragment(
            "sweep",
            "when @script --due",
            "sweep",
            Some(b"#!/bin/sh\nexit 0\n".to_vec()),
            &[original_id],
            &[],
        )
        .unwrap();
        fixture.publish(successor);

        let (reader, facts) = fixture.view();
        // Both definitions remain; only the successor is live, and only it is
        // evaluated.
        let all = definitions(&reader, &facts).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            all.iter()
                .filter(|habit| habit.label.eq_ignore_ascii_case("sweep"))
                .count(),
            2
        );
        assert_eq!(fixture.live_ids(), vec![successor_id]);
        assert!(is_superseded(&facts, original_id));
        assert_eq!(
            rows(&reader, &facts)
                .unwrap()
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [successor_id]
        );
    }

    /// Liveness must not depend on the order facts arrive in. Publishing the
    /// successor before its predecessor is legal set union, and both orders
    /// have to agree — this is the property a name-based rule cannot give.
    #[test]
    fn liveness_is_order_independent() {
        let (original, original_id) =
            habit_fragment("sweep", "every 1h", "sweep", None, &[], &[]).unwrap();
        let (successor, successor_id) =
            habit_fragment("sweep", "every 2h", "sweep", None, &[original_id], &[]).unwrap();

        for order in [[0usize, 1], [1, 0]] {
            let fixture = Fixture::new();
            for index in order {
                let fragment = if index == 0 {
                    original.clone()
                } else {
                    successor.clone()
                };
                fixture.publish(fragment);
            }
            assert_eq!(fixture.live_ids(), vec![successor_id], "{order:?}");
        }
    }

    /// Two windows authoring the same revision of the same intention converge
    /// on one definition, because identity is the content and not the name.
    #[test]
    fn identical_revisions_authored_independently_are_one_definition() {
        let fixture = Fixture::new();
        let (original, original_id) =
            habit_fragment("sweep", "every 1h", "sweep", None, &[], &[]).unwrap();
        fixture.publish(original);

        let first =
            habit_fragment("sweep", "every 2h", "sweep", None, &[original_id], &[]).unwrap();
        let second =
            habit_fragment("sweep", "every 2h", "sweep", None, &[original_id], &[]).unwrap();
        assert_eq!(first.1, second.1);
        fixture.publish(first.0);
        fixture.publish(second.0);
        assert_eq!(fixture.definition_count(), 2);
        assert_eq!(fixture.live_ids(), vec![first.1]);
    }

    /// Concurrent creation without an edge is two live definitions sharing a
    /// name. That is the truth, and it stays visible until an explicit edge
    /// resolves it — no winner is picked.
    #[test]
    fn concurrent_creation_stays_forked_until_an_edge_resolves_it() {
        let fixture = Fixture::new();
        let (mine, mine_id) = habit_fragment("sweep", "every 1h", "sweep", None, &[], &[]).unwrap();
        let (theirs, theirs_id) =
            habit_fragment("sweep", "every 2h", "sweep", None, &[], &[]).unwrap();
        fixture.publish(mine);
        fixture.publish(theirs);
        assert_eq!(fixture.live_ids().len(), 2);

        let (joined, joined_id) = habit_fragment(
            "sweep",
            "every 3h",
            "sweep",
            None,
            &[mine_id, theirs_id],
            &[],
        )
        .unwrap();
        fixture.publish(joined);
        assert_eq!(fixture.live_ids(), vec![joined_id]);
    }

    /// An edge naming a definition this window has not seen is legal and
    /// retires nothing yet. Rejecting it would mean a window that received the
    /// successor first could not read the collection at all — the partial-view
    /// hazard that makes any such rule non-monotonic.
    #[test]
    fn an_edge_to_an_unseen_definition_retires_it_on_arrival() {
        let fixture = Fixture::new();
        let (original, original_id) =
            habit_fragment("sweep", "every 1h", "sweep", None, &[], &[]).unwrap();
        let (successor, successor_id) =
            habit_fragment("sweep", "every 2h", "sweep", None, &[original_id], &[]).unwrap();

        fixture.publish(successor);
        // The predecessor is not here; nothing is retired, and the view reads.
        assert_eq!(fixture.live_ids(), vec![successor_id]);
        assert!(is_superseded(&fixture.view().1, original_id));

        fixture.publish(original);
        // It arrives already retired, and liveness is unchanged.
        assert_eq!(fixture.definition_count(), 2);
        assert_eq!(fixture.live_ids(), vec![successor_id]);
    }

    #[test]
    fn concurrent_definition_conflicts_are_visible_not_timestamp_arbitrated() {
        let fixture = Fixture::new();
        let (daily, daily_id) =
            habit_fragment("hygiene", "every 1d", "inspect branches", None, &[], &[]).unwrap();
        let (weekly, weekly_id) =
            habit_fragment("hygiene", "every 7d", "inspect branches", None, &[], &[]).unwrap();
        fixture.publish(daily);
        fixture.publish(weekly);
        assert_eq!(
            fixture
                .definitions()
                .into_iter()
                .filter(|habit| habit.label.eq_ignore_ascii_case("HYGIENE"))
                .map(|habit| habit.id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([daily_id, weekly_id])
        );
    }

    /// The point of the whole feature: the predicate is the pile's bytes, not
    /// anything the evaluating machine happens to have on disk or on PATH.
    #[test]
    fn a_carried_script_is_run_from_the_content_addressed_cache() {
        shared_cache();
        let due = script("#!/bin/sh\nexit 0\n");
        let mut row = row("when @script --due", &[], Activation::Active(Vec::new()));
        row.script = Some(due.clone());
        assert_eq!(evaluate(&row, 10_000, Path::new(".")), State::Due);

        let path = materialize_script(&due).unwrap();
        assert_eq!(path.parent().unwrap(), shared_cache());
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), due.digest());
        assert_eq!(std::fs::read(&path).unwrap(), due.bytes);

        let mut waiting = row.clone();
        waiting.script = Some(script("#!/bin/sh\nexit 1\n"));
        assert_eq!(evaluate(&waiting, 10_000, Path::new(".")), State::Waiting);
    }

    /// Editing a script changes its hash, so it lands beside the old copy
    /// rather than on top of it. Nothing can execute a superseded body.
    #[test]
    fn an_edited_script_is_a_different_cache_entry() {
        shared_cache();
        let first = script("#!/bin/sh\nexit 0\n");
        let second = script("#!/bin/sh\nexit 1\n");
        assert_ne!(first.digest(), second.digest());
        let first_path = materialize_script(&first).unwrap();
        let second_path = materialize_script(&second).unwrap();
        assert_ne!(first_path, second_path);
        assert_eq!(std::fs::read(&first_path).unwrap(), first.bytes);
        assert_eq!(std::fs::read(&second_path).unwrap(), second.bytes);
    }

    /// A truncated leftover at the hash path is repaired, not executed. Rename
    /// makes this unreachable in practice; the check is what makes that a fact
    /// rather than an assumption.
    #[test]
    fn a_short_cache_entry_is_rewritten_rather_than_trusted() {
        let cache = shared_cache();
        let full = script("#!/bin/sh\n# a longer body\nexit 0\n");
        let path = cache.join(full.digest());
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        assert_eq!(materialize_script(&full).unwrap(), path);
        assert_eq!(std::fs::read(&path).unwrap(), full.bytes);
    }

    /// A hash-shaped path is not proof by itself: the cache is user-writable,
    /// so a same-length edit must be repaired before evaluation can execute it.
    #[test]
    fn a_same_length_cache_edit_is_rewritten_rather_than_executed() {
        shared_cache();
        let expected = script("#!/bin/sh\n# cache-integrity-only\nexit 0\n");
        let path = materialize_script(&expected).unwrap();
        let altered = b"#!/bin/sh\n# cache-integrity-only\nexit 1\n";
        assert_eq!(altered.len(), expected.bytes.len());
        std::fs::write(&path, altered).unwrap();

        assert_eq!(materialize_script(&expected).unwrap(), path);
        assert_eq!(std::fs::read(path).unwrap(), expected.bytes);
    }

    #[test]
    fn a_script_handle_cannot_name_different_bytes() {
        shared_cache();
        let named = script("#!/bin/sh\n# named body\nexit 0\n");
        let other = script("#!/bin/sh\n# other body\nexit 0\n");
        let forged = Script {
            handle: named.handle,
            bytes: other.bytes,
        };
        let error = materialize_script(&forged).unwrap_err();
        assert!(
            error.contains("does not address its carried bytes"),
            "{error}"
        );
    }

    /// Substitution has to survive a cache root with a quote or a space in it,
    /// because the root comes from the environment.
    #[test]
    fn script_substitution_produces_one_shell_word() {
        let directory = tempfile::tempdir().unwrap();
        let awkward = directory.path().join("it's a cache");
        std::fs::create_dir_all(&awkward).unwrap();
        let path = awkward.join("body");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let command = format!("test -f {} && exit 0", shell_word(&path));
        assert!(condition_holds(&command, Path::new(".")).unwrap());
    }

    #[test]
    fn script_substitution_only_replaces_the_leading_command_word() {
        shared_cache();
        let carried = script("#!/bin/sh\nexit 0\n");
        let condition = Condition::parse("when @scripture @script").unwrap();
        assert_eq!(
            resolve_command(&condition, Some(&carried)).unwrap(),
            "@scripture @script"
        );
    }

    #[test]
    fn unavailable_shell_commands_are_errors_not_false_predicates() {
        assert!(condition_holds("exit 126", Path::new(".")).is_err());
        assert!(condition_holds("exit 127", Path::new(".")).is_err());
        assert_eq!(condition_holds("exit 1", Path::new(".")).unwrap(), false);
    }

    /// Round-trip: publish a definition carrying its script, read it back from
    /// the pile alone, and get the same bytes and the same handle.
    #[test]
    fn a_published_definition_carries_its_script_through_the_pile() {
        let fixture = Fixture::new();
        let source = b"#!/bin/sh\nexit 0\n".to_vec();
        let (fragment, id) = habit_fragment(
            "sweep",
            "when @script --due",
            "sweep the worktrees",
            Some(source.clone()),
            &[],
            &[],
        )
        .unwrap();
        fixture.publish(fragment);
        let definitions = fixture.definitions();
        let carried = definitions
            .iter()
            .find(|habit| habit.id == id)
            .unwrap()
            .script
            .as_ref()
            .unwrap();
        assert_eq!(carried.bytes, source);
        assert_eq!(carried.digest().len(), 64);
    }

    /// A definition and its attachment must agree in both directions. Authoring
    /// refuses a mismatch; one that arrives anyway is read, reported by the
    /// check, and evaluated as an error rather than a quiet "not due".
    #[test]
    fn condition_and_attachment_must_agree() {
        let dangling =
            habit_fragment("sweep", "when @script --due", "sweep", None, &[], &[]).unwrap_err();
        assert!(format!("{dangling:#}").contains("carries no script"));

        let unreachable = habit_fragment(
            "sweep",
            "every 1h",
            "sweep",
            Some(b"#!/bin/sh\n".to_vec()),
            &[],
            &[],
        )
        .unwrap_err();
        assert!(format!("{unreachable:#}").contains("no condition reaches"));

        let mut fragment = Fragment::empty();
        let condition = fragment.put("when @script --due".to_owned());
        let nudge = fragment.put("sweep".to_owned());
        let label = fragment.put("sweep".to_owned());
        let record = habit_record(label, condition, nudge, None, &[], &[]);
        fragment += record;
        let facts = fragment.facts().clone();
        let reader = fragment.blobs_mut().snapshot().unwrap();
        let checked = audit(&reader, &facts);
        assert_eq!(checked.definitions, 1);
        assert!(
            checked
                .notes
                .iter()
                .any(|note| note.contains("carries no script")),
            "{:?}",
            checked.notes
        );
        let read = rows(&reader, &facts).unwrap();
        assert!(matches!(
            evaluate(&read[0], 10_000, Path::new(".")),
            State::Failed(ref error) if error.contains("carries no script")
        ));
    }

    /// An unresolvable script names the habit and the exact missing blob; it
    /// never degrades into a habit that silently stops firing. The live view
    /// refuses to evaluate it, and the check reports it.
    #[test]
    fn an_unresolvable_script_blob_is_a_loud_error() {
        let mut fragment = Fragment::empty();
        let condition = fragment.put("when @script --due".to_owned());
        let nudge = fragment.put("sweep".to_owned());
        let absent = ScriptHandle::new([7; 32]);
        let label = fragment.put("sweep".to_owned());
        let record = habit_record(label, condition, nudge, Some(absent), &[], &[]);
        let id = record.root().unwrap();
        fragment += record;

        let facts = fragment.facts().clone();
        let reader = fragment.blobs_mut().snapshot().unwrap();
        let error = rows(&reader, &facts).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains(&format!("{id:x}")), "{rendered}");
        assert!(rendered.contains(&hex::encode(absent.raw)), "{rendered}");

        let checked = audit(&reader, &facts);
        assert!(
            checked
                .notes
                .iter()
                .any(|note| note.contains(&format!("{id:x}"))
                    && note.contains(&hex::encode(absent.raw))),
            "{:?}",
            checked.notes
        );
    }

    // Open-world reading. Each test below failed on the closed-world reader
    // this replaced: the facts are ordinary under merge-by-concatenation, so
    // a reader that refuses them makes the whole collection unreadable.

    fn api(fixture: &Fixture) -> Habits {
        Habits::new(fixture.pile.clone(), Some(fixture.key.clone()))
    }

    /// The schema cannot express cardinality, so a definition carrying two
    /// conditions is two readings of one intention, not a broken record. The
    /// intention is due when any reading is, and is addressed by its id.
    #[test]
    fn two_values_for_one_attribute_are_each_read_not_refused() {
        let fixture = Fixture::new();
        let api = api(&fixture);
        let id = Id::new([0xC3; 16]).unwrap();
        let mut fragment = Fragment::empty();
        let label = fragment.put("twice".to_owned());
        let hourly = fragment.put("every 1h".to_owned());
        let daily = fragment.put("every 1d".to_owned());
        let nudge = fragment.put("do it".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&id) @
            metadata::tag: &KIND_HABIT_ID,
            attrs::label: label,
            attrs::condition: hourly,
            attrs::nudge: nudge,
        };
        fragment += entity! { ExclusiveId::force_ref(&id) @ attrs::condition: daily };
        fixture.publish(fragment);

        let checked = api.check().unwrap().audit;
        assert_eq!(checked.definitions, 1);
        assert!(
            checked
                .notes
                .iter()
                .any(|note| note.contains("2 conditions")),
            "{:?}",
            checked.notes
        );
        let before = api.list(true).unwrap();
        let readings: Vec<_> = before
            .entries
            .iter()
            .filter(|entry| entry.row.id == id)
            .collect();
        assert_eq!(readings.len(), 2);
        assert!(readings.iter().all(|entry| entry.state == Some(State::Due)));
        let due = render::listed(&before, true).text;
        assert_eq!(due.matches("twice: do it").count(), 1, "{due}");

        assert_eq!(api.show("twice").unwrap().definition.id, id);
        assert_eq!(api.done(&format!("{id:x}")).unwrap().habit, id);
        assert_eq!(api.done("twice").unwrap().habit, id);
        let after = api.list(true).unwrap();
        assert_eq!(after.entries.len(), 2);
        assert!(after
            .entries
            .iter()
            .all(|entry| entry.state == Some(State::Cooling)));
    }

    /// Facts this reader does not model annotate a record; they never make it
    /// unreadable, whichever record kind they are attached to.
    #[test]
    fn facts_a_reader_does_not_model_never_fail_the_check() {
        let fixture = Fixture::new();
        let api = api(&fixture);
        let (definition, habit) =
            habit_fragment("annotated", "every 1h", "do it", None, &[], &[]).unwrap();
        let (completion, event) = completion_fragment(habit, at(10.0)).unwrap();
        let (paused, assertion) =
            state_fragment(habit, DeclaredState::Paused, &[], at(20.0)).unwrap();
        let foreign = Id::new([0xEE; 16]).unwrap();
        let mut union = definition + completion + paused;
        for annotated in [habit, event, assertion] {
            union += entity! { ExclusiveId::force_ref(&annotated) @ metadata::tag: &foreign };
        }
        union += entity! { ExclusiveId::force_ref(&habit) @ metadata::created_at: at(1.0) };
        fixture.publish(union);

        let checked = api.check().unwrap().audit;
        assert_eq!(
            (checked.definitions, checked.completions, checked.assertions),
            (1, 1, 1)
        );
        assert!(checked.notes.is_empty(), "{:?}", checked.notes);
        let listed = api.list(false).unwrap();
        assert_eq!(listed.entries.len(), 1);
        assert_eq!(listed.entries[0].row.completed_at, [tai(10.0)]);
        assert!(matches!(
            listed.entries[0].row.activation,
            Activation::Paused(_)
        ));
    }

    /// Set union has no order, so a completion or state assertion can arrive
    /// before the definition it names. It waits, and counts on arrival.
    #[test]
    fn a_completion_arriving_before_its_definition_does_not_fail_the_check() {
        let fixture = Fixture::new();
        let api = api(&fixture);
        let (definition, habit) =
            habit_fragment("early", "every 1h", "do it", None, &[], &[]).unwrap();
        let (completion, _) = completion_fragment(habit, at(10.0)).unwrap();
        let (paused, _) = state_fragment(habit, DeclaredState::Paused, &[], at(20.0)).unwrap();
        fixture.publish(completion);
        fixture.publish(paused);

        let waiting = api.check().unwrap().audit;
        assert_eq!((waiting.definitions, waiting.completions), (0, 1));
        let name = format!("{habit:x}");
        for kind in ["completion(s)", "state assertion(s)"] {
            assert!(
                waiting.notes.iter().any(|note| note.contains(kind)
                    && note.contains(&name)
                    && note.contains("no known definition yet")),
                "{:?}",
                waiting.notes
            );
        }
        assert!(api.list(false).unwrap().entries.is_empty());

        fixture.publish(definition);
        let arrived = api.check().unwrap().audit;
        assert!(arrived.notes.is_empty(), "{:?}", arrived.notes);
        let listed = api.list(false).unwrap();
        assert_eq!(listed.entries.len(), 1);
        assert_eq!(listed.entries[0].row.id, habit);
        assert_eq!(listed.entries[0].row.completed_at, [tai(10.0)]);
        assert!(matches!(
            listed.entries[0].row.activation,
            Activation::Paused(_)
        ));
    }

    /// The ordinary path already reads a state track whose predecessor this
    /// view has not received; the check must not refuse what list reads.
    #[test]
    fn an_unseen_state_predecessor_does_not_fail_the_check() {
        let fixture = Fixture::new();
        let api = api(&fixture);
        let (definition, habit) =
            habit_fragment("partial", "every 1h", "do it", None, &[], &[]).unwrap();
        let unseen = Id::new([0xC4; 16]).unwrap();
        let (paused, _) = state_fragment(habit, DeclaredState::Paused, &[unseen], at(1.0)).unwrap();
        fixture.publish(definition);
        fixture.publish(paused);

        let checked = api.check().unwrap().audit;
        assert!(
            checked
                .notes
                .iter()
                .any(|note| note.contains(&format!("{unseen:x}"))
                    && note.contains("not in this view")),
            "{:?}",
            checked.notes
        );
        assert!(matches!(
            api.list(false).unwrap().entries[0].row.activation,
            Activation::Paused(_)
        ));
    }

    /// Intrinsic ids cannot cycle, opaque ones can. A state track with no
    /// maximal assertion declares nothing, and the rest of the collection
    /// stays readable.
    #[test]
    fn a_state_cycle_does_not_make_the_collection_unreadable() {
        let (definition, habit) =
            habit_fragment("cyclic", "every 1h", "do it", None, &[], &[]).unwrap();
        let (other, other_id) =
            habit_fragment("bystander", "every 1h", "do it", None, &[], &[]).unwrap();
        let x = Id::new([0xC5; 16]).unwrap();
        let y = Id::new([0xC6; 16]).unwrap();
        let mut cycle = Fragment::empty();
        for (this, predecessor) in [(x, y), (y, x)] {
            cycle += entity! { ExclusiveId::force_ref(&this) @
                metadata::tag: &KIND_STATE_ID,
                attrs::of: &habit,
                attrs::state: STATE_PAUSED,
                metadata::supersedes: &predecessor,
                metadata::created_at: at(1.0),
            };
        }
        let mut union = definition.clone() + other.clone() + cycle.clone();
        let facts = union.facts().clone();
        let reader = union.blobs_mut().snapshot().unwrap();
        assert!(audit(&reader, &facts)
            .notes
            .iter()
            .any(|note| note.contains("no maximal assertion")));
        let rows = rows_of([definition, other, cycle]);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(
                row.activation,
                Activation::Active(Vec::new()),
                "{:x}",
                row.id
            );
        }
        assert!(rows.iter().any(|row| row.id == other_id));
    }

    /// Queries are types: a completion time that does not decode as an
    /// interval is skipped, and an interval wider than a point is read at its
    /// lower bound, as Orient reads it.
    #[test]
    fn a_completion_time_a_reader_cannot_decode_is_skipped() {
        let (definition, habit) =
            habit_fragment("timed", "every 1h", "do it", None, &[], &[]).unwrap();
        let mut inverted = at(2_000.0).raw;
        inverted[16..].copy_from_slice(&at(1_000.0).raw[16..]);
        let mut span = at(1_000.0).raw;
        span[16..].copy_from_slice(&at(1_500.0).raw[16..]);
        let mut odd = Fragment::empty();
        for (event, raw) in [
            (Id::new([0xC7; 16]).unwrap(), inverted),
            (Id::new([0xC8; 16]).unwrap(), span),
        ] {
            odd += entity! { ExclusiveId::force_ref(&event) @
                metadata::tag: &KIND_DONE_ID,
                attrs::of: &habit,
                metadata::created_at: IntervalValue::new(raw),
            };
        }
        let rows = rows_of([definition, odd]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].completed_at, [tai(1_000.0)]);
    }
}
