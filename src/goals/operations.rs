//! Callable Goals operations over maintained immutable observations.
//!
//! Inputs are literal native values. CLI paths/pipes and MCP argument decoding
//! belong to their explicit frontends, never to these operations.

#[cfg(test)]
use crate::collection_names::{open_configured, open_exact_in};
use crate::schemas::compass::{
    board, interval_key, latest_status_event, IntervalValue, DEFAULT_SCOPE_ID as GOALS_SCOPE_ID,
    DEFAULT_STATUSES, KIND_GOAL_ID, KIND_NOTE_ID, KIND_STATUS_ID,
};
use crate::schemas::relations::DEFAULT_SCOPE_ID as RELATIONS_SCOPE_ID;
#[cfg(test)]
use crate::storage::FactRead;
use crate::storage::{self, AcquiringReader, FactArchive, FacultySnapshot, FacultyStore, Storage};
use crate::{clock, goals, relations};
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
use triblespace::core::query::rangeconstraint::value_range;
#[cfg(test)]
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::prelude::*;
use triblespace_paths::{PathExpr, PathIndex, Step};

type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;

/// Pile-backed Goals functionality, independent of either frontend.
/// Each operation observes a fresh maintained snapshot through its storage handle.
#[derive(Clone, Debug)]
pub struct Goals {
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
    /// What the reads behind the write could not load
    /// ([`storage::unread_foundations_note`]); the write is stored.
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MovedGoal {
    pub goal: Id,
    pub event: Id,
    pub status: String,
    /// What the read that resolved the goal could not load.
    pub notes: Vec<String>,
}

/// The tags one tag operation asserted on a goal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaggedGoal {
    pub goal: Id,
    /// The canonical tags this operation newly asserted, sorted.
    pub added: Vec<String>,
    /// Requested tags the goal already held; nothing was written for them.
    pub held: Vec<String>,
    /// What the read that found the goal could not load: a tag held only
    /// there is asserted again, which changes nothing.
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddedNote {
    pub goal: Id,
    pub note: Id,
    /// `@label` tokens in the note that named no Relations person or group.
    /// The write succeeded; nobody was addressed by these.
    pub unresolved_mentions: Vec<String>,
    /// What the reads behind the write could not load.
    pub notes: Vec<String>,
}

/// One goal as a structured reading, for a caller that renders its own text
/// from it (`zooid goal`). Lists hold what the facts hold: the schema does not
/// constrain cardinality, so a goal may have several titles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalReading {
    pub goal: Id,
    /// Every distinct title, sorted.
    pub titles: Vec<String>,
    /// The current status event and its word.
    pub status: Option<(Id, String)>,
    /// The parent chain upward, nearest first: each step follows the first
    /// parent in id order, and stops at a cycle or a parent not in view.
    pub ancestors: Vec<(Id, String)>,
    /// The goal's notes that no other note supersedes, oldest first.
    pub notes: Vec<GoalNote>,
}

/// One goal's current status word and tags, for a caller applying its own
/// predicate (the rules sensors). No title or note text is acquired.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalState {
    pub goal: Id,
    /// The canonical current status word, when the goal has one.
    pub status: Option<String>,
    /// Every tag, sorted.
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalNote {
    pub id: Id,
    /// The note's time as Goals prints it.
    pub at: String,
    pub by: Vec<Id>,
    pub text: String,
}

/// A rendered board and the advisory notes about it.
///
/// `notes` is empty for every board a well-behaved, sequential writer
/// produces over a complete read. It speaks of priority data such writers do
/// not make (a cycle, a snapshot that ranks over itself), and of foundations
/// the read could not load; the board is read regardless.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Board {
    pub text: String,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriorityChange {
    pub higher: Id,
    pub lower: Id,
    /// The new priority snapshot of `higher`.
    pub event: Id,
    pub active: bool,
    pub higher_title: String,
    pub lower_title: String,
}

impl Goals {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    fn storage(&self) -> GoalsStorage<'_> {
        GoalsStorage {
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
        self.board(options).map(|board| board.text)
    }

    /// [`Self::list`], with the advisory notes a frontend delivers beside it.
    pub fn board(&self, options: ListOptions<'_>) -> Result<Board> {
        board(
            self.storage(),
            options.statuses.to_vec(),
            options.tags.to_vec(),
            options.all,
        )
    }

    /// Whether an actor has a status event or note at/after `since` on a goal
    /// with an explicit maintained current status in `statuses`. A missing
    /// status does not match, including `todo`. Intervals use their lower bound;
    /// future activity also qualifies. No title, note body or status text is fetched.
    /// Repeated authors are membership, not a single selected author.
    pub fn has_activity(&self, actor: Id, since: Epoch, statuses: &[String]) -> Result<bool> {
        if statuses.is_empty() {
            bail!("at least one current status is required");
        }
        let statuses: HashSet<TextHandle> = statuses
            .iter()
            .map(|status| {
                let status = normalize_status(status.clone());
                validate_label("status", &status)?;
                Ok(crate::schemas::compass::text_handle(&status))
            })
            .collect::<Result<_>>()?;
        let min = clock::point(since)?;
        // Inclusive lower-bound range. The all-max sentinel includes every
        // upper bound; typed interval decoding below skips inverted intervals.
        let max = IntervalValue::new([255; 32]);
        self.storage().with_view(|space, _reader, status_register| {
            Ok(find!(
                (goal: Id, at: (Epoch, Epoch)),
                temp!((event, payload, current, current_status), and!(
                    pattern!(space, [
                        { ?goal @ metadata::tag: &KIND_GOAL_ID },
                        { ?event @ board::by: &actor, metadata::created_at: ?at },
                        { ?current @
                            metadata::tag: &KIND_STATUS_ID,
                            board::status_of: ?goal,
                            board::status: ?current_status,
                            metadata::created_at: _?current_at,
                        },
                    ]),
                    or!(
                        pattern!(space, [{ ?event @
                            metadata::tag: &KIND_STATUS_ID,
                            board::status_of: ?goal,
                            board::status: ?payload,
                        }]),
                        pattern!(space, [{ ?event @
                            metadata::tag: &KIND_NOTE_ID,
                            board::task: ?goal,
                            board::note: ?payload,
                        }]),
                    ),
                    status_register.has(current),
                    statuses.has(current_status),
                    value_range(at, min, max),
                ))
            )
            .any(|(goal, _)| {
                // The joined winner can carry several words/times. Preserve
                // the board's canonical reading rather than accepting any word.
                latest_status_event(space, status_register, goal)
                    .is_some_and(|(_, status, _)| statuses.contains(&status))
            }))
        })
    }

    /// Show priority layers and direct relations across status columns.
    /// Filters select displayed goals, never remove ordering evidence.
    pub fn priorities(&self, options: ListOptions<'_>) -> Result<Board> {
        let statuses: Vec<String> = options
            .statuses
            .iter()
            .cloned()
            .map(normalize_status)
            .collect();
        for status in &statuses {
            validate_label("status", status)?;
        }
        let storage = self.storage();
        let (mut board, unread) = storage.with_view_read(|space, reader, status_register| {
            render_priorities(
                reader,
                space,
                status_register,
                &statuses,
                options.tags,
                options.all,
            )
        })?;
        board.notes.extend(unread);
        Ok(board)
    }

    pub fn move_goal(&self, id: &str, status: &str, persona: Option<&str>) -> Result<MovedGoal> {
        move_goal(self.storage(), id.to_owned(), status.to_owned(), persona)
    }

    /// Assert more tags on an existing goal. A tag is the same `board::tag`
    /// fact `add` writes, published later beside the goal's core: an
    /// open-world addition, never an edit, and there is no way to take one
    /// back. The fact sits on the goal itself, which has no author slot, so
    /// the only authorship is the publishing commit's signer. Tags the goal
    /// already holds are not written again; all held is a genuine no-op.
    pub fn tag(&self, id: &str, tags: &[String]) -> Result<TaggedGoal> {
        tag_goal(self.storage(), id.to_owned(), tags.to_vec())
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

    /// Say that the goal is unclear to `persona`: an ordinary attributed
    /// note tagged [`goals::UNCLEAR_TAG`] asking for an explanation. It
    /// writes no priority and no status; [`goals::unclear_goals`] reads it
    /// until a note on the goal supersedes it.
    pub fn mark_unclear(&self, id: &str, persona: Option<&str>) -> Result<AddedNote> {
        let id = id.to_owned();
        let (mut added, notes) = self.storage().update_with(
            GoalsRead::Resident,
            persona,
            &[],
            |space, _reader, by_id, _mentions| Ok((resolve_task_id(&id, space)?, by_id)),
            |(goal, by_id)| {
                author_unclear_note(goal, by_id).map(|(fragment, note)| (Some(fragment), note))
            },
        )?;
        added.notes = notes;
        Ok(added)
    }

    /// Every goal's current status and tags, read from one frozen view.
    pub fn goal_states(&self) -> Result<Vec<GoalState>> {
        self.storage().with_view(|space, reader, status_register| {
            goal_states_in(reader, space, status_register)
        })
    }

    /// One goal as text, with a note for anything the read could not load.
    pub fn show(&self, id: &str) -> Result<Board> {
        show(self.storage(), id.to_owned())
    }

    /// One goal's title, status, parent chain and current notes.
    pub fn reading(&self, id: &str) -> Result<GoalReading> {
        let id = id.to_owned();
        self.storage().with_view(|space, reader, status_register| {
            let goal = resolve_task_id(&id, space)?;
            goal_reading(reader, space, status_register, goal)
        })
    }

    pub fn prioritize(
        &self,
        higher: &str,
        lower: &str,
        persona: Option<&str>,
    ) -> Result<PriorityChange> {
        prioritize(self.storage(), higher.to_owned(), lower.to_owned(), persona)
    }

    pub fn deprioritize(
        &self,
        higher: &str,
        lower: &str,
        persona: Option<&str>,
    ) -> Result<PriorityChange> {
        deprioritize(self.storage(), higher.to_owned(), lower.to_owned(), persona)
    }

    pub fn resolve(&self, prefix: &str) -> Result<Id> {
        resolve(self.storage(), prefix.to_owned())
    }
}

/// [`Goals::goal_states`] over facts and a status register a caller already
/// holds, such as the viewer's routed Goals dataset.
pub fn goal_states_in<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    status_register: &LwwQuery,
) -> Result<Vec<GoalState>> {
    all_goal_ids(space)
        .into_iter()
        .map(|goal| {
            let status =
                task_latest_status(reader, space, status_register, goal)?.map(|(status, _)| status);
            let tags = task_tags(reader, space, goal, &[])?;
            Ok(GoalState { goal, status, tags })
        })
        .collect()
}

/// [`Goals::reading`] of one goal id over facts a caller already holds.
pub fn goal_reading_in<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    status_register: &LwwQuery,
    goal: Id,
) -> Result<GoalReading> {
    goal_reading(reader, space, status_register, goal)
}

// ── on-demand board queries ───────────────────────────────────────────
// All data stays in the maintained Succinct view; query it directly instead
// of pre-materializing Rust catalogs.

/// Query helpers that operate directly on one immutable fact view + workspace.

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
        goals::mention_labels(text)
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
struct GoalsStorage<'a> {
    storage: &'a Storage,
}

impl GoalsStorage<'_> {
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
    /// reader, refused when a foundation of either cannot be loaded. Only
    /// for an answer absence would invalidate: whether someone has been
    /// active, the states a rule decides on. Printing and other effects
    /// belong after this returns.
    fn with_view<T>(
        &self,
        f: impl FnMut(&FactArchive, &AcquiringReader<FacultySnapshot>, &LwwQuery) -> Result<T>,
    ) -> Result<T> {
        self.with_view_mode(true, f).map(|(value, _)| value)
    }

    /// [`Self::with_view`] over what this observation could load, with a
    /// note for each of the facts and the status register that left
    /// something out (JP, 2026-11-26): every board and lookup.
    fn with_view_read<T>(
        &self,
        f: impl FnMut(&FactArchive, &AcquiringReader<FacultySnapshot>, &LwwQuery) -> Result<T>,
    ) -> Result<(T, Vec<String>)> {
        self.with_view_mode(false, f)
    }

    fn with_view_mode<T>(
        &self,
        complete: bool,
        mut f: impl FnMut(&FactArchive, &AcquiringReader<FacultySnapshot>, &LwwQuery) -> Result<T>,
    ) -> Result<(T, Vec<String>)> {
        self.with_pile(|pile, signer, runtime| {
            let source = self.storage.open_collection_read(
                pile,
                GOALS_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
            let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
            let status_target = goals::status_register_for_source(pile, source)?;
            let reader = AcquiringReader::new(
                pile.snapshot().context("freeze Goals facts and status")?,
                runtime.clone(),
            );
            let (facts, unread_facts) = storage::acquire_facts_read(&reader, rank9)?.into_parts();
            let (status, unread_status) = reader
                .attached_acquiring(status_target)?
                .read_acquiring::<LwwIndex>()?
                .into_parts();
            for unread in [&unread_facts, &unread_status] {
                if complete && !unread.is_empty() {
                    return Err(storage::IncompleteAttachedRead {
                        unread: unread.clone(),
                    }
                    .into());
                }
            }
            // The status register stands on the same foundations as the
            // facts: name it only for one the facts did not miss.
            let mut notes: Vec<String> = storage::unread_foundations_note("Goals", &unread_facts)
                .into_iter()
                .collect();
            if unread_status
                .members()
                .any(|member| !unread_facts.contains(member))
            {
                notes.extend(storage::unread_foundations_note(
                    "Goals status",
                    &unread_status,
                ));
            }
            let status = status.query()?;
            let value = f(&facts, &reader, &status).map_err(|error| with_notes(error, &notes))?;
            Ok((value, notes))
        })
    }

    /// Prepare reads against one frozen, complete view, then author and
    /// publish once: refused when a foundation cannot be loaded. Only for a
    /// write that supersedes what it saw (a priority snapshot replaces the
    /// higher goal's current one). The author runs outside retries, so event
    /// clocks and publication cannot be repeated by a missing attachment.
    /// `None` is a genuine no-op.
    fn update<P, T>(
        &self,
        persona: Option<&str>,
        mentions: &[String],
        prepare: impl FnMut(
            &FactArchive,
            &AcquiringReader<FacultySnapshot>,
            Option<Id>,
            &Mentions,
        ) -> Result<P>,
        author: impl FnOnce(P) -> Result<(Option<Fragment>, T)>,
    ) -> Result<T> {
        self.update_with(GoalsRead::Complete, persona, mentions, prepare, author)
            .map(|(value, _)| value)
    }

    /// [`Self::update`] prepared as `read` asks: [`GoalsRead::Resident`] for
    /// an append (a goal, a status, a tag, a note), over what the observation
    /// could load, with a note for each collection that left something out;
    /// [`GoalsRead::Skip`] reads no Goals facts at all, for a write that names
    /// no goal.
    fn update_with<P, T>(
        &self,
        read: GoalsRead,
        persona: Option<&str>,
        mentions: &[String],
        mut prepare: impl FnMut(
            &FactArchive,
            &AcquiringReader<FacultySnapshot>,
            Option<Id>,
            &Mentions,
        ) -> Result<P>,
        author: impl FnOnce(P) -> Result<(Option<Fragment>, T)>,
    ) -> Result<(T, Vec<String>)> {
        self.with_pile(|pile, signer, runtime| {
            // Register every representation and attach the resident views
            // through one query snapshot for this action; the write authors,
            // commits, then attaches the source's frontier so the write is
            // readable through the attached views. Reads never maintain.
            let goals_source = self.storage.open_collection_read(
                pile,
                GOALS_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            let goals_succinct = pile
                .attach::<SuccinctArchiveBlob>(goals_source, ())
                .context("register Goals Succinct collection")?;
            let goals_rank9 = pile
                .attach::<Rank9AcceleratedSuccinctArchiveBlob>(goals_source, goals_succinct)
                .context("register Goals Rank9 collection")?;
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
                pile.snapshot().context("freeze Goals/Relations snapshot")?,
                runtime.clone(),
            );
            // No read refuses for being behind. There is no globally
            // consistent state to be behind of: another node holds commits
            // this one has never seen, so "stands for every admitted commit"
            // is a closed-world claim, and refusing on it would only narrow
            // the branching of one entity's history a little. A priority
            // change reads what it can see, like everything else, and
            // ensures its own images after it commits.
            let complete = matches!(read, GoalsRead::Complete);
            let mut notes = Vec::new();
            let mut read_facts = |rank9, label: &str| -> Result<FactArchive> {
                let (facts, unread) = storage::acquire_facts_read(&reader, rank9)?.into_parts();
                if complete && !unread.is_empty() {
                    return Err(storage::IncompleteAttachedRead { unread }.into());
                }
                notes.extend(storage::unread_foundations_note(label, &unread));
                Ok(facts)
            };
            let facts = match read {
                GoalsRead::Skip => FactArchive::new(vec![
                    triblespace::core::blob::encodings::succinctarchive::SuccinctArchive::from(
                        &TribleSet::new(),
                    ),
                ]),
                _ => read_facts(goals_rank9, "Goals").context("read Goals fact collection")?,
            };
            let relations = relations_rank9
                .map(|rank9| {
                    read_facts(rank9, "Relations")
                        .context("read Relations fact collection for Goals persona and mentions")
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
            let prepared = prepare(&facts, &reader, by, &resolved)
                .map_err(|error| with_notes(error, &notes))?;
            let (fragment, value) = author(prepared)?;
            if let Some(fragment) = fragment {
                crate::collection_names::require_command_write_admission_acquiring(
                    pile,
                    goals_source,
                    signer,
                    "Goals",
                    "goals list",
                    runtime,
                )?;
                runtime.block_on(publish_goals(pile, goals_source, signer, fragment))?;
            }
            Ok((value, notes))
        })
    }
}

/// Which Goals facts an update prepares over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GoalsRead {
    /// Every foundation, or refuse.
    Complete,
    /// What this observation could load.
    Resident,
    /// None: the write names no goal.
    Skip,
}

/// `error`, with the notes of what its read could not load: a goal or note
/// not found may be in one of them.
fn with_notes(error: anyhow::Error, notes: &[String]) -> anyhow::Error {
    notes
        .iter()
        .fold(error, |error, note| error.context(note.clone()))
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
    goals::read_text(reader, handle)
}

/// Parse a full 32-char hex ID. Returns a helpful error pointing to `goals resolve` on failure.
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
    if !goals::note_ids(space).contains(&note_id) {
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

fn render_priorities<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    status_register: &LwwQuery,
    status_filter: &[String],
    tag_filter: &[String],
    show_done: bool,
) -> Result<Board> {
    let goals = all_goal_ids(space);
    let explicit = goals::active_priority_edges(space);
    let edges = goals::goal_priority_edges(space);
    let ranks = goals::priority_ranks(goals.iter().copied(), &edges);
    let notes = goals::priority_notes(space, goals.iter().copied(), &edges);
    let text_handle = crate::schemas::compass::text_handle;
    let mut selected = BTreeSet::new();
    for &goal in &goals {
        let status = latest_status_event(space, status_register, goal)
            .map(|(_, status, _)| status)
            .unwrap_or_else(|| text_handle("todo"));
        if status_filter.is_empty() {
            if !show_done && status == text_handle("done") {
                continue;
            }
        } else if !status_filter.iter().any(|s| text_handle(s) == status) {
            continue;
        }
        if !tag_filter.is_empty()
            && !tag_filter
                .iter()
                .any(|tag| exists!(pattern!(space, [{ goal @ board::tag: tag.as_str() }])))
        {
            continue;
        }
        selected.insert(goal);
    }

    let mut text = String::from(
        "Priority layers use the full board, including filtered goals; ID order is display only.\n\
         Within acyclic layers, goals are incomparable; different layers can also be incomparable.\n",
    );
    // Only ids and graph ranks are sorted; goal payloads are read at rendering.
    let mut ordered: Vec<_> = selected.iter().copied().collect();
    ordered.sort_by_key(|goal| (ranks[goal], *goal));
    let mut layer = None;
    for goal in ordered {
        let rank = ranks[&goal];
        if layer != Some(rank) {
            writeln!(text, "\n== LAYER {} ==", rank + 1)?;
            layer = Some(rank);
        }
        let status = latest_status_event(space, status_register, goal)
            .map(|(_, status, _)| status)
            .unwrap_or_else(|| text_handle("todo"));
        writeln!(
            text,
            "- [{goal:x}] {} ({})",
            task_title(reader, space, goal)?,
            word(reader, status, &DEFAULT_STATUSES)?
        )?;
    }
    if selected.is_empty() {
        writeln!(text, "\nNo matching goals.")?;
    }
    writeln!(
        text,
        "\nDirect relations (higher > lower; * = outside selection):"
    )?;
    let mut any = false;
    for &(higher, lower) in &edges {
        if !selected.contains(&higher) && !selected.contains(&lower) {
            continue;
        }
        let structural = exists!(pattern!(space, [{
            higher @ metadata::tag: &KIND_GOAL_ID, board::parent: &lower
        }, { lower @ metadata::tag: &KIND_GOAL_ID }]));
        let kind = match (explicit.contains(&(higher, lower)), structural) {
            (true, true) => "explicit + child-before-parent",
            (true, false) => "explicit",
            _ => "child-before-parent",
        };
        writeln!(
            text,
            "- [{higher:x}]{} > [{lower:x}]{} ({kind})",
            if selected.contains(&higher) {
                format!(" {}", task_title(reader, space, higher)?)
            } else {
                "*".to_owned()
            },
            if selected.contains(&lower) {
                format!(" {}", task_title(reader, space, lower)?)
            } else {
                "*".to_owned()
            }
        )?;
        any = true;
    }
    if !any {
        writeln!(text, "(none touching selected goals)")?;
    }
    Ok(Board { text, notes })
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
    let priority_ranks =
        goals::priority_ranks(goal_ids.iter().copied(), &goals::goal_priority_edges(space));

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
    storage: GoalsStorage<'_>,
    title: String,
    status: String,
    parent: Option<String>,
    tags: Vec<String>,
    note: Option<String>,
    persona: Option<&str>,
) -> Result<AddedGoal> {
    let status = goals::canonical_status(status)?;
    let tags = goals::canonical_tags(tags)?;
    let mut labels = goals::mention_labels(&title);
    labels.extend(
        note.as_deref()
            .map(goals::mention_labels)
            .unwrap_or_default(),
    );
    // A goal without a parent names no other goal, so nothing of Goals is
    // read: adding one never waits on a foundation that is away.
    let read = if parent.is_some() {
        GoalsRead::Resident
    } else {
        GoalsRead::Skip
    };
    let (mut added, notes) = storage.update_with(
        read,
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
            let mut change = goals::kind_catalog_fragment();
            let title_mentions = mentions.targets(&title);
            let (goal, task_ref) = goals::goal_fragment(title, tags, parent_id, now)?;
            change += goal;
            change += goals::mention_fragment(task_ref, &title_mentions);
            let status = goals::status_fragment(task_ref, status, by_id, now)?;
            let status_event = status.root().expect("status event exports one root");
            change += status;

            let mut note_ref = None;
            if let Some(note) = note {
                let references = extract_reference_values(&note);
                let note_mentions = mentions.targets(&note);
                let (record, note_id) =
                    goals::note_fragment(task_ref, note, vec![], references, vec![], by_id, now)?;
                change += record;
                change += goals::mention_fragment(note_id, &note_mentions);
                note_ref = Some(note_id);
            }
            Ok((
                Some(change),
                AddedGoal {
                    goal: task_ref,
                    status_event,
                    note: note_ref,
                    unresolved_mentions: mentions.unresolved(),
                    notes: Vec::new(),
                },
            ))
        },
    )?;
    added.notes = notes;
    Ok(added)
}

#[cfg(test)]
fn list(
    storage: GoalsStorage<'_>,
    status_filter: Vec<String>,
    tag_filter: Vec<String>,
    show_done: bool,
) -> Result<String> {
    board(storage, status_filter, tag_filter, show_done).map(|board| board.text)
}

fn board(
    storage: GoalsStorage<'_>,
    status_filter: Vec<String>,
    tag_filter: Vec<String>,
    show_done: bool,
) -> Result<Board> {
    let status_filter: Vec<String> = status_filter.into_iter().map(normalize_status).collect();
    for status in &status_filter {
        validate_label("status", status)?;
    }

    let (mut board, unread) = storage.with_view_read(|space, reader, status_register| {
        let text = render_board(
            reader,
            space,
            status_register,
            &status_filter,
            &tag_filter,
            show_done,
        )?;
        let notes = goals::priority_notes(
            space,
            all_goal_ids(space),
            &goals::goal_priority_edges(space),
        );
        Ok(Board { text, notes })
    })?;
    board.notes.extend(unread);
    Ok(board)
}

fn move_goal(
    storage: GoalsStorage<'_>,
    id: String,
    status: String,
    persona: Option<&str>,
) -> Result<MovedGoal> {
    let status = goals::canonical_status(status)?;
    let rendered_status = status.clone();
    let (mut moved, notes) = storage.update_with(
        GoalsRead::Resident,
        persona,
        &[],
        |space, _reader, by_id, _| Ok((resolve_task_id(&id, space)?, by_id)),
        |(task_id, by_id)| {
            let mut change = goals::kind_catalog_fragment();
            let record = goals::status_fragment(task_id, status, by_id, clock::point_now()?)?;
            let event = record.root().expect("status event exports one root");
            change += record;
            Ok((
                Some(change),
                MovedGoal {
                    goal: task_id,
                    event,
                    status: rendered_status,
                    notes: Vec::new(),
                },
            ))
        },
    )?;
    moved.notes = notes;
    Ok(moved)
}

fn tag_goal(storage: GoalsStorage<'_>, id: String, tags: Vec<String>) -> Result<TaggedGoal> {
    let tags = goals::canonical_tags(tags)?;
    if tags.is_empty() {
        bail!("at least one tag is required");
    }
    let (mut tagged, notes) = storage.update_with(
        GoalsRead::Resident,
        None,
        &[],
        |space, _reader, _by, _| {
            let goal = resolve_task_id(&id, space)?;
            // A full id resolves without a lookup; a tag must name a goal
            // this view holds, never mint a stray entity.
            if !exists!(pattern!(space, [{ goal @ metadata::tag: &KIND_GOAL_ID }])) {
                bail!("no goal {goal:x} in this view; tags are added to existing goals only");
            }
            let (held, added): (Vec<String>, Vec<String>) = tags
                .iter()
                .cloned()
                .partition(|tag| exists!(pattern!(space, [{ goal @ board::tag: tag.as_str() }])));
            Ok((goal, added, held))
        },
        |(goal, added, held)| {
            let change = (!added.is_empty()).then(|| {
                let mut change = goals::kind_catalog_fragment();
                change += goals::tag_fragment(goal, &added);
                change
            });
            Ok((
                change,
                TaggedGoal {
                    goal,
                    added,
                    held,
                    notes: Vec::new(),
                },
            ))
        },
    )?;
    tagged.notes = notes;
    Ok(tagged)
}

fn add_note(
    storage: GoalsStorage<'_>,
    id: String,
    note: String,
    tags: Vec<String>,
    mut references: Vec<String>,
    supersedes: Vec<String>,
    persona: Option<&str>,
) -> Result<AddedNote> {
    let tags = goals::canonical_tags(tags)?;
    if let Some(reference) = references
        .iter()
        .find(|reference| reference.trim().is_empty())
    {
        bail!("reference must not be empty: {reference:?}");
    }
    references.extend(extract_reference_values(&note));
    references.sort();
    references.dedup();

    let labels = goals::mention_labels(&note);
    let (mut added, notes) = storage.update_with(
        GoalsRead::Resident,
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
            let mut change = goals::kind_catalog_fragment();
            let note_mentions = mentions.targets(&note);
            let (record, note_id) =
                goals::note_fragment(task_id, note, tags, references, superseded_ids, by_id, now)?;
            change += record;
            change += goals::mention_fragment(note_id, &note_mentions);
            Ok((
                Some(change),
                AddedNote {
                    goal: task_id,
                    note: note_id,
                    unresolved_mentions: mentions.unresolved(),
                    notes: Vec::new(),
                },
            ))
        },
    )?;
    added.notes = notes;
    Ok(added)
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

fn goal_reading<P: TriblePattern>(
    reader: &impl BlobStoreGet,
    space: &P,
    status_register: &LwwQuery,
    goal: Id,
) -> Result<GoalReading> {
    let status = latest_status_event(space, status_register, goal)
        .map(|(event, status, _)| -> Result<_> {
            Ok((event, word(reader, status, &DEFAULT_STATUSES)?))
        })
        .transpose()?;
    let mut ancestors = Vec::new();
    let mut seen = HashSet::from([goal]);
    let mut at = goal;
    while let Some(parent) = task_parents(space, at).into_iter().next() {
        if !seen.insert(parent) {
            break;
        }
        let title = task_title(reader, space, parent)?;
        if title.is_empty() {
            break;
        }
        ancestors.push((parent, title));
        at = parent;
    }
    let superseded: HashSet<Id> = find!(
        (newer: Id, old: Id),
        pattern!(space, [{ ?newer @ metadata::supersedes: ?old }])
    )
    .map(|(_, old)| old)
    .collect();
    let mut notes: Vec<(i128, GoalNote)> = Vec::new();
    for (note, handle, at) in find!(
        (note: Id, note_handle: TextHandle, at: IntervalValue),
        pattern!(space, [{
            ?note @
            metadata::tag: &KIND_NOTE_ID,
            board::task: &goal,
            board::note: ?note_handle,
            metadata::created_at: ?at,
        }])
    ) {
        if superseded.contains(&note) {
            continue;
        }
        let (Some(key), Some(at)) = (interval_key(at), format_interval(at)) else {
            continue;
        };
        notes.push((
            key,
            GoalNote {
                id: note,
                at,
                by: event_actors(space, note),
                text: read_text(reader, handle)?,
            },
        ));
    }
    notes.sort_by(|(a, x), (b, y)| (a, x.id, &x.text).cmp(&(b, y.id, &y.text)));
    Ok(GoalReading {
        goal,
        titles: task_titles(reader, space, goal)?,
        status,
        ancestors,
        notes: notes.into_iter().map(|(_, note)| note).collect(),
    })
}

fn show(storage: GoalsStorage<'_>, id: String) -> Result<Board> {
    let (text, notes) = storage.with_view_read(|space, reader, status_register| {
        let task_id = resolve_task_id(&id, space)?;
        render_goal(reader, space, status_register, task_id)
    })?;
    Ok(Board { text, notes })
}

fn prioritize(
    storage: GoalsStorage<'_>,
    higher_input: String,
    lower_input: String,
    persona: Option<&str>,
) -> Result<PriorityChange> {
    storage.update(
        persona,
        &[],
        |space, reader, by_id, _| {
            prepare_priority(space, reader, &higher_input, &lower_input, by_id)
        },
        author_prepared_priority,
    )
}

// Operation-local plan, consumed exactly once by the author.
type PreparedPriority = (Id, Id, String, String, PriorityPlan, Option<Id>);

fn prepare_priority(
    space: &FactArchive,
    reader: &impl BlobStoreGet,
    higher_input: &str,
    lower_input: &str,
    by_id: Option<Id>,
) -> Result<PreparedPriority> {
    let higher_id = resolve_task_id(&higher_input, space)?;
    let lower_id = resolve_task_id(&lower_input, space)?;

    if higher_id == lower_id {
        bail!("cannot prioritize a goal over itself");
    }

    // The order as it would read after this write, child before
    // parent included. Ranking B over A while A > B is a flip: B's
    // new snapshot reaches A's current one, so A's claim over B's
    // replaced snapshot lapses and no cycle is left. A write is
    // refused only when a cycle through the higher goal would remain:
    // a parent ranked over its own descendant, or a cycle closed
    // through a claim the new snapshot does not reach.
    let plan = priority_plan(space, higher_id, Some(lower_id), None);
    let edges = goals::goal_priority_edges_after(space, higher_id, plan.over.keys().copied());
    let on_cycle = edges.iter().any(|&(from, to)| {
        from == higher_id && goals::would_create_priority_cycle(&edges, higher_id, to)
    });
    if on_cycle {
        let paths = parent_paths(space)?;
        if is_ancestor(&paths, higher_id, lower_id) || is_ancestor(&paths, lower_id, higher_id) {
            bail!("children are implicitly prioritized over their parents");
        }
        bail!("would create a priority cycle");
    }

    Ok((
        higher_id,
        lower_id,
        task_title(reader, space, higher_id)?,
        task_title(reader, space, lower_id)?,
        plan,
        by_id,
    ))
}

fn author_prepared_priority(
    (higher_id, lower_id, higher_title, lower_title, plan, by_id): PreparedPriority,
) -> Result<(Option<Fragment>, PriorityChange)> {
    let mut change = goals::kind_catalog_fragment();
    let (record, event) = author_priority(plan, higher_id, clock::point_now()?, by_id);
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
}

/// The same priority operation over a host's borrowed native pile and exact route.
/// No storage is opened and no persona is inferred. The host revalidates its
/// workspace before entry and cancels stale intent at the final publication gate.
pub async fn prioritize_in(
    pile: &mut triblespace::core::repo::pile::Pile,
    signer: &ed25519_dalek::SigningKey,
    collection: triblespace::core::collection::CollectionHandle,
    higher: Id,
    lower: Id,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<PriorityChange> {
    let snapshot = pile.snapshot()?;
    let source = crate::collection_names::open_workspace_read_in(
        &snapshot,
        signer.verifying_key(),
        collection,
    )?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    let reader = pile.snapshot()?;
    let facts = storage::acquire_facts(&reader, rank9)?;
    let prepared = prepare_priority(&facts, &reader, &fmt_id(higher), &fmt_id(lower), None)?;
    let (fragment, value) = author_prepared_priority(prepared)?;
    if let Some(fragment) = fragment {
        crate::collection_names::open_workspace_write_in(
            &pile.snapshot()?,
            signer.verifying_key(),
            collection,
        )?;
        before_commit()?;
        publish_goals(pile, source, signer, fragment).await?;
    }
    Ok(value)
}

/// The note [`Goals::mark_unclear`] and [`mark_unclear_in`] both write: the
/// fixed request body, tagged [`goals::UNCLEAR_TAG`], attributed to `by`.
fn author_unclear_note(goal: Id, by: Option<Id>) -> Result<(Fragment, AddedNote)> {
    let mut change = goals::kind_catalog_fragment();
    let (record, note) = goals::note_fragment(
        goal,
        goals::UNCLEAR_NOTE,
        vec![goals::UNCLEAR_TAG.to_owned()],
        Vec::new(),
        Vec::new(),
        by,
        clock::point_now()?,
    )?;
    change += record;
    Ok((
        change,
        AddedNote {
            goal,
            note,
            unresolved_mentions: Vec::new(),
            notes: Vec::new(),
        },
    ))
}

/// [`Goals::mark_unclear`] over a host's borrowed native pile and exact
/// route, as [`prioritize_in`] is for a priority. No storage is opened and no
/// persona is inferred: `by` is the Relations person the host already knows to
/// be acting, or `None`, leaving the commit's signer as the only authorship.
/// The host revalidates its workspace before entry and cancels stale intent at
/// the final publication gate.
pub async fn mark_unclear_in(
    pile: &mut triblespace::core::repo::pile::Pile,
    signer: &ed25519_dalek::SigningKey,
    collection: triblespace::core::collection::CollectionHandle,
    goal: Id,
    by: Option<Id>,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<AddedNote> {
    let snapshot = pile.snapshot()?;
    let source = crate::collection_names::open_workspace_read_in(
        &snapshot,
        signer.verifying_key(),
        collection,
    )?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    let reader = pile.snapshot()?;
    let facts = storage::acquire_facts(&reader, rank9)?;
    let goal = resolve_task_id(&fmt_id(goal), &facts)?;
    let (fragment, note) = author_unclear_note(goal, by)?;
    crate::collection_names::open_workspace_write_in(
        &pile.snapshot()?,
        signer.verifying_key(),
        collection,
    )?;
    before_commit()?;
    publish_goals(pile, source, signer, fragment).await?;
    Ok(note)
}

async fn publish_goals<S>(
    pile: &mut S,
    source: triblespace::core::collection::Collection<blobencodings::SimpleArchive>,
    signer: &ed25519_dalek::SigningKey,
    fragment: Fragment,
) -> Result<()>
where
    S: triblespace::core::repo::Store
        + triblespace::core::collection::CollectionStoreExt
        + triblespace::core::repo::async_store::AsyncBlobStoreAcquire
        + Send,
{
    pile.commit(source, signer, fragment)
        .context("commit Goals collection fragment")?;
    async {
        let status = goals::status_register_for_source(pile, source)?;
        storage::seed_attached(pile, status, signer).await?;
        storage::ensure_downstream(pile, source, signer).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await
    .context("Goals fragment was committed, but ensuring its derived views failed")
}

/// What one priority write says about `goal`: the goals its new snapshot
/// ranks over, each with its current snapshots (none yet: the write mints a
/// bottom one).
struct PriorityPlan {
    over: BTreeMap<Id, BTreeSet<Id>>,
}

/// Plan a new priority snapshot of `goal`: every goal it is explicitly above
/// now, with `add` and without `remove`.
///
/// The new snapshot ranks over each of those goals' CURRENT snapshots, so a
/// claim a goal kept over an older snapshot of another is refreshed, and a
/// claim that has lapsed (the other goal re-ranked itself above this one) is
/// not revived by being copied.
fn priority_plan<P: TriblePattern>(
    space: &P,
    goal: Id,
    add: Option<Id>,
    remove: Option<Id>,
) -> PriorityPlan {
    let reading = goals::priority_reading(space);
    let mut lowers: BTreeSet<Id> = reading
        .edges
        .iter()
        .filter(|(higher, lower)| *higher == goal && *lower != goal)
        .map(|(_, lower)| *lower)
        .collect();
    lowers.extend(add);
    if let Some(remove) = remove {
        lowers.remove(&remove);
    }
    PriorityPlan {
        over: lowers
            .into_iter()
            .map(|lower| {
                let current = reading.current.get(&lower).cloned().unwrap_or_default();
                (lower, current)
            })
            .collect(),
    }
}

/// The facts of a planned priority write, and the new snapshot's id.
fn author_priority(
    plan: PriorityPlan,
    goal: Id,
    at: IntervalValue,
    by: Option<Id>,
) -> (Fragment, Id) {
    let mut fragment = Fragment::empty();
    let mut over = BTreeSet::new();
    for (lower, current) in plan.over {
        if current.is_empty() {
            let (bottom, snapshot) = goals::priority_snapshot_fragment(lower, [], at);
            fragment += bottom;
            fragment += entity! { ExclusiveId::force_ref(&snapshot) @ board::by?: by.as_ref() };
            over.insert(snapshot);
        } else {
            over.extend(current);
        }
    }
    let (record, snapshot) = goals::priority_snapshot_fragment(goal, over, at);
    fragment += record;
    // Authorship annotates the snapshot; it never changes the identity core.
    fragment += entity! { ExclusiveId::force_ref(&snapshot) @ board::by?: by.as_ref() };
    (fragment, snapshot)
}

fn deprioritize(
    storage: GoalsStorage<'_>,
    higher_input: String,
    lower_input: String,
    persona: Option<&str>,
) -> Result<PriorityChange> {
    storage.update(
        persona,
        &[],
        |space, reader, by_id, _| {
            let higher_id = resolve_task_id(&higher_input, space)?;
            let lower_id = resolve_task_id(&lower_input, space)?;

            let edges = goals::active_priority_edges(space);
            if !edges.contains(&(higher_id, lower_id)) {
                bail!("no active priority relationship between these goals");
            }

            Ok((
                higher_id,
                lower_id,
                task_title(reader, space, higher_id)?,
                task_title(reader, space, lower_id)?,
                priority_plan(space, higher_id, None, Some(lower_id)),
                by_id,
            ))
        },
        |(higher_id, lower_id, higher_title, lower_title, plan, by_id)| {
            let mut change = goals::kind_catalog_fragment();
            let (record, event) = author_priority(plan, higher_id, clock::point_now()?, by_id);
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

fn resolve(storage: GoalsStorage<'_>, prefix: String) -> Result<Id> {
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

    #[test]
    fn borrowed_priority_uses_the_shared_plan_and_cancels_before_publication() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        use triblespace::core::repo::pile::Pile;
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[22; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let source = pile
            .collection(
                "priority fixture",
                CollectionPolicy::new(
                    AdmissionPolicy::Open,
                    AdmissionPolicy::direct(signer.verifying_key()),
                ),
            )
            .unwrap();
        let mut fragment = Fragment::empty();
        let higher = plain_goal(&mut fragment, "higher", 1.0);
        let lower = plain_goal(&mut fragment, "lower", 1.0);
        pile.commit(source, &signer, fragment).unwrap();
        let count = |pile: &mut Pile| {
            pile.snapshot()
                .unwrap()
                .records()
                .unwrap()
                .map(|record| record.unwrap())
                .filter(|record| {
                    matches!(
                        record,
                        triblespace::core::collection::CollectionRecord::Commit(_)
                    )
                })
                .count()
        };
        let before = count(&mut pile);
        assert!(pollster::block_on(prioritize_in(
            &mut pile,
            &signer,
            source.handle(),
            higher,
            lower,
            || bail!("cancelled fixture")
        ))
        .is_err());
        assert_eq!(count(&mut pile), before);
        let change = pollster::block_on(prioritize_in(
            &mut pile,
            &signer,
            source.handle(),
            higher,
            lower,
            || Ok(()),
        ))
        .unwrap();
        assert_eq!(
            (change.higher, change.lower, change.active),
            (higher, lower, true)
        );
        assert_eq!(count(&mut pile), before + 1);
        assert!(pollster::block_on(prioritize_in(
            &mut pile,
            &signer,
            source.handle(),
            higher,
            higher,
            || panic!("self-priority reached publication")
        ))
        .is_err());
        let denied = SigningKey::from_bytes(&[23; 32]);
        assert!(pollster::block_on(prioritize_in(
            &mut pile,
            &denied,
            source.handle(),
            lower,
            higher,
            || panic!("denied writer reached publication")
        ))
        .is_err());
        assert_eq!(count(&mut pile), before + 1);
        pile.close().unwrap();
    }

    #[test]
    fn borrowed_unclear_note_stays_unclear_until_a_note_supersedes_it_and_cancels_before_publication(
    ) {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        use triblespace::core::repo::pile::Pile;
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[24; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let source = pile
            .collection(
                "unclear fixture",
                CollectionPolicy::new(
                    AdmissionPolicy::Open,
                    AdmissionPolicy::direct(signer.verifying_key()),
                ),
            )
            .unwrap();
        let mut fragment = Fragment::empty();
        let goal = plain_goal(&mut fragment, "puzzling", 1.0);
        let other = plain_goal(&mut fragment, "plain", 1.0);
        pile.commit(source, &signer, fragment).unwrap();
        let count = |pile: &mut Pile| {
            pile.snapshot()
                .unwrap()
                .records()
                .unwrap()
                .map(|record| record.unwrap())
                .filter(|record| {
                    matches!(
                        record,
                        triblespace::core::collection::CollectionRecord::Commit(_)
                    )
                })
                .count()
        };
        let facts = |pile: &mut Pile| {
            let snapshot = pile.snapshot().unwrap();
            let read = crate::collection_names::open_workspace_read_in(
                &snapshot,
                signer.verifying_key(),
                source.handle(),
            )
            .unwrap();
            let succinct = pile.attach::<SuccinctArchiveBlob>(read, ()).unwrap();
            let rank9 = pile
                .attach::<Rank9AcceleratedSuccinctArchiveBlob>(read, succinct)
                .unwrap();
            storage::acquire_facts(&pile.snapshot().unwrap(), rank9).unwrap()
        };
        let before = count(&mut pile);
        assert!(pollster::block_on(mark_unclear_in(
            &mut pile,
            &signer,
            source.handle(),
            goal,
            None,
            || bail!("cancelled fixture")
        ))
        .is_err());
        assert_eq!(count(&mut pile), before);

        let added = pollster::block_on(mark_unclear_in(
            &mut pile,
            &signer,
            source.handle(),
            goal,
            None,
            || Ok(()),
        ))
        .unwrap();
        assert_eq!(added.goal, goal);
        assert_eq!(count(&mut pile), before + 1);
        let read = facts(&mut pile);
        assert_eq!(goals::unclear_goals(&read), BTreeSet::from([goal]));
        // Unclear is a note: no priority edge and no status was written.
        assert!(goals::active_priority_edges(&read).is_empty());
        assert_eq!(
            find!(event: Id, pattern!(&read, [{ ?event @ metadata::tag: &KIND_STATUS_ID }]))
                .count(),
            0
        );

        // An unrelated later note, such as progress, answers nothing.
        let (progress, _) = goals::note_fragment(
            goal,
            "Worked on it.",
            vec![],
            vec![],
            vec![],
            None,
            point(4.0e9),
        )
        .unwrap();
        pile.commit(source, &signer, progress).unwrap();
        assert_eq!(
            goals::unclear_goals(&facts(&mut pile)),
            BTreeSet::from([goal])
        );
        // The explanation supersedes the unclear note and clears it.
        let (explanation, _) = goals::note_fragment(
            goal,
            "It tracks the puzzle.",
            vec![],
            vec![],
            vec![added.note],
            None,
            point(4.1e9),
        )
        .unwrap();
        pile.commit(source, &signer, explanation).unwrap();
        assert!(goals::unclear_goals(&facts(&mut pile)).is_empty());

        assert!(pollster::block_on(mark_unclear_in(
            &mut pile,
            &signer,
            source.handle(),
            other,
            None,
            || Ok(())
        ))
        .is_ok());
        let denied = SigningKey::from_bytes(&[25; 32]);
        assert!(pollster::block_on(mark_unclear_in(
            &mut pile,
            &denied,
            source.handle(),
            goal,
            None,
            || panic!("denied writer reached publication")
        ))
        .is_err());
        assert_eq!(
            goals::unclear_goals(&facts(&mut pile)),
            BTreeSet::from([other])
        );
        pile.close().unwrap();
    }

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
        let status = goals::status_register_collection(pile, signer.verifying_key()).unwrap();
        pollster::block_on(async {
            drop(pile.maintain_attached(succinct, signer).await.unwrap());
            drop(pile.maintain_attached(rank9, signer).await.unwrap());
            drop(pile.maintain_attached(status, signer).await.unwrap());
        });
    }

    fn carry_storage(storage: GoalsStorage<'_>) {
        storage
            .storage
            .with_pile(|pile, signer| {
                let source = open_configured(pile, GOALS_SCOPE_ID, signer.verifying_key())?;
                carry(pile, source, signer);
                Ok(())
            })
            .unwrap();
    }

    fn sparse_view(
        mut fragment: Fragment,
    ) -> (
        AcquiringPile,
        goals::GoalsSnapshot,
        Collection<SimpleArchive>,
        SigningKey,
    ) {
        let signer = SigningKey::from_bytes(&[7; 32]);
        let mut store = AcquiringPile::new(fragment.blobs().clone(), signer.verifying_key());
        fragment.blobs_mut().keep([]);
        let source =
            crate::collection_names::open(&mut store.pile, GOALS_SCOPE_ID, signer.verifying_key())
                .unwrap();
        store.pile.commit(source, &signer, fragment).unwrap();
        carry(&mut store.pile, source, &signer);
        let view = pollster::block_on(goals::materialize_indexed_collection(
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
            crate::collection_names::open(&mut remote, GOALS_SCOPE_ID, signer.verifying_key())
                .unwrap();
        let descriptor: TribleSet = remote.snapshot().unwrap().get(source.handle()).unwrap();
        let name = triblespace::core::collection::descriptor::name(&descriptor)
            .unwrap()
            .unwrap();
        let mut store = AcquiringPile::new(remote.into_inner().blobs, signer.verifying_key());
        let before = store.snapshot().unwrap();

        let opened = pollster::block_on(storage::read(&mut store, &before, |reader| {
            open_exact_in(reader, GOALS_SCOPE_ID, source.handle())
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
        let (mut fragment, goal) = goals::goal_fragment("selected goal", vec![], None, at).unwrap();
        let (note, note_id) = goals::note_fragment(
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
            goals::goal_fragment("selected goal", vec!["shown".to_owned()], None, at).unwrap();
        fragment += goals::status_fragment(selected, "doing", None, at).unwrap();
        let (hidden, _) =
            goals::goal_fragment("other tag goal", vec!["other".to_owned()], None, at).unwrap();
        fragment += hidden;
        let (done, done_id) =
            goals::goal_fragment("done goal", vec!["shown".to_owned()], None, at).unwrap();
        fragment += done;
        fragment += goals::status_fragment(done_id, "done", None, at).unwrap();
        fragment += goals::note_fragment(
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
    fn priorities_do_not_acquire_hidden_relation_endpoint_titles() {
        let (mut fragment, parent) =
            goals::goal_fragment("hidden parent", vec![], None, point(1.0)).unwrap();
        let (child, selected) = goals::goal_fragment(
            "selected child",
            vec!["shown".to_owned()],
            Some(parent),
            point(2.0),
        )
        .unwrap();
        fragment += child;
        let title = find!(handle: TextHandle,
            pattern!(fragment.facts(), [{ selected @ board::title: ?handle }]))
        .next()
        .unwrap();
        let (mut store, view, _, _) = sparse_view(fragment);
        let output =
            pollster::block_on(storage::read(&mut store, view.store_snapshot(), |reader| {
                render_priorities(
                    reader,
                    view.facts(),
                    view.status_register(),
                    &[],
                    &["shown".to_owned()],
                    false,
                )
            }))
            .unwrap();
        assert!(output.text.contains("selected child"));
        assert!(!output.text.contains("hidden parent"));
        assert!(output.text.contains(&format!(
            "[{selected:x}] selected child > [{parent:x}]* (child-before-parent)"
        )));
        assert_eq!(store.requested, vec![title.transmute()]);
    }

    #[test]
    fn show_fetches_selected_goal_parent_note_and_reference_only() {
        let at: IntervalValue = (Epoch::from_tai_seconds(0.0), Epoch::from_tai_seconds(0.0))
            .try_to_inline()
            .unwrap();
        let (mut fragment, parent) = goals::goal_fragment("parent goal", vec![], None, at).unwrap();
        let (child, goal) =
            goals::goal_fragment("selected goal", vec![], Some(parent), at).unwrap();
        fragment += child;
        let (note, note_id) = goals::note_fragment(
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
            goals::goal_fragment("unrelated goal", vec![], None, at).unwrap();
        fragment += unrelated;
        fragment += goals::note_fragment(
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
        let (mut fragment, goal) = goals::goal_fragment("selected goal", vec![], None, at).unwrap();
        let (note, note_id) = goals::note_fragment(
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
        let (mut fragment, goal) = goals::goal_fragment("original goal", vec![], None, at).unwrap();
        fragment += goals::status_fragment(goal, "todo", None, at).unwrap();
        let (mut store, view, source, signer) = sparse_view(fragment);
        let original_support = source.admitted(view.store_snapshot()).unwrap();
        let (mut arrival, arrived_goal) =
            goals::goal_fragment("later goal", vec![], None, later).unwrap();
        arrival += goals::status_fragment(goal, "done", None, later).unwrap();
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
        assert!(!goals::goal_ids(view.facts()).contains(&arrived_goal));
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
                let source = open_configured(pile, GOALS_SCOPE_ID, signer.verifying_key())?;
                let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
                let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
                let status = goals::status_register_collection(pile, signer.verifying_key())?;
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
        let faculty = Goals::with_storage(storage.clone());
        let added = faculty.add("eager goal", AddOptions::default()).unwrap();
        let (first_facts, first_status) = observe();
        assert!(goals::goal_ids(&first_facts).contains(&added.goal));
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
        let storage = GoalsStorage {
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
            .with_view(|facts, _, _| Ok(*goals::goal_ids(facts).iter().next().unwrap()))
            .unwrap();

        // Goals preserves unrelated open-world facts. Multiple values on
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
                goals::commit_collection(pile, signer, fragment)?;
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

    /// A real pile whose Goals collection holds `fragment` as one raw
    /// commit, carried by the worker, so reads see exactly those facts.
    fn raw_pile(fragment: Fragment) -> (tempfile::TempDir, Storage) {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("open-compass.pile");
        let key = directory.path().join("open-compass.key");
        std::fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        let storage = Storage::new(pile, Some(key));
        let handle = GoalsStorage { storage: &storage };
        handle
            .with_pile(|pile, signer, _runtime| {
                goals::commit_collection(pile, signer, fragment)?;
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
        let (goal, id) = goals::goal_fragment(title, vec![], None, point(seconds)).unwrap();
        *fragment += goal;
        id
    }

    #[test]
    fn has_activity_joins_actor_current_status_and_exact_cutoff() {
        let actor = genid().id;
        let other = genid().id;
        let absent = genid().id;
        let mut fragment = Fragment::empty();
        let doing = plain_goal(&mut fragment, "doing", 1.0);
        let done = plain_goal(&mut fragment, "done", 1.0);
        let blocked = plain_goal(&mut fragment, "blocked", 1.0);
        for (goal, status) in [(doing, "doing"), (done, "done"), (blocked, "blocked")] {
            fragment += goals::status_fragment(goal, status, None, point(2.0)).unwrap();
        }
        fragment += goals::status_fragment(doing, "doing", Some(actor), point(50.0)).unwrap();
        for (goal, by, at) in [
            (doing, Some(actor), 100.25),
            (doing, None, 400.0),
            (done, Some(actor), 200.0),
            (blocked, Some(other), 300.0),
        ] {
            fragment += goals::note_fragment(goal, "body", vec![], vec![], vec![], by, point(at))
                .unwrap()
                .0;
        }
        // Many recent self-authored events on a now-done goal stay excluded.
        for second in 1_000..1_256 {
            fragment += goals::note_fragment(
                done,
                "done history",
                vec![],
                vec![],
                vec![],
                Some(actor),
                point(second as f64),
            )
            .unwrap()
            .0;
        }
        let missing_status = plain_goal(&mut fragment, "no status", 1.0);
        fragment += goals::note_fragment(
            missing_status,
            "no current status",
            vec![],
            vec![],
            vec![],
            Some(actor),
            point(600.0),
        )
        .unwrap()
        .0;
        let mover = genid().id;
        fragment += goals::status_fragment(blocked, "blocked", Some(mover), point(310.0)).unwrap();
        // Tags and priority events do not become attributed notes or moves.
        fragment += entity! { ExclusiveId::force_ref(&blocked) @
            board::tag: format!("{actor:x}"),
        };
        fragment += entity! {
            metadata::tag: &crate::schemas::compass::KIND_PRIORITY_ID,
            board::task: &doing,
            board::by: &actor,
            metadata::created_at: point(500.0),
        };
        let (_directory, storage) = raw_pile(fragment);
        let goals = Goals::with_storage(storage);
        let active = vec!["doing".to_owned(), "blocked".to_owned()];
        let at = |seconds| Epoch::from_unix_seconds(seconds);
        assert!(goals.has_activity(actor, at(100.25), &active).unwrap());
        assert!(!goals.has_activity(actor, at(100.250001), &active).unwrap());
        assert!(!goals.has_activity(absent, at(0.0), &active).unwrap());
        assert!(goals.has_activity(other, at(300.0), &active).unwrap());
        assert!(goals.has_activity(mover, at(310.0), &active).unwrap());
        assert!(!goals.has_activity(mover, at(310.000001), &active).unwrap());
        assert!(!goals
            .has_activity(actor, at(0.0), &["todo".into()])
            .unwrap());
        assert!(goals
            .has_activity(actor, at(200.0), &["done".into()])
            .unwrap());
        assert!(goals
            .has_activity(actor, at(50.0), &[" DOING ".into()])
            .unwrap());
        assert!(goals.has_activity(actor, at(0.0), &[]).is_err());
    }

    #[test]
    fn has_activity_reads_all_authors_without_text_payloads() {
        let mut authors = [genid().id, genid().id];
        authors.sort();
        let goal = genid().id;
        let note = genid().id;
        let parent = genid().id;
        let mut fragment = entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
            board::parent: &parent,
        };
        fragment += goals::status_fragment(goal, "blocked", None, point(1.0)).unwrap();
        fragment += entity! { ExclusiveId::force_ref(&note) @
            metadata::tag: &KIND_NOTE_ID,
            board::task: &goal,
            board::note*: ["first body", "second body"],
            board::by*: authors.iter(),
            metadata::created_at*: [point(2.0), point(3.0)],
            metadata::name: "unmodeled annotation",
        };
        // Keep only facts: status words, note bodies and annotations are absent.
        // The query needs their handles, never their payloads or a goal title.
        fragment.blobs_mut().keep([]);
        let (directory, storage) = raw_pile(fragment);
        let goals = Goals::with_storage(storage);
        for actor in authors {
            assert!(goals
                .has_activity(actor, Epoch::from_unix_seconds(3.0), &["blocked".into()],)
                .unwrap());
        }
        // The thin CLI delivers an actual JSON boolean with no prose wrapper.
        use clap::Parser;
        let cli = crate::goals::cli::Cli::try_parse_from([
            "goals".to_owned(),
            "--pile".into(),
            directory
                .path()
                .join("open-compass.pile")
                .display()
                .to_string(),
            "--key".into(),
            directory
                .path()
                .join("open-compass.key")
                .display()
                .to_string(),
            "has-activity".into(),
            "--actor".into(),
            format!("{:x}", authors[1]),
            "--since".into(),
            "1970-01-01T00:00:00 TAI".into(),
            "--status".into(),
            "blocked".into(),
        ])
        .unwrap();
        let mut text = String::new();
        let mut emit = |part| {
            if let crate::out::Part::Text { text: value } = part {
                text.push_str(&value);
            }
            Ok(())
        };
        crate::goals::cli::execute(cli, &mut crate::out::Out::new(&mut emit)).unwrap();
        assert_eq!(text, "true\n");
    }

    #[test]
    fn has_activity_preserves_latest_status_word_and_event_ties() {
        for repeated_words in [false, true] {
            let actor = genid().id;
            let goal = genid().id;
            let first = genid().id;
            let second = if repeated_words { first } else { genid().id };
            let mut fragment = entity! { ExclusiveId::force_ref(&goal) @
                metadata::tag: &KIND_GOAL_ID,
            };
            for (event, status) in [(first, "doing"), (second, "done")] {
                fragment += entity! { ExclusiveId::force_ref(&event) @
                    metadata::tag: &KIND_STATUS_ID,
                    board::status_of: &goal,
                    board::status: status,
                    metadata::created_at: point(2.0),
                };
            }
            fragment += goals::note_fragment(
                goal,
                "recent",
                vec![],
                vec![],
                vec![],
                Some(actor),
                point(3.0),
            )
            .unwrap()
            .0;
            let doing_wins = if repeated_words {
                crate::schemas::compass::text_handle("doing").raw
                    > crate::schemas::compass::text_handle("done").raw
            } else {
                first > second
            };
            let (_directory, storage) = raw_pile(fragment);
            let goals = Goals::with_storage(storage);
            assert_eq!(
                goals
                    .has_activity(actor, Epoch::from_unix_seconds(3.0), &["doing".into()],)
                    .unwrap(),
                doing_wins
            );
            assert_eq!(
                goals
                    .has_activity(actor, Epoch::from_unix_seconds(3.0), &["done".into()],)
                    .unwrap(),
                !doing_wins
            );
        }
    }

    #[test]
    fn has_activity_propagates_existing_status_order_conflict() {
        use triblespace::core::collection::lww_register::LwwRegisterError;

        let actor = genid().id;
        let goal = genid().id;
        let event = genid().id;
        let mut fragment = entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
        };
        fragment += entity! { ExclusiveId::force_ref(&event) @
            metadata::tag: &KIND_STATUS_ID,
            board::status_of: &goal,
            board::status: "doing",
            board::by: &actor,
            metadata::created_at*: [point(2.0), point(2.5)],
        };
        let (_directory, storage) = raw_pile(fragment);
        let goals = Goals::with_storage(storage);
        // The shared view's LWW query rejects distinct order values on a
        // complete state before either the board or activity reader runs.
        let expected = LwwRegisterError::ConflictingOrder(event.raw());
        let board_error = goals.board(ListOptions::default()).unwrap_err();
        assert_eq!(
            board_error.downcast_ref::<LwwRegisterError>(),
            Some(&expected)
        );
        let activity_error = goals
            .has_activity(actor, Epoch::from_unix_seconds(1.0), &["doing".into()])
            .unwrap_err();
        assert_eq!(
            activity_error.downcast_ref::<LwwRegisterError>(),
            Some(&expected)
        );
    }

    #[test]
    fn has_activity_skips_incomplete_and_inverted_readings() {
        let actor = genid().id;
        let goal = genid().id;
        let mut invalid = point(4.0).raw;
        invalid[16..].copy_from_slice(&point(3.0).raw[16..]);
        let mut fragment = entity! { ExclusiveId::force_ref(&goal) @
            metadata::tag: &KIND_GOAL_ID,
        };
        fragment += goals::status_fragment(goal, "doing", None, point(1.0)).unwrap();
        fragment += entity! {
            metadata::tag: &KIND_NOTE_ID,
            board::task: &goal,
            board::note: "invalid interval",
            board::by: &actor,
            metadata::created_at: IntervalValue::new(invalid),
        };
        fragment += entity! {
            metadata::tag: &KIND_NOTE_ID,
            board::task: &goal,
            board::by: &actor,
            metadata::created_at: point(5.0),
        };
        // A timestamp and author alone do not make an activity event.
        fragment += entity! {
            board::task: &goal,
            board::note: "no note kind",
            board::by: &actor,
            metadata::created_at: point(6.0),
        };
        let wide_actor = genid().id;
        let wide: IntervalValue = (
            Epoch::from_unix_seconds(2.0),
            Epoch::from_unix_seconds(10.0),
        )
            .try_to_inline()
            .unwrap();
        fragment += goals::note_fragment(
            goal,
            "wide interval",
            vec![],
            vec![],
            vec![],
            Some(wide_actor),
            wide,
        )
        .unwrap()
        .0;
        let (_directory, storage) = raw_pile(fragment);
        let goals = Goals::with_storage(storage);
        assert!(!goals
            .has_activity(actor, Epoch::from_unix_seconds(2.0), &["doing".into()],)
            .unwrap());
        assert!(goals
            .has_activity(wide_actor, Epoch::from_unix_seconds(2.0), &["doing".into()],)
            .unwrap());
        assert!(!goals
            .has_activity(
                wide_actor,
                Epoch::from_unix_seconds(2.000001),
                &["doing".into()],
            )
            .unwrap());
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
        let handle = GoalsStorage { storage: &storage };

        let board = list(handle, vec![], vec![], false).unwrap();
        assert_eq!(board, list(handle, vec![], vec![], false).unwrap());
        assert!(
            board.contains(&format!("  - [{goal:x}] alpha / beta\n")),
            "{board}"
        );
        assert_eq!(board.matches(&format!("[{goal:x}]")).count(), 1, "{board}");

        let shown = show(handle, format!("{goal:x}")).unwrap().text;
        assert_eq!(shown, show(handle, format!("{goal:x}")).unwrap().text);
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
        let handle = GoalsStorage { storage: &storage };

        let board = list(handle, vec![], vec![], false).unwrap();
        assert!(board.contains("noted goal (1 note)\n"), "{board}");
        let shown = show(handle, format!("{goal:x}")).unwrap().text;
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
        let handle = GoalsStorage { storage: &storage };
        let shown = show(handle, format!("{goal:x}")).unwrap().text;
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
        let handle = GoalsStorage { storage: &storage };

        let board = list(handle, vec![], vec![], false).unwrap();
        assert!(board.contains("inverted goal (1 note)"), "{board}");
        assert!(board.contains("ordinary goal"), "{board}");
        let shown = show(handle, format!("{goal:x}")).unwrap().text;
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
        fragment += goals::note_fragment(
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
        fragment += goals::status_fragment(absent, "doing", None, point(2.0)).unwrap();
        let (bottom, bottom_id) = goals::priority_snapshot_fragment(goal, [], point(2.0));
        fragment += bottom;
        fragment += goals::priority_snapshot_fragment(absent, [bottom_id], point(2.0)).0;
        let (_directory, storage) = raw_pile(fragment);
        let handle = GoalsStorage { storage: &storage };

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
        let (early, early_id) = goals::note_fragment(
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
        let handle = GoalsStorage { storage: &storage };

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
        let shown = show(handle, format!("{goal:x}")).unwrap().text;
        assert!(shown.contains(&format!("[{:x}]", added.note)), "{shown}");
        assert!(
            shown.contains(&format!("supersedes: {early_id:x}")),
            "{shown}"
        );
    }

    /// The over-set of a priority snapshot, sorted.
    fn over_set<P: TriblePattern>(facts: &P, snapshot: Id) -> BTreeSet<Id> {
        find!(
            lower: Id,
            pattern!(facts, [{ snapshot @ board::over: ?lower }])
        )
        .collect()
    }

    /// A pairwise priority event as Goals wrote them before snapshots.
    fn pairwise(fragment: &mut Fragment, higher: Id, lower: Id, seconds: f64) {
        *fragment += entity! {
            metadata::tag: &crate::schemas::compass::KIND_PRIORITIZE_ID,
            board::higher: &higher,
            board::lower: &lower,
            metadata::created_at: point(seconds),
        };
    }

    /// `prioritize` writes a priority snapshot over the current snapshots of
    /// every goal the higher goal is above, plus the new one; `deprioritize`
    /// writes one without the lower goal. A pairwise event in the pile is
    /// neither read nor carried over, nor retracted.
    #[test]
    fn prioritize_writes_a_snapshot_and_leaves_pairwise_events_unread() {
        let mut fragment = Fragment::empty();
        let a = plain_goal(&mut fragment, "alpha", 1.0);
        let b = plain_goal(&mut fragment, "beta", 2.0);
        let x = plain_goal(&mut fragment, "chi", 3.0);
        pairwise(&mut fragment, a, x, 4.0);
        let (_directory, storage) = raw_pile(fragment);
        let handle = GoalsStorage { storage: &storage };

        let change = prioritize(handle, format!("{a:x}"), format!("{b:x}"), None).unwrap();
        assert!(change.active);
        let second = prioritize(handle, format!("{a:x}"), format!("{x:x}"), None).unwrap();
        handle
            .with_view(|facts, _, _| {
                assert!(!exists!(pattern!(facts, [{ _?event @
                    metadata::tag: &crate::schemas::compass::KIND_DEPRIORITIZE_ID,
                }])));
                let reading = goals::priority_reading(facts);
                assert_eq!(reading.current[&a], BTreeSet::from([second.event]));
                let mut expected = reading.current[&b].clone();
                expected.extend(reading.current[&x].iter().copied());
                assert_eq!(over_set(facts, second.event), expected);
                assert_eq!(
                    goals::active_priority_edges(facts),
                    BTreeSet::from([(a, b), (a, x)])
                );
                Ok(())
            })
            .unwrap();

        let removed = deprioritize(handle, format!("{a:x}"), format!("{x:x}"), None).unwrap();
        assert!(!removed.active);
        handle
            .with_view(|facts, _, _| {
                let reading = goals::priority_reading(facts);
                assert_eq!(reading.current[&a], BTreeSet::from([removed.event]));
                assert_eq!(over_set(facts, removed.event), reading.current[&b].clone());
                assert_eq!(
                    goals::active_priority_edges(facts),
                    BTreeSet::from([(a, b)])
                );
                Ok(())
            })
            .unwrap();
        assert!(deprioritize(handle, format!("{a:x}"), format!("{x:x}"), None).is_err());

        let board = board(handle, vec![], vec![], false).unwrap();
        assert!(board.notes.is_empty(), "{:?}", board.notes);
        let alpha = board.text.find("alpha").unwrap();
        assert!(alpha < board.text.find("beta").unwrap(), "{}", board.text);
    }

    /// A board whose only priority data is pairwise events shows no priority
    /// relation: the goals are in recency order, as with no priorities.
    #[test]
    fn a_pairwise_only_board_shows_no_priority_relation() {
        let mut fragment = Fragment::empty();
        let older = plain_goal(&mut fragment, "older", 1.0);
        let newer = plain_goal(&mut fragment, "newer", 2.0);
        pairwise(&mut fragment, older, newer, 3.0);
        let (_directory, storage) = raw_pile(fragment);
        let handle = GoalsStorage { storage: &storage };
        assert_eq!(
            board(handle, vec![], vec![], false).unwrap(),
            Board {
                text: format!("\n== TODO (2) ==\n- [{newer:x}] newer\n- [{older:x}] older\n\n"),
                notes: vec![],
            }
        );
        handle
            .with_view(|facts, _, _| {
                assert!(goals::active_priority_edges(facts).is_empty());
                Ok(())
            })
            .unwrap();
    }

    /// `prioritize B A` while A > B is a flip, not a cycle: B's new snapshot
    /// ranks over A's current one, so A's claim over B's replaced snapshot
    /// lapses and the board reads B > A only.
    #[test]
    fn prioritize_flips_an_existing_order() {
        let mut fragment = Fragment::empty();
        let b = plain_goal(&mut fragment, "beta", 1.0);
        let a = plain_goal(&mut fragment, "alpha", 2.0);
        let (_directory, storage) = raw_pile(fragment);
        let handle = GoalsStorage { storage: &storage };

        prioritize(handle, format!("{a:x}"), format!("{b:x}"), None).unwrap();
        let flipped = prioritize(handle, format!("{b:x}"), format!("{a:x}"), None).unwrap();
        assert_eq!((flipped.higher, flipped.lower), (b, a));
        handle
            .with_view(|facts, _, _| {
                assert_eq!(
                    goals::active_priority_edges(facts),
                    BTreeSet::from([(b, a)])
                );
                Ok(())
            })
            .unwrap();
        let board = board(handle, vec![], vec![], false).unwrap();
        assert!(board.notes.is_empty(), "{:?}", board.notes);
        assert!(
            board.text.find("beta").unwrap() < board.text.find("alpha").unwrap(),
            "{}",
            board.text
        );
        // Flipping back is a flip too.
        prioritize(handle, format!("{a:x}"), format!("{b:x}"), None).unwrap();
        handle
            .with_view(|facts, _, _| {
                assert_eq!(
                    goals::active_priority_edges(facts),
                    BTreeSet::from([(a, b)])
                );
                Ok(())
            })
            .unwrap();
    }

    /// A flip lapses only claims over the flipped goal's replaced snapshot
    /// that its new snapshot reaches. A cycle closed through a claim it does
    /// not reach would still be on the board after the write, so the write
    /// is refused; so is ranking a parent over its own child.
    #[test]
    fn a_write_that_would_leave_a_cycle_is_still_refused() {
        let mut fragment = Fragment::empty();
        let a = plain_goal(&mut fragment, "alpha", 1.0);
        let b = plain_goal(&mut fragment, "beta", 2.0);
        let c = plain_goal(&mut fragment, "gamma", 3.0);
        let (b0, b0_id) = goals::priority_snapshot_fragment(b, [], point(4.0));
        let (c0, c0_id) = goals::priority_snapshot_fragment(c, [], point(4.0));
        fragment += b0;
        fragment += c0;
        // A over C's first snapshot; C, re-prioritised later, over B.
        fragment += goals::priority_snapshot_fragment(a, [c0_id], point(5.0)).0;
        fragment += goals::priority_snapshot_fragment(c, [b0_id], point(6.0)).0;
        let (child_record, child) =
            goals::goal_fragment("child", vec![], Some(a), point(7.0)).unwrap();
        fragment += child_record;
        let (_directory, storage) = raw_pile(fragment);
        let handle = GoalsStorage { storage: &storage };
        handle
            .with_view(|facts, _, _| {
                assert_eq!(
                    goals::active_priority_edges(facts),
                    BTreeSet::from([(a, c), (c, b)])
                );
                Ok(())
            })
            .unwrap();

        let refused = prioritize(handle, format!("{b:x}"), format!("{a:x}"), None).unwrap_err();
        assert!(
            format!("{refused:#}").contains("priority cycle"),
            "{refused:#}"
        );
        let refused = prioritize(handle, format!("{a:x}"), format!("{child:x}"), None).unwrap_err();
        assert!(
            format!("{refused:#}").contains("children are implicitly prioritized"),
            "{refused:#}"
        );
    }

    /// Concurrent mutual rankings reach the board as a cycle: it is listed,
    /// the same on every read, with a note beside it, never refused.
    #[test]
    fn a_priority_cycle_on_the_board_is_listed_and_noted() {
        let mut fragment = Fragment::empty();
        let a = plain_goal(&mut fragment, "alpha", 1.0);
        let b = plain_goal(&mut fragment, "beta", 2.0);
        let (a0, a0_id) = goals::priority_snapshot_fragment(a, [], point(3.0));
        let (b0, b0_id) = goals::priority_snapshot_fragment(b, [], point(3.0));
        fragment += a0;
        fragment += b0;
        fragment += goals::priority_snapshot_fragment(a, [b0_id], point(4.0)).0;
        fragment += goals::priority_snapshot_fragment(b, [a0_id], point(4.0)).0;
        let (_directory, storage) = raw_pile(fragment);
        let handle = GoalsStorage { storage: &storage };

        let first = board(handle, vec![], vec![], false).unwrap();
        assert_eq!(first, board(handle, vec![], vec![], false).unwrap());
        assert!(first.text.contains("alpha") && first.text.contains("beta"));
        assert_eq!(first.notes.len(), 1, "{:?}", first.notes);
        assert!(
            first.notes[0].contains("priority cycle"),
            "{:?}",
            first.notes
        );
        let priorities = Goals::with_storage(storage)
            .priorities(ListOptions::default())
            .unwrap();
        assert_eq!(priorities.notes, first.notes);
        assert!(priorities.text.contains(&format!("[{a:x}] alpha")));
        assert!(priorities.text.contains(&format!("[{b:x}] beta")));
        assert!(priorities
            .text
            .contains(&format!("[{a:x}] alpha > [{b:x}] beta (explicit)")));
        assert!(priorities
            .text
            .contains(&format!("[{b:x}] beta > [{a:x}] alpha (explicit)")));
    }

    #[test]
    fn priority_authorship_annotates_every_new_snapshot_without_changing_identity() {
        let higher = genid().id;
        let lower = genid().id;
        let actor = genid().id;
        let plan = || PriorityPlan {
            over: BTreeMap::from([(lower, BTreeSet::new())]),
        };
        let (plain, plain_id) = author_priority(plan(), higher, point(1.0), None);
        let (authored, authored_id) = author_priority(plan(), higher, point(1.0), Some(actor));
        assert_eq!(plain_id, authored_id);
        let plain_reading = goals::priority_reading(plain.facts());
        assert_eq!(plain_reading, goals::priority_reading(authored.facts()));
        for snapshots in plain_reading.current.values() {
            for &snapshot in snapshots {
                assert!(exists!(
                    pattern!(authored.facts(), [{ snapshot @ board::by: &actor }])
                ));
            }
        }
        assert!(!exists!(
            pattern!(plain.facts(), [{ _?snapshot @ board::by: _?actor }])
        ));
    }

    /// Where no priorities are involved the board is exactly what it was,
    /// and carries no note.
    #[test]
    fn a_board_without_priorities_is_unchanged_and_unnoted() {
        let mut fragment = Fragment::empty();
        let first = plain_goal(&mut fragment, "first", 1.0);
        let second = plain_goal(&mut fragment, "second", 2.0);
        let (_directory, storage) = raw_pile(fragment);
        let handle = GoalsStorage { storage: &storage };
        assert_eq!(
            board(handle, vec![], vec![], false).unwrap(),
            Board {
                text: format!("\n== TODO (2) ==\n- [{second:x}] second\n- [{first:x}] first\n\n"),
                notes: vec![],
            }
        );
    }

    /// A Goals pile whose collection holds `resident`, then one more
    /// foundation put there by `plant` through the raw pile.
    fn goals_with_residual(
        plant: impl FnOnce(
            &mut triblespace::core::repo::pile::Pile,
            triblespace::core::collection::Collection<SimpleArchive>,
            &SigningKey,
        ) -> Result<Inline<inlineencodings::Handle<SimpleArchive>>>,
    ) -> (
        tempfile::TempDir,
        Storage,
        Id,
        Inline<inlineencodings::Handle<SimpleArchive>>,
    ) {
        let mut fragment = Fragment::empty();
        let resident = plain_goal(&mut fragment, "resident goal", 1.0);
        let (directory, storage) = raw_pile(fragment);
        let planted = storage
            .with_pile(|pile, signer| {
                let source = open_configured(pile, GOALS_SCOPE_ID, signer.verifying_key())?;
                plant(pile, source, signer)
            })
            .unwrap();
        (directory, storage, resident, planted)
    }

    /// 26 November 2026: one foundation stranded on another host and every
    /// Goals command refused. Appends now go on, the board and a goal are
    /// shown from what is here with a note naming what is not, and only an
    /// answer absence would invalidate (activity, a priority snapshot that
    /// replaces the one it saw) still refuses.
    #[test]
    fn goals_go_on_while_a_foundation_is_away() {
        let (_directory, storage, resident, away) = goals_with_residual(|pile, source, signer| {
            let mut fragment = Fragment::empty();
            plain_goal(&mut fragment, "away goal", 2.0);
            crate::storage::residual_fixture::strand(pile, source, signer, fragment)
        });
        let away_hex = hex::encode(away.raw);
        let named = |notes: &[String]| notes.iter().any(|note| note.contains(&away_hex));
        let goals = Goals::with_storage(storage);
        let resident_hex = format!("{resident:x}");

        // (a) Appends are stored; those that resolved a goal say what their
        // read could not load. A goal without a parent reads no Goals facts.
        let added = goals.add("added meanwhile", AddOptions::default()).unwrap();
        assert!(added.notes.is_empty(), "{:?}", added.notes);
        let child = goals
            .add(
                "child meanwhile",
                AddOptions {
                    parent: Some(&resident_hex),
                    ..AddOptions::default()
                },
            )
            .unwrap();
        assert!(named(&child.notes), "{:?}", child.notes);
        let moved = goals.move_goal(&resident_hex, "doing", None).unwrap();
        assert!(named(&moved.notes), "{:?}", moved.notes);
        let noted = goals
            .note(&resident_hex, "still writable", NoteOptions::default())
            .unwrap();
        assert!(named(&noted.notes), "{:?}", noted.notes);
        let tagged = goals.tag(&resident_hex, &["outage".to_owned()]).unwrap();
        assert!(named(&tagged.notes), "{:?}", tagged.notes);

        // (b) The board shows what is here, and says what is not.
        let board = goals.board(ListOptions::default()).unwrap();
        for title in ["resident goal", "added meanwhile", "child meanwhile"] {
            assert!(board.text.contains(title), "{}", board.text);
        }
        assert!(!board.text.contains("away goal"), "{}", board.text);
        assert!(named(&board.notes), "{:?}", board.notes);
        let mut parts = Vec::new();
        super::super::render::board(
            &board,
            &mut crate::out::Out::new(&mut |part| {
                parts.push(part);
                Ok(())
            }),
        )
        .unwrap();
        assert!(parts.iter().any(|part| matches!(
            part,
            crate::out::Part::Note { text } if text.contains(&away_hex)
        )));
        let shown = goals.show(&resident_hex).unwrap();
        assert!(shown.text.contains("resident goal"), "{}", shown.text);
        assert!(named(&shown.notes), "{:?}", shown.notes);
        let priorities = goals.priorities(ListOptions::default()).unwrap();
        assert!(named(&priorities.notes), "{:?}", priorities.notes);
        // A goal not found may be the one away, and the error says so. (A
        // full id resolves without a lookup, so ask by a prefix.)
        let error = goals.show("fffffff").unwrap_err();
        assert!(format!("{error:#}").contains(&away_hex), "{error:#}");

        // Absence still refuses where it would change the answer.
        let error = goals
            .has_activity(genid().id, Epoch::from_unix_seconds(0.0), &["doing".into()])
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<storage::IncompleteAttachedRead>()
                .is_some(),
            "{error:#}"
        );
        let error = goals
            .prioritize(&resident_hex, &format!("{:x}", added.goal), None)
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<storage::IncompleteAttachedRead>()
                .is_some(),
            "{error:#}"
        );
    }

    /// Bytes that are here and form no archive are a fault, not a foundation
    /// that is away: the board fails loudly.
    #[test]
    fn goals_fail_loudly_on_a_malformed_foundation() {
        let (_directory, storage, resident, _) =
            goals_with_residual(crate::storage::residual_fixture::malformed);
        let goals = Goals::with_storage(storage);
        let error = goals.board(ListOptions::default()).unwrap_err();
        assert!(
            error
                .downcast_ref::<storage::IncompleteAttachedRead>()
                .is_none(),
            "a fault is not a gap: {error:#}"
        );
        assert!(goals.show(&format!("{resident:x}")).is_err());
    }
}
