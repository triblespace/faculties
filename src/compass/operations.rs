//! Callable Compass operations over maintained immutable observations.
//!
//! Inputs are literal native values. CLI paths/pipes and MCP argument decoding
//! belong to their explicit frontends, never to these operations.

#[cfg(test)]
use crate::collection_names::{open_configured, open_exact_in};
use crate::schemas::compass::{
    board, latest_status_event, DEFAULT_SCOPE_ID as COMPASS_SCOPE_ID, DEFAULT_STATUSES,
    KIND_GOAL_ID, KIND_NOTE_ID, KIND_STATUS_ID,
};
use crate::schemas::relations::DEFAULT_SCOPE_ID as RELATIONS_SCOPE_ID;
#[cfg(test)]
use crate::storage::FactRead;
use crate::storage::{self, AcquiringReader, FactArchive, FacultySnapshot, FacultyStore, Storage};
use crate::{clock, compass, relations};
use anyhow::{bail, Context, Result};
use hifitime::Epoch;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::lww_register::{LwwIndex, LwwQuery};
use triblespace::core::metadata;
#[cfg(test)]
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::prelude::*;
use triblespace_paths::{PathExpr, PathIndex, Step};

type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;

/// Pile-backed Compass functionality, independent of either frontend.
/// Each operation observes a fresh maintained snapshot through its storage handle.
#[derive(Clone, Debug)]
pub struct Compass {
    storage: Storage,
}

#[derive(Clone, Copy, Debug)]
pub struct AddOptions<'a> {
    pub status: &'a str,
    pub parent: Option<&'a str>,
    pub tags: &'a [String],
    pub note: Option<&'a str>,
    /// Cooperative authorship claim, not authorization; never inferred from
    /// the process environment. A label or full id resolves through Relations.
    pub persona: Option<&'a str>,
}

impl Default for AddOptions<'_> {
    fn default() -> Self {
        Self {
            status: "todo",
            parent: None,
            tags: &[],
            note: None,
            persona: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ListOptions<'a> {
    pub statuses: &'a [String],
    pub tags: &'a [String],
    pub all: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoteOptions<'a> {
    pub tags: &'a [String],
    pub references: &'a [String],
    pub supersedes: &'a [String],
    pub persona: Option<&'a str>,
}

/// Identifiers of the complete action published by one add operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddedGoal {
    pub goal: Id,
    pub status_event: Id,
    pub note: Option<Id>,
    /// `@label` tokens in the title or note that named no Relations person
    /// or group. The write succeeded; nobody was addressed by these.
    pub unresolved_mentions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MovedGoal {
    pub goal: Id,
    pub event: Id,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddedNote {
    pub goal: Id,
    pub note: Id,
    /// `@label` tokens in the note that named no Relations person or group.
    /// The write succeeded; nobody was addressed by these.
    pub unresolved_mentions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriorityChange {
    pub higher: Id,
    pub lower: Id,
    pub event: Id,
    pub active: bool,
    pub higher_title: String,
    pub lower_title: String,
}

impl Compass {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    fn storage(&self) -> CompassStorage<'_> {
        CompassStorage {
            storage: &self.storage,
        }
    }

    pub fn add(&self, title: &str, options: AddOptions<'_>) -> Result<AddedGoal> {
        add_goal(
            self.storage(),
            title.to_owned(),
            options.status.to_owned(),
            options.parent.map(str::to_owned),
            options.tags.to_vec(),
            options.note.map(str::to_owned),
            options.persona,
        )
    }

    /// Render only the selected goals; hidden goals' text is not acquired.
    pub fn list(&self, options: ListOptions<'_>) -> Result<String> {
        list(
            self.storage(),
            options.statuses.to_vec(),
            options.tags.to_vec(),
            options.all,
        )
    }

    pub fn move_goal(&self, id: &str, status: &str, persona: Option<&str>) -> Result<MovedGoal> {
        move_goal(self.storage(), id.to_owned(), status.to_owned(), persona)
    }

    /// The body is literal prose, including any leading `@` characters.
    pub fn note(&self, id: &str, body: &str, options: NoteOptions<'_>) -> Result<AddedNote> {
        add_note(
            self.storage(),
            id.to_owned(),
            body.to_owned(),
            options.tags.to_vec(),
            options.references.to_vec(),
            options.supersedes.to_vec(),
            options.persona,
        )
    }

    pub fn show(&self, id: &str) -> Result<String> {
        show(self.storage(), id.to_owned())
    }

    pub fn prioritize(&self, higher: &str, lower: &str) -> Result<PriorityChange> {
        prioritize(self.storage(), higher.to_owned(), lower.to_owned())
    }

    pub fn deprioritize(&self, higher: &str, lower: &str) -> Result<PriorityChange> {
        deprioritize(self.storage(), higher.to_owned(), lower.to_owned())
    }

    pub fn resolve(&self, prefix: &str) -> Result<Id> {
        resolve(self.storage(), prefix.to_owned())
    }
}

// ── on-demand board queries ───────────────────────────────────────────
// All data stays in the maintained Succinct view; query it directly instead
// of pre-materializing Rust catalogs.

/// Query helpers that operate directly on one immutable fact view + workspace.

type IntervalValue = Inline<inlineencodings::NsTAIInterval>;

/// The lower bound of an interval in TAI nanoseconds, if the value decodes.
///
/// Queries are types: a stored value that does not decode as an interval (an
/// inverted pair) is not a time this reader can order by, so every caller
/// skips the row that carries it instead of panicking. A wider interval is
/// read at its lower bound, the earliest moment it admits.
fn interval_key(interval: IntervalValue) -> Option<i128> {
    let (lower, _): (i128, i128) = interval.try_from_inline().ok()?;
    Some(lower)
}

/// The lower bound of an interval as text, if the value decodes.
fn format_interval(interval: IntervalValue) -> Option<String> {
    let (lower, _): (Epoch, Epoch) = interval.try_from_inline().ok()?;
    Some(format!("{}", lower))
}

fn fmt_ids(ids: &[Id]) -> String {
    ids.iter()
        .map(|id| fmt_id(*id))
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_label(label: &str, value: &str) -> Result<()> {
    if value.as_bytes().iter().any(|b| *b == 0) {
        bail!("{label} contains NUL bytes: {value}");
    }
    Ok(())
}

fn normalize_status(status: String) -> String {
    status.trim().to_lowercase()
}

fn fmt_id(id: Id) -> String {
    format!("{id:x}")
}

/// Extract `[text](faculty:<hex>)` markdown link references from text.
/// Returns (faculty, hex_string) pairs.
fn extract_references(text: &str) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    let mut rest = text;
    while let Some(paren) = rest.find("](") {
        let after = &rest[paren + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        let link = &after[..end];
        if let Some(colon) = link.find(':') {
            let faculty = &link[..colon];
            let hex: String = link[colon + 1..]
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if hex.len() >= 4
                && !faculty.is_empty()
                && faculty
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                refs.push((faculty.to_string(), hex));
            }
        }
        rest = &after[end + 1..];
    }
    refs.sort();
    refs.dedup();
    refs
}

fn extract_reference_values(text: &str) -> Vec<String> {
    extract_references(text)
        .into_iter()
        .map(|(faculty, hex)| format!("{faculty}:{hex}"))
        .collect()
}

/// What each `@label` of one write resolved to through Relations, keyed by
/// [`relations::lookup_key`] and keeping the spelling the author used. A label
/// that resolves to nothing keeps an empty set, so the write can name it in a
/// warning and still succeed.
#[derive(Clone, Debug, Default)]
struct Mentions(BTreeMap<String, (String, BTreeSet<Id>)>);

impl Mentions {
    /// Every anchor the `@label` tokens of `text` resolved to.
    fn targets(&self, text: &str) -> Vec<Id> {
        compass::mention_labels(text)
            .iter()
            .filter_map(|label| self.0.get(&relations::lookup_key(label)))
            .flat_map(|(_, targets)| targets.iter().copied())
            .collect()
    }

    fn unresolved(&self) -> Vec<String> {
        self.0
            .values()
            .filter(|(_, targets)| targets.is_empty())
            .map(|(label, _)| label.clone())
            .collect()
    }
}

#[derive(Clone, Copy)]
struct CompassStorage<'a> {
    storage: &'a Storage,
}

impl CompassStorage<'_> {
    fn with_pile<T>(
        &self,
        f: impl FnOnce(
            &mut FacultyStore,
            &ed25519_dalek::SigningKey,
            &std::sync::Arc<tokio::runtime::Runtime>,
        ) -> Result<T>,
    ) -> Result<T> {
        self.storage
            .with_store(|pile, signer, runtime| f(pile, signer, runtime))
    }

    /// Prepare a pure read against fixed facts/status and an acquiring blob
    /// reader. Printing and other effects belong after this returns.
    fn with_view<T>(
        &self,
        mut f: impl FnMut(&FactArchive, &AcquiringReader<FacultySnapshot>, &LwwQuery) -> Result<T>,
    ) -> Result<T> {
        self.with_pile(|pile, signer, runtime| {
            let source = self.storage.open_collection_read(
                pile,
                COMPASS_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
            let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
            let status_target = compass::status_register_for_source(pile, source)?;
            let reader = AcquiringReader::new(
                pile.snapshot().context("freeze Compass facts and status")?,
                runtime.clone(),
            );
            let facts = storage::acquire_facts(&reader, rank9)?;
            let status = storage::require_complete_attached_read(
                reader
                    .attached_acquiring(status_target)?
                    .read_acquiring::<LwwIndex>()?,
            )?
            .query()?;
            f(&facts, &reader, &status)
        })
    }

    /// Prepare reads against one frozen view, then author and publish once.
    /// The author runs outside retries, so event clocks and publication cannot
    /// be repeated by a missing attachment. `None` is a genuine no-op.
    fn update<P, T>(
        &self,
        persona: Option<&str>,
        mentions: &[String],
        mut prepare: impl FnMut(
            &FactArchive,
            &AcquiringReader<FacultySnapshot>,
            Option<Id>,
            &Mentions,
        ) -> Result<P>,
        author: impl FnOnce(P) -> Result<(Option<Fragment>, T)>,
    ) -> Result<T> {
        self.with_pile(|pile, signer, runtime| {
            // Register every representation and attach the resident views
            // through one query snapshot for this action; the write authors,
            // commits, then attaches the source's frontier so the write is
            // readable through the attached views. Reads never maintain.
            let compass_source = self.storage.open_collection_read(
                pile,
                COMPASS_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            let compass_succinct = pile
                .attach::<SuccinctArchiveBlob>(compass_source, ())
                .context("register Compass Succinct collection")?;
            let compass_rank9 = pile
                .attach::<Rank9AcceleratedSuccinctArchiveBlob>(compass_source, compass_succinct)
                .context("register Compass Rank9 collection")?;
            let relations_rank9 = if persona.is_some() || !mentions.is_empty() {
                let source = self.storage.open_collection_read(
                    pile,
                    RELATIONS_SCOPE_ID,
                    signer.verifying_key(),
                    runtime,
                )?;
                let succinct = pile
                    .attach::<SuccinctArchiveBlob>(source, ())
                    .context("register Relations Succinct collection")?;
                let rank9 = pile
                    .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
                    .context("register Relations Rank9 collection")?;
                Some(rank9)
            } else {
                None
            };

            // Attach every view through one immutable store boundary, so
            // validation and persona resolution cannot mix collection
            // watermarks.
            let reader = AcquiringReader::new(
                pile.snapshot()
                    .context("freeze Compass/Relations snapshot")?,
                runtime.clone(),
            );
            // No read refuses for being behind. There is no globally
            // consistent state to be behind of: another node holds commits
            // this one has never seen, so "stands for every admitted commit"
            // is a closed-world claim, and refusing on it would only narrow
            // the branching of one entity's history a little. A priority
            // change reads what it can see, like everything else, and
            // ensures its own images after it commits.
            let facts = storage::acquire_facts(&reader, compass_rank9)
                .context("read Compass fact collection")?;
            let relations = relations_rank9
                .map(|rank9| {
                    storage::acquire_facts(&reader, rank9)
                        .context("read Relations fact collection for Compass persona and mentions")
                })
                .transpose()?;
            let by = match (persona, &relations) {
                (Some(persona), Some(relations)) => {
                    Some(resolve_persona_id(relations, &reader, persona)?)
                }
                _ => None,
            };
            let mut resolved = Mentions::default();
            if let Some(relations) = &relations {
                for label in mentions {
                    let key = relations::lookup_key(label);
                    if resolved.0.contains_key(&key) {
                        continue;
                    }
                    let targets = relations::mention_targets(&reader, relations, label)?;
                    resolved.0.insert(key, (label.clone(), targets));
                }
            }
            let prepared = prepare(&facts, &reader, by, &resolved)?;
            let (fragment, value) = author(prepared)?;
            if let Some(fragment) = fragment {
                crate::collection_names::require_command_write_admission_acquiring(
                    pile,
                    compass_source,
                    signer,
                    "Compass",
                    "compass list",
                    runtime,
                )?;
                pile.commit(compass_source, signer, fragment)
                    .context("commit Compass collection fragment")?;
                let status = compass::status_register_for_source(pile, compass_source)?;
                runtime
                    .block_on(async {
                        storage::seed_attached(pile, status, signer).await?;
                        storage::ensure_downstream(pile, compass_source, signer).await?;
                        Ok::<_, anyhow::Error>(())
                    })
                    .context(
                        "Compass fragment was committed, but ensuring its derived views failed",
                    )?;
            }
            Ok(value)
        })
    }
}

/// Every distinct title of a goal, sorted by text.
///
/// The schema cannot express cardinality, so a goal may hold several titles
/// (two writers, a merge). Each is read; none is chosen over another. A goal
/// whose title has not arrived has none.
fn task_titles<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    task_id: Id,
) -> Result<Vec<String>> {
    let mut titles: Vec<String> =
        find!(h: TextHandle, pattern!(space, [{ task_id @ board::title: ?h }]))
            .map(|handle| read_text(reader, handle))
            .collect::<Result<_>>()?;
    titles.sort();
    titles.dedup();
    Ok(titles)
}

/// A goal's titles as one line: each distinct title, sorted, joined by
/// " / ". One title reads as itself, and no title as the empty string.
fn task_title<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    task_id: Id,
) -> Result<String> {
    Ok(task_titles(reader, space, task_id)?.join(" / "))
}

/// The text of a status or tag word. A word whose handle is that of one of
/// `known` (the default statuses, and whatever a filter named) takes that text
/// without fetching its blob; any other is read.
fn word(reader: &impl BlobStoreGet, handle: TextHandle, known: &[&str]) -> Result<String> {
    match known
        .iter()
        .find(|word| crate::schemas::compass::text_handle(word) == handle)
    {
        Some(word) => Ok((*word).to_owned()),
        None => read_text(reader, handle),
    }
}

fn task_tags<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    task_id: Id,
    known: &[&str],
) -> Result<Vec<String>> {
    let mut tags: Vec<String> = find!(
        tag: TextHandle,
        pattern!(space, [{ task_id @ metadata::tag: &KIND_GOAL_ID, board::tag: ?tag }])
    )
    .map(|tag| word(reader, tag, known))
    .collect::<Result<_>>()?;
    tags.sort();
    tags.dedup();
    Ok(tags)
}

/// Every parent a goal names, in id order. A parent need not be present.
fn task_parents<P: TriblePattern>(space: &P, task_id: Id) -> Vec<Id> {
    let mut parents: Vec<Id> =
        find!(p: Id, pattern!(space, [{ task_id @ board::parent: ?p }])).collect();
    parents.sort();
    parents.dedup();
    parents
}

/// When a goal was created: the earliest of its creation times that decodes.
fn task_created_at<P: TriblePattern>(space: &P, task_id: Id) -> Option<IntervalValue> {
    find!(s: IntervalValue, pattern!(space, [{ task_id @ metadata::created_at: ?s }]))
        .filter_map(|at| Some((interval_key(at)?, at)))
        .min_by_key(|(key, at)| (*key, at.raw))
        .map(|(_, at)| at)
}

/// Latest status for a task.
fn task_latest_status<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    status_register: &LwwQuery,
    task_id: Id,
) -> Result<Option<(String, IntervalValue)>> {
    latest_status_event(space, status_register, task_id)
        .map(|(_, status, at)| Ok((word(reader, status, &DEFAULT_STATUSES)?, at)))
        .transpose()
}

/// All goal IDs.
fn all_goal_ids<P: TriblePattern>(space: &P) -> Vec<Id> {
    find!(id: Id, pattern!(space, [{ ?id @ metadata::tag: &KIND_GOAL_ID }])).collect()
}

fn read_text(reader: &impl BlobStoreGet, handle: TextHandle) -> Result<String> {
    compass::read_text(reader, handle)
}

/// Parse a full 32-char hex ID. Returns a helpful error pointing to `compass resolve` on failure.
fn resolve_task_id<P: TriblePattern>(input: &str, space: &P) -> Result<Id> {
    crate::resolve_id_prefix(input, all_goal_ids(space))
}

fn resolve_note_id<P: TriblePattern>(input: &str, space: &P) -> Result<Id> {
    let trimmed = input.trim();
    if trimmed.len() != 32 {
        bail!("supersedes requires a full 32-char note id: '{trimmed}'");
    }
    let note_id =
        Id::from_hex(trimmed).ok_or_else(|| anyhow::anyhow!("invalid note id '{trimmed}'"))?;
    // A note is a note whether or not the goal it names is in this view.
    if !compass::note_ids(space).contains(&note_id) {
        bail!("supersedes target is not an existing note: '{trimmed}'");
    }
    Ok(note_id)
}

fn parent_paths<P: TriblePattern>(space: &P) -> Result<PathIndex> {
    let parent_plus = PathExpr::from(Step::Forward(board::parent.id().into()))
        .plus()
        .compile();
    let edges: TribleSet = find!(
        (child: Id, parent: Id),
        pattern!(space, [{ ?child @ board::parent: ?parent }])
    )
    .map(|(child, parent)| {
        let parent: Inline<inlineencodings::GenId> = parent.to_inline();
        Trible::force(&child, &board::parent.id(), &parent)
    })
    .collect();
    PathIndex::from_tribles(parent_plus, edges.iter())
        .map_err(|error| anyhow::anyhow!("materialize goal-parent ancestry: {error}"))
}

/// Check if `to` is an ancestor of `from` (or `from` itself) in the parent tree.
fn is_ancestor(paths: &PathIndex, from: Id, to: Id) -> bool {
    if from == to {
        return true;
    }
    let from: Inline<inlineencodings::GenId> = from.to_inline();
    let to: Inline<inlineencodings::GenId> = to.to_inline();
    paths.contains(&from.raw, &to.raw)
}

/// Count the distinct notes on a task; a note with two bodies is one note.
fn note_count<P: TriblePattern>(space: &P, task_id: Id) -> usize {
    find!(
        note: Id,
        pattern!(space, [{ ?note @ metadata::tag: &KIND_NOTE_ID, board::task: &task_id, board::note: _?body }])
    )
    .collect::<BTreeSet<_>>()
    .len()
}

/// Every persona an event names as its author, in id order.
fn event_actors<P: TriblePattern>(space: &P, event_id: Id) -> Vec<Id> {
    let mut actors: Vec<Id> =
        find!(by: Id, pattern!(space, [{ event_id @ board::by: ?by }])).collect();
    actors.sort();
    actors.dedup();
    actors
}

fn note_tags<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    note_id: Id,
) -> Result<Vec<String>> {
    let mut tags: Vec<String> =
        find!(tag: TextHandle, pattern!(space, [{ note_id @ board::tag: ?tag }]))
            .map(|tag| read_text(reader, tag))
            .collect::<Result<_>>()?;
    tags.sort();
    tags.dedup();
    Ok(tags)
}

fn note_references<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    note_id: Id,
) -> Result<Vec<String>> {
    let mut references: Vec<String> = find!(
        handle: TextHandle,
        pattern!(space, [{ note_id @ board::reference: ?handle }])
    )
    .map(|handle| read_text(reader, handle))
    .collect::<Result<_>>()?;
    references.sort();
    references.dedup();
    Ok(references)
}

fn note_supersedes<P: TriblePattern>(space: &P, note_id: Id) -> Vec<Id> {
    let mut predecessors: Vec<Id> = find!(
        predecessor: Id,
        pattern!(space, [{ note_id @ metadata::supersedes: ?predecessor }])
    )
    .collect();
    predecessors.sort();
    predecessors.dedup();
    predecessors
}

fn render_board<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    status_register: &LwwQuery,
    status_filter: &[String],
    tag_filter: &[String],
    show_done: bool,
) -> Result<String> {
    let goal_ids = all_goal_ids(space);
    let priority_ranks = compass::priority_ranks(
        goal_ids.iter().copied(),
        &compass::goal_priority_edges(space),
    );

    let mut columns: HashMap<String, Vec<TaskRow>> = HashMap::new();
    let text_handle = crate::schemas::compass::text_handle;
    let mut known: Vec<&str> = DEFAULT_STATUSES.to_vec();
    known.extend(status_filter.iter().map(String::as_str));
    known.extend(tag_filter.iter().map(String::as_str));

    for &task_id in &goal_ids {
        // Filter by content handle, so a goal the filters drop costs no fetch.
        let latest = latest_status_event(space, status_register, task_id);
        let status_handle = latest
            .map(|(_, status, _)| status)
            .unwrap_or_else(|| text_handle("todo"));
        if status_filter.is_empty() {
            if !show_done && status_handle == text_handle("done") {
                continue;
            }
        } else if !status_filter
            .iter()
            .any(|s| text_handle(s) == status_handle)
        {
            continue;
        }
        if !tag_filter.is_empty()
            && !tag_filter.iter().any(|t| {
                exists!(pattern!(space, [{
                    task_id @ metadata::tag: &KIND_GOAL_ID, board::tag: t.as_str()
                }]))
            })
        {
            continue;
        }

        let status = word(reader, status_handle, &known)?;
        let status_at = latest.map(|(_, _, at)| at);
        let tags = task_tags(reader, space, task_id, &known)?;

        let title = task_title(reader, space, task_id)?;
        let created_at = task_created_at(space, task_id);
        let notes = note_count(space, task_id);
        let parents = task_parents(space, task_id);

        let sort_key = status_at
            .and_then(interval_key)
            .or_else(|| created_at.and_then(interval_key))
            .unwrap_or(0);
        columns.entry(status).or_default().push(TaskRow {
            id: task_id,
            id_hex: fmt_id(task_id),
            title,
            tags,
            sort_key,
            note_count: notes,
            parents,
        });
    }

    let mut ordered_statuses = Vec::new();
    for status in DEFAULT_STATUSES {
        if columns.contains_key(status) {
            ordered_statuses.push(status.to_string());
        }
    }
    let mut extras: Vec<String> = columns
        .keys()
        .filter(|s| !DEFAULT_STATUSES.contains(&s.as_str()))
        .cloned()
        .collect();
    extras.sort();
    ordered_statuses.extend(extras);

    if ordered_statuses.is_empty() {
        return Ok("No goals yet.\n".to_owned());
    }

    let mut output = String::new();
    for status in ordered_statuses {
        let rows = columns.remove(&status).unwrap_or_default();
        writeln!(output)?;
        writeln!(output, "== {} ({}) ==", status.to_uppercase(), rows.len())?;
        let ordered = order_rows(rows, &priority_ranks);
        for (row, depth) in ordered {
            let indent = "  ".repeat(depth);
            writeln!(
                output,
                "{}- [{}] {}{}{}",
                indent,
                row.id_hex,
                row.title,
                row.tag_suffix(),
                row.note_suffix()
            )?;
        }
    }
    writeln!(output)?;
    Ok(output)
}

#[derive(Debug, Clone)]
struct TaskRow {
    id: Id,
    id_hex: String,
    title: String,
    tags: Vec<String>,
    sort_key: i128,
    note_count: usize,
    /// Every parent the goal names, in id order; it nests under the first
    /// one the board also shows.
    parents: Vec<Id>,
}

#[derive(Debug)]
struct NoteRow {
    id: Id,
    text: String,
    sort_key: i128,
    at: String,
    by: Vec<Id>,
    tags: Vec<String>,
    references: Vec<String>,
    supersedes: Vec<Id>,
}

impl TaskRow {
    fn tag_suffix(&self) -> String {
        if self.tags.is_empty() {
            String::new()
        } else {
            format!(
                " {}",
                self.tags
                    .iter()
                    .map(|t| format!("#{t}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
    }

    fn note_suffix(&self) -> String {
        if self.note_count == 0 {
            String::new()
        } else if self.note_count == 1 {
            " (1 note)".to_string()
        } else {
            format!(" ({} notes)", self.note_count)
        }
    }
}

fn order_rows(rows: Vec<TaskRow>, ranks: &BTreeMap<Id, usize>) -> Vec<(TaskRow, usize)> {
    let mut by_id: HashMap<Id, TaskRow> = HashMap::new();
    for row in rows {
        by_id.insert(row.id, row);
    }
    let ids: HashSet<Id> = by_id.keys().copied().collect();
    let mut children: HashMap<Id, Vec<Id>> = HashMap::new();
    let mut roots = Vec::new();

    for (id, row) in &by_id {
        if let Some(parent) = row.parents.iter().find(|parent| ids.contains(*parent)) {
            children.entry(*parent).or_default().push(*id);
            continue;
        }
        roots.push(*id);
    }

    let sort_ids = |items: &mut Vec<Id>| {
        items.sort_by(|a, b| {
            let a_rank = ranks.get(a).copied().unwrap_or(usize::MAX);
            let b_rank = ranks.get(b).copied().unwrap_or(usize::MAX);
            match a_rank.cmp(&b_rank) {
                std::cmp::Ordering::Equal => {
                    // Fall back to timestamp (most recent first)
                    let a_key = by_id.get(a).map(|row| row.sort_key).unwrap_or(0);
                    let b_key = by_id.get(b).map(|row| row.sort_key).unwrap_or(0);
                    b_key.cmp(&a_key).then_with(|| a.cmp(b))
                }
                other => other,
            }
        });
    };

    sort_ids(&mut roots);
    for kids in children.values_mut() {
        sort_ids(kids);
    }

    let mut ordered = Vec::new();
    let mut visited = HashSet::new();

    fn walk(
        id: Id,
        depth: usize,
        by_id: &HashMap<Id, TaskRow>,
        children: &HashMap<Id, Vec<Id>>,
        visited: &mut HashSet<Id>,
        out: &mut Vec<(TaskRow, usize)>,
    ) {
        if !visited.insert(id) {
            return;
        }
        let Some(row) = by_id.get(&id) else {
            return;
        };
        out.push((row.clone(), depth));
        if let Some(kids) = children.get(&id) {
            for kid in kids {
                walk(*kid, depth + 1, by_id, children, visited, out);
            }
        }
    }

    for root in roots {
        walk(root, 0, &by_id, &children, &mut visited, &mut ordered);
    }

    // Whatever no root reached sits on a parent cycle, which opaque ids can
    // form; it is shown from its smallest id, so the order is the same on
    // every read.
    let mut rest: Vec<Id> = by_id.keys().copied().collect();
    rest.sort();
    for id in rest {
        if !visited.contains(&id) {
            walk(id, 0, &by_id, &children, &mut visited, &mut ordered);
        }
    }

    ordered
}

/// Resolve one active Relations person for attribution. The flag remains a
/// cooperative authorship claim, but it cannot name an unknown or retired
/// anchor.
fn resolve_persona_id<P: TriblePattern>(
    space: &P,
    reader: &impl BlobStoreGet,
    input: &str,
) -> Result<Id> {
    relations::resolve_person(reader, space, input, false)?.require_unique("persona", input)
}

fn add_goal(
    storage: CompassStorage<'_>,
    title: String,
    status: String,
    parent: Option<String>,
    tags: Vec<String>,
    note: Option<String>,
    persona: Option<&str>,
) -> Result<AddedGoal> {
    let status = compass::canonical_status(status)?;
    let tags = compass::canonical_tags(tags)?;
    let mut labels = compass::mention_labels(&title);
    labels.extend(
        note.as_deref()
            .map(compass::mention_labels)
            .unwrap_or_default(),
    );
    storage.update(
        persona,
        &labels,
        |space, _reader, by_id, mentions| {
            let parent_id = parent
                .as_deref()
                .map(|parent| resolve_task_id(parent, space))
                .transpose()?;
            Ok((parent_id, by_id, mentions.clone()))
        },
        |(parent_id, by_id, mentions)| {
            let now = clock::point_now()?;
            let mut change = compass::kind_catalog_fragment();
            let title_mentions = mentions.targets(&title);
            let (goal, task_ref) = compass::goal_fragment(title, tags, parent_id, now)?;
            change += goal;
            change += compass::mention_fragment(task_ref, &title_mentions);
            let status = compass::status_fragment(task_ref, status, by_id, now)?;
            let status_event = status.root().expect("status event exports one root");
            change += status;

            let mut note_ref = None;
            if let Some(note) = note {
                let references = extract_reference_values(&note);
                let note_mentions = mentions.targets(&note);
                let (record, note_id) =
                    compass::note_fragment(task_ref, note, vec![], references, vec![], by_id, now)?;
                change += record;
                change += compass::mention_fragment(note_id, &note_mentions);
                note_ref = Some(note_id);
            }
            Ok((
                Some(change),
                AddedGoal {
                    goal: task_ref,
                    status_event,
                    note: note_ref,
                    unresolved_mentions: mentions.unresolved(),
                },
            ))
        },
    )
}

fn list(
    storage: CompassStorage<'_>,
    status_filter: Vec<String>,
    tag_filter: Vec<String>,
    show_done: bool,
) -> Result<String> {
    let status_filter: Vec<String> = status_filter.into_iter().map(normalize_status).collect();
    for status in &status_filter {
        validate_label("status", status)?;
    }

    storage.with_view(|space, reader, status_register| {
        render_board(
            reader,
            space,
            status_register,
            &status_filter,
            &tag_filter,
            show_done,
        )
    })
}

fn move_goal(
    storage: CompassStorage<'_>,
    id: String,
    status: String,
    persona: Option<&str>,
) -> Result<MovedGoal> {
    let status = compass::canonical_status(status)?;
    let rendered_status = status.clone();
    storage.update(
        persona,
        &[],
        |space, _reader, by_id, _| Ok((resolve_task_id(&id, space)?, by_id)),
        |(task_id, by_id)| {
            let mut change = compass::kind_catalog_fragment();
            let record = compass::status_fragment(task_id, status, by_id, clock::point_now()?)?;
            let event = record.root().expect("status event exports one root");
            change += record;
            Ok((
                Some(change),
                MovedGoal {
                    goal: task_id,
                    event,
                    status: rendered_status,
                },
            ))
        },
    )
}

fn add_note(
    storage: CompassStorage<'_>,
    id: String,
    note: String,
    tags: Vec<String>,
    mut references: Vec<String>,
    supersedes: Vec<String>,
    persona: Option<&str>,
) -> Result<AddedNote> {
    let tags = compass::canonical_tags(tags)?;
    if let Some(reference) = references
        .iter()
        .find(|reference| reference.trim().is_empty())
    {
        bail!("reference must not be empty: {reference:?}");
    }
    references.extend(extract_reference_values(&note));
    references.sort();
    references.dedup();

    let labels = compass::mention_labels(&note);
    storage.update(
        persona,
        &labels,
        |space, _reader, by_id, mentions| {
            let task_id = resolve_task_id(&id, space)?;
            let superseded_ids: Vec<Id> = supersedes
                .iter()
                .map(|input| resolve_note_id(input, space))
                .collect::<Result<_>>()?;
            Ok((task_id, superseded_ids, by_id, mentions.clone()))
        },
        |(task_id, superseded_ids, by_id, mentions)| {
            let now = clock::point_now()?;
            let mut change = compass::kind_catalog_fragment();
            let note_mentions = mentions.targets(&note);
            let (record, note_id) = compass::note_fragment(
                task_id,
                note,
                tags,
                references,
                superseded_ids,
                by_id,
                now,
            )?;
            change += record;
            change += compass::mention_fragment(note_id, &note_mentions);
            Ok((
                Some(change),
                AddedNote {
                    goal: task_id,
                    note: note_id,
                    unresolved_mentions: mentions.unresolved(),
                },
            ))
        },
    )
}

fn render_goal<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    status_register: &LwwQuery,
    task_id: Id,
) -> Result<String> {
    let mut output = String::new();
    writeln!(output, "Goal {:x}", task_id)?;
    // One line per title; a goal whose title has not arrived has none.
    for title in task_titles(reader, space, task_id)? {
        writeln!(output, "Title: {}", title)?;
    }
    if let Some(created) = task_created_at(space, task_id).and_then(format_interval) {
        writeln!(output, "Created: {}", created)?;
    }

    if let Some((status, at)) = task_latest_status(reader, space, status_register, task_id)? {
        match format_interval(at) {
            Some(at) => writeln!(output, "Status: {} (since {})", status, at)?,
            None => writeln!(output, "Status: {}", status)?,
        }
    }

    let tags = task_tags(reader, space, task_id, &[])?;
    if !tags.is_empty() {
        writeln!(output, "Tags: {}", tags.join(", "))?;
    }

    for parent_id in task_parents(space, task_id) {
        let parent_hex = fmt_id(parent_id);
        let parent_title = task_title(reader, space, parent_id)?;
        if parent_title.is_empty() {
            writeln!(output, "Parent: {parent_hex}")?;
        } else {
            writeln!(output, "Parent: {parent_title} ({parent_hex})")?;
        }
    }

    // Status history for this task: one row per complete reading of each
    // event, ordered by time, then event id, then word. A row whose time does
    // not decode is skipped.
    let mut history: Vec<(i128, Id, String, String, Vec<Id>)> = Vec::new();
    for (event, status, at) in find!(
        (event: Id, status: TextHandle, at: IntervalValue),
        pattern!(space, [{
            ?event @
            metadata::tag: &KIND_STATUS_ID,
            board::status_of: &task_id,
            board::status: ?status,
            metadata::created_at: ?at,
        }])
    ) {
        let (Some(key), Some(at)) = (interval_key(at), format_interval(at)) else {
            continue;
        };
        history.push((
            key,
            event,
            word(reader, status, &DEFAULT_STATUSES)?,
            at,
            event_actors(space, event),
        ));
    }
    if !history.is_empty() {
        history.sort();
        writeln!(output)?;
        writeln!(output, "Status history:")?;
        for (_, _, status, at, by) in &history {
            if by.is_empty() {
                writeln!(output, "- {at} {status}")?;
            } else {
                writeln!(output, "- {at} {status} by {}", fmt_ids(by))?;
            }
        }
    }

    // Notes for this task: one row per complete reading of each note (a note
    // with two bodies shows both under its one id), ordered by time, then
    // note id, then text. A row whose time does not decode is skipped.
    let mut notes: Vec<NoteRow> = Vec::new();
    for (note_id, handle, at) in find!(
        (note_id: Id, note_handle: TextHandle, at: IntervalValue),
        pattern!(space, [{
            ?note_id @
            metadata::tag: &KIND_NOTE_ID,
            board::task: &task_id,
            board::note: ?note_handle,
            metadata::created_at: ?at,
        }])
    ) {
        let (Some(sort_key), Some(at)) = (interval_key(at), format_interval(at)) else {
            continue;
        };
        notes.push(NoteRow {
            id: note_id,
            text: read_text(reader, handle)?,
            sort_key,
            at,
            by: event_actors(space, note_id),
            tags: note_tags(reader, space, note_id)?,
            references: note_references(reader, space, note_id)?,
            supersedes: note_supersedes(space, note_id),
        });
    }
    if !notes.is_empty() {
        notes.sort_by(|a, b| (a.sort_key, a.id, &a.text).cmp(&(b.sort_key, b.id, &b.text)));
        writeln!(output)?;
        writeln!(output, "Notes:")?;
        for note in &notes {
            if note.by.is_empty() {
                writeln!(output, "- [{}] {}", fmt_id(note.id), note.at)?;
            } else {
                writeln!(
                    output,
                    "- [{}] {} by {}",
                    fmt_id(note.id),
                    note.at,
                    fmt_ids(&note.by)
                )?;
            }
            if note.text.is_empty() {
                writeln!(output, "  (empty)")?;
            } else {
                for line in note.text.lines() {
                    writeln!(output, "  {line}")?;
                }
            }
            if !note.tags.is_empty() {
                writeln!(
                    output,
                    "  tags: {}",
                    note.tags
                        .iter()
                        .map(|tag| format!("#{tag}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )?;
            }
            if !note.references.is_empty() {
                writeln!(output, "  refs: {}", note.references.join(", "))?;
            }
            if !note.supersedes.is_empty() {
                writeln!(
                    output,
                    "  supersedes: {}",
                    note.supersedes
                        .iter()
                        .map(|id| fmt_id(*id))
                        .collect::<Vec<_>>()
                        .join(", ")
                )?;
            }
        }

        let mut all_refs = Vec::new();
        for note in &notes {
            all_refs.extend(extract_references(&note.text));
        }
        all_refs.sort();
        all_refs.dedup();
        if !all_refs.is_empty() {
            writeln!(output)?;
            writeln!(output, "References:")?;
            for (faculty, hex) in &all_refs {
                writeln!(output, "  ⇢ {faculty}:{hex}")?;
            }
        }
    }
    Ok(output)
}

fn show(storage: CompassStorage<'_>, id: String) -> Result<String> {
    storage.with_view(|space, reader, status_register| {
        let task_id = resolve_task_id(&id, space)?;
        render_goal(reader, space, status_register, task_id)
    })
}

fn prioritize(
    storage: CompassStorage<'_>,
    higher_input: String,
    lower_input: String,
) -> Result<PriorityChange> {
    storage.update(
        None,
        &[],
        |space, reader, _, _| {
            let higher_id = resolve_task_id(&higher_input, space)?;
            let lower_id = resolve_task_id(&lower_input, space)?;

            if higher_id == lower_id {
                bail!("cannot prioritize a goal over itself");
            }

            // Build full edge set (explicit + implicit child→parent)
            let edges = compass::goal_priority_edges(space);

            if compass::would_create_priority_cycle(&edges, higher_id, lower_id) {
                let paths = parent_paths(space)?;
                if is_ancestor(&paths, higher_id, lower_id)
                    || is_ancestor(&paths, lower_id, higher_id)
                {
                    bail!("children are implicitly prioritized over their parents");
                }
                bail!("would create a priority cycle");
            }

            Ok((
                higher_id,
                lower_id,
                task_title(reader, space, higher_id)?,
                task_title(reader, space, lower_id)?,
            ))
        },
        |(higher_id, lower_id, higher_title, lower_title)| {
            let mut change = compass::kind_catalog_fragment();
            let record = compass::priority_fragment(higher_id, lower_id, true, clock::point_now()?);
            let event = record.root().expect("priority event exports one root");
            change += record;
            Ok((
                Some(change),
                PriorityChange {
                    higher: higher_id,
                    lower: lower_id,
                    event,
                    active: true,
                    higher_title,
                    lower_title,
                },
            ))
        },
    )
}

fn deprioritize(
    storage: CompassStorage<'_>,
    higher_input: String,
    lower_input: String,
) -> Result<PriorityChange> {
    storage.update(
        None,
        &[],
        |space, reader, _, _| {
            let higher_id = resolve_task_id(&higher_input, space)?;
            let lower_id = resolve_task_id(&lower_input, space)?;

            let edges = compass::active_priority_edges(space);
            if !edges.contains(&(higher_id, lower_id)) {
                bail!("no active priority relationship between these goals");
            }

            Ok((
                higher_id,
                lower_id,
                task_title(reader, space, higher_id)?,
                task_title(reader, space, lower_id)?,
            ))
        },
        |(higher_id, lower_id, higher_title, lower_title)| {
            let mut change = compass::kind_catalog_fragment();
            let record =
                compass::priority_fragment(higher_id, lower_id, false, clock::point_now()?);
            let event = record.root().expect("priority event exports one root");
            change += record;
            Ok((
                Some(change),
                PriorityChange {
                    higher: higher_id,
                    lower: lower_id,
                    event,
                    active: false,
                    higher_title,
                    lower_title,
                },
            ))
        },
    )
}

fn resolve(storage: CompassStorage<'_>, prefix: String) -> Result<Id> {
    storage.with_view(|space, _reader, _status_register| resolve_task_id(&prefix, space))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::initialize_signer;
    use anybytes::Bytes;
    use ed25519_dalek::SigningKey;
    use std::convert::Infallible;
    use std::future::{ready, Future};
    use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
    use triblespace::core::blob::encodings::UnknownBlob;
    use triblespace::core::blob::MemoryBlobStoreSnapshot;
    use triblespace::core::repo::async_store::AsyncBlobStoreAcquire;
    use triblespace::core::repo::pile::ReadError;
    use triblespace::core::repo::MissingBlob;

    type BlobHandle = Inline<inlineencodings::Handle<UnknownBlob>>;

    /// A real resident-only pile with an exact-handle remote byte fixture.
    struct AcquiringPile {
        pile: Pile,
        remote: MemoryBlobStoreSnapshot,
        requested: Vec<BlobHandle>,
        arriving: Option<(Collection<SimpleArchive>, SigningKey, Fragment)>,
        _file: tempfile::NamedTempFile,
    }

    impl AcquiringPile {
        fn new(mut remote: MemoryBlobStore, host: ed25519_dalek::VerifyingKey) -> Self {
            let file = tempfile::NamedTempFile::new().unwrap();
            Self {
                pile: Pile::open_as(file.path(), host).unwrap(),
                remote: remote.snapshot().unwrap(),
                requested: Vec::new(),
                arriving: None,
                _file: file,
            }
        }
    }

    impl SnapshotSource for AcquiringPile {
        type Snapshot = PileSnapshot;
        type SnapshotError = ReadError;

        fn snapshot(&mut self) -> Result<PileSnapshot, ReadError> {
            self.pile.snapshot()
        }
    }

    impl AsyncBlobStoreAcquire for AcquiringPile {
        type AcquireError = Infallible;

        fn acquire(
            &mut self,
            handle: BlobHandle,
        ) -> impl Future<Output = Result<Option<Bytes>, Infallible>> + Send {
            let resident = self.pile.snapshot().unwrap();
            if resident.contains_blob(handle).unwrap() {
                return ready(Ok(Some(resident.get(handle).unwrap())));
            }
            self.requested.push(handle);
            if let Some((collection, signer, fragment)) = self.arriving.take() {
                self.pile.commit(collection, &signer, fragment).unwrap();
            }
            if !self.remote.contains_blob(handle).unwrap() {
                return ready(Ok(None));
            }
            let bytes: Bytes = self.remote.get(handle).unwrap();
            let cached: BlobHandle = self.pile.put(bytes.clone()).unwrap();
            assert_eq!(cached, handle);
            ready(Ok(Some(bytes)))
        }
    }

    /// What the maintenance worker does between a write and a read: carry
    /// the source's commits through the chain and the status register. Reads
    /// attach what was carried and never maintain.
    fn carry(pile: &mut Pile, source: Collection<SimpleArchive>, signer: &SigningKey) {
        let succinct = pile.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
        let rank9 = pile
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
            .unwrap();
        let status = compass::status_register_collection(pile, signer.verifying_key()).unwrap();
        pollster::block_on(async {
            drop(pile.maintain_attached(succinct, signer).await.unwrap());
            drop(pile.maintain_attached(rank9, signer).await.unwrap());
            drop(pile.maintain_attached(status, signer).await.unwrap());
        });
    }

    fn carry_storage(storage: CompassStorage<'_>) {
        storage
            .storage
            .with_pile(|pile, signer| {
                let source = open_configured(pile, COMPASS_SCOPE_ID, signer.verifying_key())?;
                carry(pile, source, signer);
                Ok(())
            })
            .unwrap();
    }

    fn sparse_view(
        mut fragment: Fragment,
    ) -> (
        AcquiringPile,
        compass::CompassSnapshot,
        Collection<SimpleArchive>,
        SigningKey,
    ) {
        let signer = SigningKey::from_bytes(&[7; 32]);
        let mut store = AcquiringPile::new(fragment.blobs().clone(), signer.verifying_key());
        fragment.blobs_mut().keep([]);
        let source = crate::collection_names::open(
            &mut store.pile,
            COMPASS_SCOPE_ID,
            signer.verifying_key(),
        )
        .unwrap();
        store.pile.commit(source, &signer, fragment).unwrap();
        carry(&mut store.pile, source, &signer);
        let view = pollster::block_on(compass::materialize_indexed_collection(
            &mut store.pile,
            &signer,
        ))
        .unwrap();
        (store, view, source, signer)
    }

    #[test]
    fn configured_descriptor_read_acquires_descriptor_and_name_only() {
        let mut remote = MemoryRepo::default();
        let signer = SigningKey::from_bytes(&[8; 32]);
        let source =
            crate::collection_names::open(&mut remote, COMPASS_SCOPE_ID, signer.verifying_key())
                .unwrap();
        let descriptor: TribleSet = remote.snapshot().unwrap().get(source.handle()).unwrap();
        let name = triblespace::core::collection::descriptor::name(&descriptor)
            .unwrap()
            .unwrap();
        let mut store = AcquiringPile::new(remote.into_inner().blobs, signer.verifying_key());
        let before = store.snapshot().unwrap();

        let opened = pollster::block_on(storage::read(&mut store, &before, |reader| {
            open_exact_in(reader, COMPASS_SCOPE_ID, source.handle())
        }))
        .unwrap();

        assert_eq!(opened, source);
        assert_eq!(
            store.requested,
            vec![source.handle().transmute(), name.transmute()]
        );
        assert!(!before.contains_blob(source.handle()).unwrap());
        assert_eq!(store.snapshot().unwrap().wants().unwrap().count(), 0);
    }

    #[test]
    fn missing_title_and_reference_keep_the_typed_missing_blob_error() {
        let at: IntervalValue = (Epoch::from_tai_seconds(0.0), Epoch::from_tai_seconds(0.0))
            .try_to_inline()
            .unwrap();
        let (mut fragment, goal) =
            compass::goal_fragment("selected goal", vec![], None, at).unwrap();
        let (note, note_id) = compass::note_fragment(
            goal,
            "selected note",
            vec![],
            vec!["git:DEADBEEF".to_owned()],
            vec![],
            None,
            at,
        )
        .unwrap();
        fragment += note;
        let title = find!(handle: TextHandle, pattern!(fragment.facts(), [{ goal @ board::title: ?handle }]))
            .next().unwrap();
        let reference = find!(handle: TextHandle, pattern!(fragment.facts(), [{ note_id @ board::reference: ?handle }]))
            .next().unwrap();
        let (_, view, _, _) = sparse_view(fragment);

        let missing_title = task_title(view.store_snapshot(), view.facts(), goal).unwrap_err();
        let missing_reference =
            note_references(view.store_snapshot(), view.facts(), note_id).unwrap_err();
        for (error, expected) in [(missing_title, title), (missing_reference, reference)] {
            assert_eq!(
                error
                    .chain()
                    .find_map(|source| source.downcast_ref::<MissingBlob>())
                    .unwrap()
                    .handle,
                expected.transmute()
            );
        }
        // An absent title fact retains its existing open-world meaning.
        assert_eq!(
            task_title(view.store_snapshot(), &TribleSet::new(), goal).unwrap(),
            ""
        );
    }

    #[test]
    fn board_fetches_only_titles_surviving_status_and_tag_filters() {
        let at: IntervalValue = (Epoch::from_tai_seconds(0.0), Epoch::from_tai_seconds(0.0))
            .try_to_inline()
            .unwrap();
        let (mut fragment, selected) =
            compass::goal_fragment("selected goal", vec!["shown".to_owned()], None, at).unwrap();
        fragment += compass::status_fragment(selected, "doing", None, at).unwrap();
        let (hidden, _) =
            compass::goal_fragment("other tag goal", vec!["other".to_owned()], None, at).unwrap();
        fragment += hidden;
        let (done, done_id) =
            compass::goal_fragment("done goal", vec!["shown".to_owned()], None, at).unwrap();
        fragment += done;
        fragment += compass::status_fragment(done_id, "done", None, at).unwrap();
        fragment += compass::note_fragment(
            selected,
            "note not needed by list",
            vec![],
            vec!["git:DEADBEEF".to_owned()],
            vec![],
            None,
            at,
        )
        .unwrap()
        .0;
        let title = find!(handle: TextHandle, pattern!(fragment.facts(), [{ selected @ board::title: ?handle }]))
            .next().unwrap();
        let (mut store, view, _, _) = sparse_view(fragment);

        let output =
            pollster::block_on(storage::read(&mut store, view.store_snapshot(), |reader| {
                render_board(
                    reader,
                    view.facts(),
                    view.status_register(),
                    &[],
                    &["shown".to_owned()],
                    false,
                )
            }))
            .unwrap();

        assert!(output.contains("selected goal"));
        assert!(output.contains("DOING (1)"));
        assert!(!output.contains("other tag goal"));
        assert!(!output.contains("done goal"));
        assert_eq!(store.requested, vec![title.transmute()]);
        assert!(!view.store_snapshot().contains_blob(title).unwrap());
        assert_eq!(store.snapshot().unwrap().wants().unwrap().count(), 0);
    }

    #[test]
    fn show_fetches_selected_goal_parent_note_and_reference_only() {
        let at: IntervalValue = (Epoch::from_tai_seconds(0.0), Epoch::from_tai_seconds(0.0))
            .try_to_inline()
            .unwrap();
        let (mut fragment, parent) =
            compass::goal_fragment("parent goal", vec![], None, at).unwrap();
        let (child, goal) =
            compass::goal_fragment("selected goal", vec![], Some(parent), at).unwrap();
        fragment += child;
        let (note, note_id) = compass::note_fragment(
            goal,
            "selected note",
            vec!["review".to_owned()],
            vec!["git:DEADBEEF".to_owned()],
            vec![],
            None,
            at,
        )
        .unwrap();
        fragment += note;
        let (unrelated, unrelated_goal) =
            compass::goal_fragment("unrelated goal", vec![], None, at).unwrap();
        fragment += unrelated;
        fragment += compass::note_fragment(
            unrelated_goal,
            "unrelated note",
            vec![],
            vec![],
            vec![],
            None,
            at,
        )
        .unwrap()
        .0;
        let mut expected: Vec<BlobHandle> = find!(handle: TextHandle,
            pattern!(fragment.facts(), [{ goal @ board::title: ?handle }]))
        .map(|handle| handle.transmute())
        .collect();
        expected.extend(
            find!(handle: TextHandle,
            pattern!(fragment.facts(), [{ parent @ board::title: ?handle }]))
            .map(|handle| handle.transmute()),
        );
        expected.extend(
            find!(handle: TextHandle,
            pattern!(fragment.facts(), [{ note_id @ board::note: ?handle }]))
            .map(|handle| handle.transmute()),
        );
        // The note's tag is shown, and a tag is text read from its blob.
        expected.extend(
            find!(handle: TextHandle,
            pattern!(fragment.facts(), [{ note_id @ board::tag: ?handle }]))
            .map(|handle| handle.transmute()),
        );
        expected.extend(
            find!(handle: TextHandle,
            pattern!(fragment.facts(), [{ note_id @ board::reference: ?handle }]))
            .map(|handle| handle.transmute()),
        );
        let (mut store, view, _, _) = sparse_view(fragment);

        let output =
            pollster::block_on(storage::read(&mut store, view.store_snapshot(), |reader| {
                render_goal(reader, view.facts(), view.status_register(), goal)
            }))
            .unwrap();

        assert!(output.contains("Title: selected goal"));
        assert!(output.contains("Parent: parent goal"));
        assert!(output.contains("selected note"));
        assert!(output.contains("refs: git:DEADBEEF"));
        assert!(!output.contains("unrelated"));
        assert_eq!(store.requested, expected);
        assert_eq!(store.snapshot().unwrap().wants().unwrap().count(), 0);
    }

    #[test]
    fn unavailable_note_or_reference_fails_the_whole_prepared_show() {
        let at: IntervalValue = (Epoch::from_tai_seconds(0.0), Epoch::from_tai_seconds(0.0))
            .try_to_inline()
            .unwrap();
        let (mut fragment, goal) =
            compass::goal_fragment("selected goal", vec![], None, at).unwrap();
        let (note, note_id) = compass::note_fragment(
            goal,
            "selected note",
            vec![],
            vec!["git:DEADBEEF".to_owned()],
            vec![],
            None,
            at,
        )
        .unwrap();
        fragment += note;
        let title = find!(handle: TextHandle, pattern!(fragment.facts(), [{ goal @ board::title: ?handle }]))
            .next().unwrap();
        let body = find!(handle: TextHandle, pattern!(fragment.facts(), [{ note_id @ board::note: ?handle }]))
            .next().unwrap();
        let reference = find!(handle: TextHandle, pattern!(fragment.facts(), [{ note_id @ board::reference: ?handle }]))
            .next().unwrap();

        for missing in [body, reference] {
            let (mut store, view, _, _) = sparse_view(fragment.clone());
            let mut remote = fragment.blobs().clone();
            remote.keep(
                [title, body, reference]
                    .into_iter()
                    .filter(|handle| *handle != missing)
                    .map(|handle| handle.transmute()),
            );
            store.remote = remote.snapshot().unwrap();
            let error =
                pollster::block_on(storage::read(&mut store, view.store_snapshot(), |reader| {
                    render_goal(reader, view.facts(), view.status_register(), goal)
                }))
                .unwrap_err();
            assert_eq!(
                error
                    .chain()
                    .find_map(|source| source.downcast_ref::<MissingBlob>())
                    .unwrap()
                    .handle,
                missing.transmute()
            );
            assert_eq!(store.requested.last(), Some(&missing.transmute()));
        }
    }

    #[test]
    fn render_retry_keeps_frozen_facts_status_and_support() {
        let at: IntervalValue = (Epoch::from_tai_seconds(0.0), Epoch::from_tai_seconds(0.0))
            .try_to_inline()
            .unwrap();
        let later: IntervalValue = (Epoch::from_tai_seconds(1.0), Epoch::from_tai_seconds(1.0))
            .try_to_inline()
            .unwrap();
        let (mut fragment, goal) =
            compass::goal_fragment("original goal", vec![], None, at).unwrap();
        fragment += compass::status_fragment(goal, "todo", None, at).unwrap();
        let (mut store, view, source, signer) = sparse_view(fragment);
        let original_support = source.admitted(view.store_snapshot()).unwrap();
        let (mut arrival, arrived_goal) =
            compass::goal_fragment("later goal", vec![], None, later).unwrap();
        arrival += compass::status_fragment(goal, "done", None, later).unwrap();
        store.arriving = Some((source, signer, arrival));

        let output =
            pollster::block_on(storage::read(&mut store, view.store_snapshot(), |reader| {
                render_board(
                    reader,
                    view.facts(),
                    view.status_register(),
                    &[],
                    &[],
                    false,
                )
            }))
            .unwrap();

        assert!(output.contains("original goal"));
        assert!(output.contains("TODO (1)"));
        assert!(!output.contains("later goal"));
        assert!(!compass::goal_ids(view.facts()).contains(&arrived_goal));
        assert_eq!(
            source.admitted(view.store_snapshot()).unwrap(),
            original_support
        );
        assert_eq!(original_support.len(), 1);
        assert_eq!(store.requested.len(), 1);
        let after = store.snapshot().unwrap();
        assert_eq!(source.admitted(&after).unwrap().len(), 2);
        assert_eq!(after.wants().unwrap().count(), 0);
    }

    #[test]
    fn parent_paths_preserve_reflexive_and_transitive_ancestry() {
        let root = Id::new([1; 16]).unwrap();
        let child = Id::new([2; 16]).unwrap();
        let grandchild = Id::new([3; 16]).unwrap();
        let unrelated = Id::new([4; 16]).unwrap();
        let mut space = TribleSet::new();
        space += entity! { ExclusiveId::force_ref(&child) @ board::parent: &root };
        space += entity! { ExclusiveId::force_ref(&grandchild) @ board::parent: &child };

        let paths = parent_paths(&space).unwrap();
        assert!(is_ancestor(&paths, grandchild, root));
        assert!(is_ancestor(&paths, child, root));
        assert!(is_ancestor(&paths, root, root));
        assert!(!is_ancestor(&paths, root, grandchild));
        assert!(!is_ancestor(&paths, grandchild, unrelated));
    }

    #[test]
    fn inline_references_are_exact_sorted_and_deduplicated() {
        assert_eq!(
            extract_reference_values(
                "[wiki](wiki:ABcd1234) [again](wiki:ABcd1234) [git](git:DEADBEEF)"
            ),
            ["git:DEADBEEF", "wiki:ABcd1234"]
        );
    }

    #[test]
    fn dangling_markdown_link_is_not_a_reference_or_a_panic() {
        assert!(extract_reference_values("unfinished ](").is_empty());
    }

    #[test]
    fn successive_actions_advance_the_views_a_reader_prepares() {
        let directory = tempfile::tempdir().unwrap();
        let pile_path = directory.path().join("eager-compass.pile");
        let key = directory.path().join("eager-compass.key");
        std::fs::File::create(&pile_path).unwrap();
        initialize_signer(&pile_path, Some(&key)).unwrap();
        let storage = Storage::new(pile_path.clone(), Some(key));
        let (succinct, rank9, status) = storage
            .with_pile(|pile, signer| {
                let source = open_configured(pile, COMPASS_SCOPE_ID, signer.verifying_key())?;
                let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
                let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
                let status = compass::status_register_collection(pile, signer.verifying_key())?;
                Ok((succinct, rank9, status))
            })
            .unwrap();
        // The reader prepares the pre-named targets; publication only commits.
        let observe = || {
            storage
                .with_pile(|pile, signer| {
                    let snapshot = pollster::block_on(async {
                        drop(pile.maintain_attached(succinct, signer).await?);
                        drop(pile.maintain_attached(rank9, signer).await?);
                        pile.maintain_attached(status, signer).await
                    })?;
                    Ok((
                        snapshot.read_facts(rank9)?,
                        snapshot
                            .attached(status)?
                            .read::<triblespace::core::collection::lww_register::LwwIndex>()?
                            .into_value()
                            .query()?,
                    ))
                })
                .unwrap()
        };
        let faculty = Compass::with_storage(storage.clone());
        let added = faculty.add("eager goal", AddOptions::default()).unwrap();
        let (first_facts, first_status) = observe();
        assert!(compass::goal_ids(&first_facts).contains(&added.goal));
        assert_eq!(
            latest_status_event(&first_facts, &first_status, added.goal)
                .unwrap()
                .1,
            crate::schemas::compass::text_handle("todo")
        );
        faculty
            .move_goal(&format!("{:x}", added.goal), "doing", None)
            .unwrap();
        let (facts, status) = observe();
        assert_eq!(
            latest_status_event(&facts, &status, added.goal).unwrap().1,
            crate::schemas::compass::text_handle("doing")
        );
        assert_eq!(
            latest_status_event(&first_facts, &first_status, added.goal)
                .unwrap()
                .1,
            crate::schemas::compass::text_handle("todo"),
            "previously selected views remain immutable"
        );
    }

    #[test]
    fn native_actions_accumulate_and_reads_do_not_write() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("compass.pile");
        let key = directory.path().join("compass.key");
        std::fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        let storage_handle = Storage::new(pile.clone(), Some(key.clone()));
        let storage = CompassStorage {
            storage: &storage_handle,
        };

        add_goal(
            storage,
            "A native goal".to_owned(),
            "todo".to_owned(),
            None,
            vec!["test".to_owned()],
            Some("first note".to_owned()),
            None,
        )
        .unwrap();
        let goal = storage
            .with_view(|facts, _, _| Ok(*compass::goal_ids(facts).iter().next().unwrap()))
            .unwrap();

        // Compass preserves unrelated open-world facts. Multiple values on
        // the order attribute alone do not form a status coordinate and must
        // not make the maintained register reject the exact cover.
        let unrelated = ufoid();
        let first: IntervalValue = {
            let value = Epoch::from_unix_seconds(1.0);
            (value, value).try_to_inline().unwrap()
        };
        let second: IntervalValue = {
            let value = Epoch::from_unix_seconds(2.0);
            (value, value).try_to_inline().unwrap()
        };
        let mut fragment = entity! { &unrelated @ metadata::created_at: first };
        fragment += entity! { &unrelated @ metadata::created_at: second };
        storage
            .with_pile(|pile, signer, _runtime| {
                compass::commit_collection(pile, signer, fragment)?;
                Ok(())
            })
            .unwrap();
        // A raw commit is the worker's to carry; no faculty write ensured it.
        carry_storage(storage);
        storage
            .with_view(|facts, _, status_register| {
                assert_eq!(
                    latest_status_event(facts, status_register, goal).map(|(_, value, _)| value),
                    Some(crate::schemas::compass::text_handle("todo"))
                );
                Ok(())
            })
            .unwrap();

        let before = std::fs::metadata(&pile).unwrap().len();
        list(storage, vec![], vec![], true).unwrap();
        assert_eq!(std::fs::metadata(&pile).unwrap().len(), before);

        move_goal(storage, format!("{goal:x}"), "doing".to_owned(), None).unwrap();
        storage
            .with_view(|facts, _, status_register| {
                assert_eq!(
                    latest_status_event(facts, status_register, goal).map(|(_, value, _)| value),
                    Some(crate::schemas::compass::text_handle("doing"))
                );
                Ok(())
            })
            .unwrap();
    }

    /// A real pile whose Compass collection holds `fragment` as one raw
    /// commit, carried by the worker, so reads see exactly those facts.
    fn raw_pile(fragment: Fragment) -> (tempfile::TempDir, Storage) {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("open-compass.pile");
        let key = directory.path().join("open-compass.key");
        std::fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        let storage = Storage::new(pile, Some(key));
        let handle = CompassStorage { storage: &storage };
        handle
            .with_pile(|pile, signer, _runtime| {
                compass::commit_collection(pile, signer, fragment)?;
                Ok(())
            })
            .unwrap();
        carry_storage(handle);
        (directory, storage)
    }

    fn point(seconds: f64) -> IntervalValue {
        clock::point(Epoch::from_unix_seconds(seconds)).unwrap()
    }

    fn text(fragment: &mut Fragment, value: &str) -> TextHandle {
        fragment.put::<blobencodings::UTF8String, _>(value.to_owned())
    }

    fn plain_goal(fragment: &mut Fragment, title: &str, seconds: f64) -> Id {
        let (goal, id) = compass::goal_fragment(title, vec![], None, point(seconds)).unwrap();
        *fragment += goal;
        id
    }

    /// The schema cannot express cardinality, so a goal with two titles and
    /// two parents is read, not refused: the board shows every title, sorted
    /// and joined by " / ", and nests the goal under its first shown parent
    /// by id; `show` prints one line per title and per parent. Both readings
    /// are the same on every read.
    #[test]
    fn a_goal_with_two_titles_and_two_parents_lists_and_shows_both() {
        let mut fragment = Fragment::empty();
        let first_parent = plain_goal(&mut fragment, "first parent", 1.0);
        let second_parent = plain_goal(&mut fragment, "second parent", 2.0);
        let goal = genid().id;
        let (beta, alpha) = (text(&mut fragment, "beta"), text(&mut fragment, "alpha"));
        fragment += entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: beta,
            board::parent: &first_parent,
            metadata::created_at: point(3.0),
        };
        fragment += entity! { ExclusiveId::force_ref(&goal) @
            board::title: alpha,
            board::parent: &second_parent,
            metadata::created_at: point(4.0),
        };
        let (_directory, storage) = raw_pile(fragment);
        let handle = CompassStorage { storage: &storage };

        let board = list(handle, vec![], vec![], false).unwrap();
        assert_eq!(board, list(handle, vec![], vec![], false).unwrap());
        assert!(
            board.contains(&format!("  - [{goal:x}] alpha / beta\n")),
            "{board}"
        );
        assert_eq!(board.matches(&format!("[{goal:x}]")).count(), 1, "{board}");

        let shown = show(handle, format!("{goal:x}")).unwrap();
        assert_eq!(shown, show(handle, format!("{goal:x}")).unwrap());
        assert!(shown.contains("Title: alpha\nTitle: beta\n"), "{shown}");
        let (low, high) = if first_parent < second_parent {
            (first_parent, second_parent)
        } else {
            (second_parent, first_parent)
        };
        let parent_line = |id: Id| {
            let title = if id == first_parent {
                "first parent"
            } else {
                "second parent"
            };
            format!("Parent: {title} ({id:x})\n")
        };
        assert!(
            shown.contains(&format!("{}{}", parent_line(low), parent_line(high))),
            "{shown}"
        );
        // The earliest creation time is the one shown.
        let (earliest, _): (Epoch, Epoch) = point(3.0).try_from_inline().unwrap();
        assert!(shown.contains(&format!("Created: {earliest}\n")), "{shown}");
    }

    /// A note with two bodies is one note: the board counts it once, and
    /// `show` prints each body as its own reading under the one id.
    #[test]
    fn a_note_with_two_bodies_is_counted_once_and_shows_both() {
        let mut fragment = Fragment::empty();
        let goal = plain_goal(&mut fragment, "noted goal", 1.0);
        let note = genid().id;
        let (second, first) = (
            text(&mut fragment, "second body"),
            text(&mut fragment, "first body"),
        );
        fragment += entity! { ExclusiveId::force_ref(&note) @
            metadata::tag: &KIND_NOTE_ID,
            board::task: &goal,
            board::note: second,
            metadata::created_at: point(2.0),
        };
        fragment += entity! { ExclusiveId::force_ref(&note) @ board::note: first };
        let (_directory, storage) = raw_pile(fragment);
        let handle = CompassStorage { storage: &storage };

        let board = list(handle, vec![], vec![], false).unwrap();
        assert!(board.contains("noted goal (1 note)\n"), "{board}");
        let shown = show(handle, format!("{goal:x}")).unwrap();
        let first_at = shown.find("  first body\n").expect("first body shown");
        let second_at = shown.find("  second body\n").expect("second body shown");
        assert!(first_at < second_at, "{shown}");
        assert_eq!(
            shown.matches(&format!("- [{note:x}]")).count(),
            2,
            "{shown}"
        );
    }

    /// A goal whose title has not arrived is still a goal: `show` prints
    /// what is known instead of calling it missing.
    #[test]
    fn a_goal_without_a_title_still_shows() {
        let goal = genid().id;
        let fragment = entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            metadata::created_at: point(1.0),
        };
        let (_directory, storage) = raw_pile(fragment);
        let handle = CompassStorage { storage: &storage };
        let shown = show(handle, format!("{goal:x}")).unwrap();
        assert!(
            shown.starts_with(&format!("Goal {goal:x}\nCreated: ")),
            "{shown}"
        );
        assert!(list(handle, vec![], vec![], false)
            .unwrap()
            .contains(&format!("[{goal:x}]")));
    }

    /// Queries are types: a time that does not decode (an inverted interval)
    /// is not one this reader can order by, so the board and `show` skip it
    /// instead of panicking.
    #[test]
    fn times_that_do_not_decode_are_skipped_not_panicked() {
        let mut raw = point(2_000.0).raw;
        raw[16..].copy_from_slice(&point(1_000.0).raw[16..]);
        let inverted = IntervalValue::new(raw);
        let decoded: Result<(i128, i128), _> = inverted.try_from_inline();
        assert!(decoded.is_err());

        let mut fragment = Fragment::empty();
        let other = plain_goal(&mut fragment, "ordinary goal", 1.0);
        let goal = genid().id;
        let title = text(&mut fragment, "inverted goal");
        fragment += entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::title: title,
            metadata::created_at: inverted,
        };
        let body = text(&mut fragment, "inverted note");
        fragment += entity! {
            metadata::tag: &KIND_NOTE_ID,
            board::task: &goal,
            board::note: body,
            metadata::created_at: inverted,
        };
        let doing = text(&mut fragment, "doing");
        fragment += entity! {
            metadata::tag: &KIND_STATUS_ID,
            board::status_of: &other,
            board::status: doing,
            metadata::created_at: inverted,
        };
        fragment += entity! {
            metadata::tag: &crate::schemas::compass::KIND_PRIORITIZE_ID,
            board::higher: &goal,
            board::lower: &other,
            metadata::created_at: inverted,
        };
        let (_directory, storage) = raw_pile(fragment);
        let handle = CompassStorage { storage: &storage };

        let board = list(handle, vec![], vec![], false).unwrap();
        assert!(board.contains("inverted goal (1 note)"), "{board}");
        assert!(board.contains("ordinary goal"), "{board}");
        let shown = show(handle, format!("{goal:x}")).unwrap();
        assert!(!shown.contains("Created:"), "{shown}");
        assert!(!shown.contains("inverted note"), "{shown}");
        show(handle, format!("{other:x}")).unwrap();
    }

    /// A note or status event may name a goal this view does not hold yet;
    /// it waits, and the board reads the goals it does hold.
    #[test]
    fn events_naming_an_absent_goal_do_not_break_the_board() {
        let mut fragment = Fragment::empty();
        let goal = plain_goal(&mut fragment, "present goal", 1.0);
        let absent = genid().id;
        fragment += compass::note_fragment(
            absent,
            "early note",
            vec![],
            vec![],
            vec![],
            None,
            point(2.0),
        )
        .unwrap()
        .0;
        fragment += compass::status_fragment(absent, "doing", None, point(2.0)).unwrap();
        fragment += compass::priority_fragment(absent, goal, true, point(2.0));
        let (_directory, storage) = raw_pile(fragment);
        let handle = CompassStorage { storage: &storage };

        assert_eq!(
            list(handle, vec![], vec![], false).unwrap(),
            format!("\n== TODO (1) ==\n- [{goal:x}] present goal\n\n")
        );
        show(handle, format!("{goal:x}")).unwrap();
    }

    /// Superseding names a note, and a note is a note whether or not the goal
    /// it hangs off has arrived here yet.
    #[test]
    fn a_note_whose_goal_is_absent_can_still_be_superseded() {
        let mut fragment = Fragment::empty();
        let goal = plain_goal(&mut fragment, "present goal", 1.0);
        let (early, early_id) = compass::note_fragment(
            genid().id,
            "note on an absent goal",
            vec![],
            vec![],
            vec![],
            None,
            point(2.0),
        )
        .unwrap();
        fragment += early;
        let (_directory, storage) = raw_pile(fragment);
        let handle = CompassStorage { storage: &storage };

        let added = add_note(
            handle,
            format!("{goal:x}"),
            "supersedes the early note".to_owned(),
            vec![],
            vec![],
            vec![format!("{early_id:x}")],
            None,
        )
        .unwrap();
        let shown = show(handle, format!("{goal:x}")).unwrap();
        assert!(shown.contains(&format!("[{:x}]", added.note)), "{shown}");
        assert!(
            shown.contains(&format!("supersedes: {early_id:x}")),
            "{shown}"
        );
    }
}
