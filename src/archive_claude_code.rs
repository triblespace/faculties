//! Deterministic Claude Code JSONL projection onto Archive's canonical block DAG.
//!
//! This module deliberately stops at the source-adapter boundary. It scans a
//! file or directory, projects each source file into bounded,
//! attachment-complete [`Fragment`]s, and hands those fragments to a
//! caller-provided sink. A child
//! fragment may name a predecessor catalog entity carried by another emitted
//! fragment, so callers must stage the complete emitted union through
//! `ArchiveImportWriter` and publish it as one unit (`commit_unit`). The adapter
//! knows nothing about Archive's CLI, legacy repositories, branches,
//! workspaces, or signing policy.
//!
//! Directory projection first scans the complete semantic source graph. Exact
//! `forkedFrom` aliases are quotiented first. Only non-fork parent assertions
//! enter the semantic graph; contentless records are contracted, and narrowly
//! classified reciprocal `system/turn_duration` telemetry backlinks are
//! removed before any intrinsic block identity is constructed. Blocks are then
//! planned over the complete record DAG, so cross-file references and Rayon
//! completion order cannot affect identity. Provider uuids thread that graph
//! while projecting and are never stored: a node is its predecessors and the
//! fact it holds, and its receipt is the source namespace and the node.
//! Copies of a record that differ only in time agree: time is annotation.
//! Copies under one uuid that say different things (a fork replayed by an
//! older Claude Code, an attachment rendered anew after an upgrade) are
//! different records, each in its own position; only copies that say the
//! same thing collapse, and a fork copy is its origin only where they agree.
//!
//! Every fact of every record with content is one node in the main chain, in
//! the order the agent experienced it: user and assistant records, harness
//! context the model saw (`attachment` records) and `system` records, all of
//! the last two `in`. Sealed reasoning (a thinking block's `signature`, a
//! `redacted_thinking` block's `data`) is a THINKING blob fact right after
//! the thinking it seals. Claude Code runs a tool as soon as its call is
//! complete and logs the result before the message's next call; the model
//! emitted every call of one message (one `message.id`) before it perceived
//! any result, so what was logged inside a message follows the whole message.
//! A message is complete once the transcript shows a record of another
//! message or a user prompt; until then what was logged inside it is held
//! back, so a longer transcript only appends nodes. Only the branch a file
//! wrote last is held back: a branch end written before it (the history
//! before a compaction or a rewind) is one the file has moved on from, and
//! its open message is complete. A tool result's node
//! names the node of the call it answers. Each emitted fragment is one node
//! with its fact and receipt. The session id annotates the receipts of the
//! file's leaves only; a subagent's `.meta.json` annotates its transcript's
//! leaves. Titles and the file's path are not kept.
//! `history.jsonl` prompts that no transcript beside it holds become nodes
//! of their own. No source bytes are stored; [`SKIPPED_KINDS`] lists what is
//! not projected and why.

/// Record kinds and fields that project to nothing, with the reason. Anything
/// else of consequence is a block.
pub const SKIPPED_KINDS: &[(&str, &str)] = &[
    ("attachment/total_tokens_reminder", "token-count bookkeeping"),
    ("system/turn_duration", "turn timing bookkeeping"),
    (
        "progress",
        "streaming tool and hook states the final result supersedes; agent_progress copies subagent transcripts",
    ),
    ("queue-operation", "duplicates the queued_command attachment"),
    (
        "file-history-snapshot, file-history-delta",
        "pointers into the local file-history backup directory",
    ),
    (
        "last-prompt, mode, permission-mode, atis-latch, frame-link, agent-setting, cost-state, agent-name, started, launched, result, continued-in, fork-context-ref, pr-link",
        "UI and session state",
    ),
    (
        "ai-title, custom-title",
        "a label for the session, not part of it",
    ),
    (
        "toolUseResult",
        "client-side structured mirror of the tool_result content block",
    ),
    (
        "record envelope (cwd, gitBranch, version, slug, userType, entrypoint, requestId, ...)",
        "per-record client state",
    ),
];

/// Top-level fields of a harness record that are envelope, not content.
const ENVELOPE_KEYS: &[&str] = &[
    "parentUuid",
    "logicalParentUuid",
    "isSidechain",
    "type",
    "uuid",
    "timestamp",
    "sessionId",
    "session_id",
    "userType",
    "entrypoint",
    "cwd",
    "version",
    "gitBranch",
    "slug",
    "sessionKind",
    "agentId",
    "requestId",
    "promptId",
    "forkedFrom",
];

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use hifitime::Epoch;
use rayon::prelude::*;
use triblespace::core::blob::Bytes;
use triblespace::core::import::scanner as sc;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

use crate::schemas::blockdag as schema;
use crate::{archive_source, blockdag, files};

const TOOL_RESULT_STATUS_OK: &str = "urn:triblespace:archive:tool-result-status:v1:ok";
const TOOL_RESULT_STATUS_ERROR: &str = "urn:triblespace:archive:tool-result-status:v1:error";
/// The model label Claude Code writes on assistant records it made itself.
const SYNTHETIC_MODEL: &str = "<synthetic>";
/// Observable projection accounting. Missing or unresolved source evidence is
/// counted rather than silently replaced with invented values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectionStats {
    /// Non-empty JSONL records scanned.
    pub records_seen: usize,
    /// Distinct nodes emitted, one per fact.
    pub source_projections: usize,
    /// Sealed reasoning tokens kept as THINKING blob facts.
    pub sealed_tokens: usize,
    /// `history.jsonl` prompts read.
    pub history_prompts: usize,
    /// Of those, prompts a transcript beside the log already holds; they
    /// land on the transcript's blocks and project nothing here.
    pub history_in_transcripts: usize,
    /// Facts emitted, one node each, before set-level deduplication.
    pub content_parts: usize,
    /// Records projected to nothing: bookkeeping kinds, contentless records,
    /// and records without a `(sessionId, uuid)` identity. See
    /// [`SKIPPED_KINDS`].
    pub skipped_records: usize,
    /// Dialogue records lacking either `sessionId` or `uuid`.
    pub missing_source_identity: usize,
    /// `parentUuid` references whose target source record has neither a block
    /// nor a forwarded projected ancestor.
    pub skipped_parents: usize,
    /// `parentUuid` references whose source key is absent from the imported
    /// corpus. Semantic cycles are rejected before any fragment is emitted.
    pub unresolved_parents: usize,
    /// `tool_result` correlators with no matching explicit source class or raw
    /// `(sessionId, tool_use_id)` binding.
    pub unresolved_tool_results: usize,
    /// Image sources omitted because neither bytes nor a usable pointer existed.
    pub undecodable_images: usize,
    /// Records held back at the tail: what arrived inside a message the
    /// transcript does not yet show complete. They land on a later import.
    pub held_back_records: usize,
    /// The facts of those records: the nodes a later import adds for them.
    pub held_back_facts: usize,
}

impl ProjectionStats {
    fn absorb(&mut self, other: Self) {
        self.records_seen += other.records_seen;
        self.source_projections += other.source_projections;
        self.sealed_tokens += other.sealed_tokens;
        self.history_prompts += other.history_prompts;
        self.history_in_transcripts += other.history_in_transcripts;
        self.content_parts += other.content_parts;
        self.held_back_records += other.held_back_records;
        self.held_back_facts += other.held_back_facts;
        self.skipped_records += other.skipped_records;
        self.missing_source_identity += other.missing_source_identity;
        self.skipped_parents += other.skipped_parents;
        self.unresolved_parents += other.unresolved_parents;
        self.unresolved_tool_results += other.unresolved_tool_results;
        self.undecodable_images += other.undecodable_images;
    }
}

/// One bounded projection fragment plus every blob its facts reference.
///
/// `source_path` is for diagnostics only and is never stored: moving a
/// transcript changes nothing the import publishes.
#[derive(Debug)]
pub struct ProjectedFile {
    pub source_path: PathBuf,
    pub fragment: Fragment,
    /// The part of `fragment` that is node identity; the rest annotates.
    pub identity: blockdag::Identity,
    pub stats: ProjectionStats,
}

/// Corpus-level result returned after every projected fragment has reached
/// `emit`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectionSummary {
    /// JSONL files scanned, including files containing no projectable records.
    pub files_scanned: usize,
    /// Non-empty bounded fragments handed to the sink.
    pub fragments_emitted: usize,
    pub stats: ProjectionStats,
}

/// Project one Claude Code JSONL file or a recursively scanned directory.
///
/// Files are discovered and emitted deterministically. Each projected record
/// is emitted immediately as a bounded fragment, followed by the file's leaf
/// annotations. The caller must stage every supplied
/// fragment into one `ArchiveImportWriter` and publish (`commit_unit` or
/// `finish`) only after projection succeeds; publishing fragments independently can expose an
/// invalid partial catalog. The projector itself performs no pile writes.
pub fn project_path<F>(path: &Path, emit: F) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    project_path_finished(path, None, emit)
}

/// [`project_path`], treating every transcript last modified before
/// `finished_before` as finished.
///
/// The transcript itself never says that a message is complete when it ends
/// on what was logged inside it (a session stopped after a tool ran, a
/// subagent ending on its StructuredOutput result), so such a tail is held
/// back. A file older than the cutoff is finished: its held-back tail is
/// placed after its message, exactly as if a later record had closed it. The
/// modification time only decides WHEN that tail is emitted, never what any
/// node or receipt is: identity stays free of time.
pub fn project_path_finished<F>(
    path: &Path,
    finished_before: Option<SystemTime>,
    mut emit: F,
) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    if path.is_dir() {
        return project_directory(path, finished_before, &mut emit);
    }
    if path.file_name().and_then(|name| name.to_str()) == Some(HISTORY_FILE) {
        return project_history(path, &mut emit);
    }
    // A subagent's `.meta.json` annotates its transcript's leaves, so it is
    // projected through that transcript.
    let path = &match path
        .to_str()
        .and_then(|name| name.strip_suffix(".meta.json"))
    {
        Some(stem) => PathBuf::from(format!("{stem}.jsonl")),
        None => path.to_path_buf(),
    };

    let (prescan, snapshot) = freeze_and_prescan(path, finished_before)?;
    let plan = SourcePlan::from_scans(std::slice::from_ref(&prescan))
        .context("plan canonical Claude Code lineage")?;
    let meta = subagent_meta(path)?;
    project_snapshot(path, snapshot, &prescan, &plan, meta, &mut emit)
}

/// Freeze one possibly live transcript once and pre-scan exactly the frozen
/// bytes, so a file still being written is planned and projected from one
/// read. The modification time is read AFTER the bytes, from the same
/// handle: a write after the freeze can only make the file look unfinished,
/// never close a message the frozen bytes leave open. In an unfinished file a
/// last line without its newline that does not scan as a record is one still
/// being written: it is left out, and lands on a later import like the rest
/// of the held-back tail. (A last line that does scan is the same record
/// with or without its newline, so it is kept, as it always was.)
fn freeze_and_prescan(
    path: &Path,
    finished_before: Option<SystemTime>,
) -> Result<(PreScan, archive_source::FrozenSource)> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    let frozen = archive_source::freeze_open_file(&mut file, path, len)?;
    let finished = match finished_before {
        Some(cutoff) => {
            file.metadata()
                .and_then(|metadata| metadata.modified())
                .with_context(|| format!("read the modification time of {}", path.display()))?
                < cutoff
        }
        None => false,
    };
    let complete = frozen
        .bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |end| end + 1);
    let partial = &frozen.bytes[complete..];
    let writing = !finished
        && !partial.iter().all(u8::is_ascii_whitespace)
        && scanner_bytes(partial)
            .ok()
            .and_then(|mut bytes| scan_record(&mut bytes).ok())
            .is_none();
    let bytes = if writing {
        frozen.bytes.slice(..complete)
    } else {
        frozen.bytes
    };
    let mut scan = prescan_reader(&mut std::io::Cursor::new(bytes.as_ref()), path)?;
    scan.finished = finished;
    let snapshot = archive_source::FrozenSource {
        digest: scan.digest,
        bytes,
    };
    Ok((scan, snapshot))
}

/// The prompt log Claude Code keeps beside `projects/`.
const HISTORY_FILE: &str = "history.jsonl";

/// A subagent's `.meta.json`, as canonical JSON.
#[derive(Clone, Debug, Default)]
struct SubagentMeta {
    detail: String,
}

/// Read `agent-<id>.meta.json` beside `agent-<id>.jsonl`, if there is one.
fn subagent_meta(transcript: &Path) -> Result<Option<SubagentMeta>> {
    let Some(stem) = transcript
        .to_str()
        .and_then(|name| name.strip_suffix(".jsonl"))
    else {
        return Ok(None);
    };
    let meta = PathBuf::from(format!("{stem}.meta.json"));
    if !meta.is_file() {
        return Ok(None);
    }
    let bytes = archive_source::read_file(&meta)?;
    let detail = archive_source::canonical_json(bytes.clone())
        .map_err(|error| anyhow!("parse {}: {error}", meta.display()))?;
    Ok(Some(SubagentMeta { detail }))
}

/// Project an immutable resident JSONL stream with the same canonical lineage
/// plan as a single CLI file. The name is provenance
/// only; all scans use the supplied allocation and never open host paths.
pub fn project_bytes<F>(source_name: &str, bytes: Bytes, mut emit: F) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    let path = Path::new(source_name);
    let prescan = prescan_reader(&mut std::io::Cursor::new(bytes.as_ref()), path)?;
    let plan = SourcePlan::from_scans(std::slice::from_ref(&prescan))
        .context("plan canonical resident Claude Code lineage")?;
    let snapshot = archive_source::FrozenSource {
        digest: *blake3::hash(bytes.as_ref()).as_bytes(),
        bytes,
    };
    project_snapshot(path, snapshot, &prescan, &plan, None, &mut emit)
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct SourceKey {
    session: String,
    uuid: String,
}

impl SourceKey {
    fn new(session: impl Into<String>, uuid: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            uuid: uuid.into(),
        }
    }

    fn locator(&self) -> String {
        format!("{}/{}", self.session, self.uuid)
    }
}

#[derive(Debug, Default)]
struct PreScan {
    sources: HashMap<SourceKey, SourceObservation>,
    aliases: BTreeSet<(SourceKey, SourceKey)>,
    digest: [u8; 32],
    /// The file is known finished: what it holds back at its tail is emitted.
    finished: bool,
    /// Line numbers whose record is a further version of its uuid, with the
    /// key that version is planned under.
    versions: HashMap<usize, SourceKey>,
    /// The last line holding each key's content.
    last_lines: HashMap<SourceKey, usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SourceObservation {
    /// The record has content to project.
    content: bool,
    turn_duration: bool,
    non_turn_duration: bool,
    parents: BTreeSet<SourceKey>,
    /// Where a fork's replay placed its copy; used only when the copy turns
    /// out to say something else than its origin.
    replay_parents: BTreeSet<SourceKey>,
    shapes: Vec<RecordShape>,
    /// How the record takes part in the agent's perceive-act order.
    flow: Option<Flow>,
}

/// How one record takes part in the perceive-act order.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Flow {
    /// A record of one assistant message, keyed by the provider's message id
    /// (or its own locator when it has none). The id is never stored.
    Act(String),
    /// What the harness logged inside a message: tool results and harness
    /// context. The agent perceived it only after the message.
    Inside,
    /// A user prompt that is not a tool result: the message before it is
    /// complete.
    Prompt,
}

/// What one record contributes, without time: time is annotation, so two
/// copies of a record that differ only in time agree.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordShape {
    parts: Vec<PartShape>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PartShape {
    fact: Id,
    tool_call_id: Option<String>,
    responds_to: Option<ToolReference>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolReference {
    source: Option<SourceKey>,
    session: String,
    correlator: String,
}

/// Canonical source graph and node plan, built from every record before any
/// fragment is emitted.
///
/// Explicit `forkedFrom` edges form an identity quotient. Parent observations
/// from non-fork receipts union between quotient classes. Contentless source
/// records are contracted; only reciprocal backlinks on Claude's proven
/// message-less `system/turn_duration` telemetry are removed. Every class with
/// content is a chain of nodes, one per fact, in the main chain.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SourcePlan {
    canonical: HashMap<SourceKey, SourceKey>,
    /// Nearest classes with content above each class, as observed; a class
    /// absent from the corpus stays here so it can be counted unresolved.
    parents: HashMap<SourceKey, BTreeSet<SourceKey>>,
    /// The nodes of each class with content, in order.
    nodes: HashMap<SourceKey, Vec<Id>>,
    /// What the first node of each class follows.
    firsts: HashMap<SourceKey, BTreeSet<Id>>,
    /// The call node each fact of a class answers, if any.
    responds: HashMap<SourceKey, Vec<Option<Id>>>,
    /// Tool results of each class whose call the corpus does not hold.
    unresolved_results: HashMap<SourceKey, usize>,
    /// Nodes some other node follows.
    followed: HashSet<Id>,
    /// Classes held back at the tail, inside a message not yet complete.
    held: HashSet<SourceKey>,
}

/// Where a branch of the history stands while classes are placed.
#[derive(Clone, Debug, Default)]
struct Placement {
    /// What the next placed node follows.
    tail: BTreeSet<Id>,
    /// The message still open, if any.
    message: Option<String>,
    /// What was logged inside the open message, in source order, not yet
    /// placed.
    deferred: Vec<SourceKey>,
}

/// Assigns node chains to classes.
struct Placer<'a> {
    observed: &'a HashMap<SourceKey, SourceObservation>,
    nodes: HashMap<SourceKey, Vec<Id>>,
    firsts: HashMap<SourceKey, BTreeSet<Id>>,
}

impl Placer<'_> {
    /// Chain one class's nodes after `previous`; returns its last node.
    fn place(&mut self, key: &SourceKey, previous: BTreeSet<Id>) -> Result<BTreeSet<Id>> {
        let shape = self.observed[key]
            .shapes
            .first()
            .expect("a class with content has a shape");
        let mut tail = previous.clone();
        let mut chain = Vec::with_capacity(shape.parts.len());
        for part in &shape.parts {
            let node = node_id(&tail, part.fact)?;
            chain.push(node);
            tail = BTreeSet::from([node]);
        }
        self.nodes.insert(key.clone(), chain);
        self.firsts.insert(key.clone(), previous);
        Ok(tail)
    }

    /// Complete the open message: what it deferred follows it, in source
    /// order. Returns the new tail.
    fn close(&mut self, state: Placement) -> Result<BTreeSet<Id>> {
        let mut tail = state.tail;
        for key in &state.deferred {
            tail = self.place(key, tail)?;
        }
        Ok(tail)
    }
}

impl SourcePlan {
    fn from_scans(scans: &[PreScan]) -> Result<Self> {
        Self::build(scans)
    }

    fn build(scans: &[PreScan]) -> Result<Self> {
        let mut raw: HashMap<SourceKey, SourceObservation> = HashMap::new();
        let mut keys = BTreeSet::new();
        let mut aliases = BTreeSet::new();
        let mut key_scans: HashMap<SourceKey, BTreeSet<usize>> = HashMap::new();
        for (index, scan) in scans.iter().enumerate() {
            aliases.extend(scan.aliases.iter().cloned());
            for (key, incoming) in &scan.sources {
                if incoming.content {
                    key_scans.entry(key.clone()).or_default().insert(index);
                }
                keys.insert(key.clone());
                keys.extend(incoming.parents.iter().cloned());
                let observation = raw.entry(key.clone()).or_default();
                observation.content |= incoming.content;
                observation.turn_duration |= incoming.turn_duration;
                observation.non_turn_duration |= incoming.non_turn_duration;
                observation.flow = merge_flow(observation.flow.take(), incoming.flow.clone());
                observation.parents.extend(incoming.parents.iter().cloned());
                observation
                    .replay_parents
                    .extend(incoming.replay_parents.iter().cloned());
                for shape in &incoming.shapes {
                    if !observation.shapes.contains(shape) {
                        observation.shapes.push(shape.clone());
                    }
                }
            }
        }
        // A fork copy is its origin only where it says the same thing. One
        // that says something else is a record of its own, in the position
        // its replay gave it.
        let aliases = equal_aliases(&raw, aliases);
        let aliased: HashSet<&SourceKey> =
            aliases.iter().map(|(destination, _)| destination).collect();
        let replayed: Vec<(SourceKey, BTreeSet<SourceKey>)> = raw
            .iter()
            .filter(|(key, observation)| {
                !observation.replay_parents.is_empty() && !aliased.contains(key)
            })
            .map(|(key, observation)| (key.clone(), observation.replay_parents.clone()))
            .collect();
        for (key, parents) in replayed {
            keys.extend(parents.iter().cloned());
            raw.get_mut(&key)
                .expect("a replayed key is observed")
                .parents
                .extend(parents);
        }
        for (destination, origin) in &aliases {
            keys.insert(destination.clone());
            keys.insert(origin.clone());
        }

        let canonical = alias_quotient(&keys, &aliases);
        let canonical_key =
            |key: &SourceKey| canonical.get(key).cloned().unwrap_or_else(|| key.clone());
        let mut observed: HashMap<SourceKey, SourceObservation> = HashMap::new();
        for (key, incoming) in &raw {
            let class = canonical_key(key);
            let observation = observed.entry(class).or_default();
            observation.content |= incoming.content;
            observation.turn_duration |= incoming.turn_duration;
            observation.non_turn_duration |= incoming.non_turn_duration;
            observation.flow = merge_flow(observation.flow.take(), incoming.flow.clone());
            observation
                .parents
                .extend(incoming.parents.iter().map(&canonical_key));
            for shape in &incoming.shapes {
                if !observation.shapes.contains(shape) {
                    observation.shapes.push(shape.clone());
                }
            }
        }

        for (key, observation) in &observed {
            if observation.content && observation.turn_duration {
                bail!(
                    "Claude Code source key {} is both content and turn-duration telemetry",
                    key.locator()
                );
            }
        }

        let semantic: HashSet<_> = observed
            .iter()
            .filter_map(|(key, observation)| observation.content.then_some(key.clone()))
            .collect();
        let mut normalized: HashMap<_, _> = observed
            .iter()
            .map(|(key, observation)| (key.clone(), observation.parents.clone()))
            .collect();

        // Claude Code emits a message-less `system/turn_duration` record T
        // after an assistant A in a small number of transcripts while also
        // recording A as T's parent and T as one observation of A's parent.
        // That is telemetry adjacency, not a semantic A <-> T cycle. A census
        // of 3,391 files / 1.3M records found exactly 19 reciprocal cases and
        // no counterexample to removing only T's direct reciprocal backlink.
        // Requiring every
        // occurrence of T to carry the exact telemetry subtype keeps this
        // normalization deliberately narrower than generic cycle breaking.
        for (telemetry, observation) in &observed {
            if !observation.turn_duration || observation.non_turn_duration {
                continue;
            }
            let reciprocal = observation
                .parents
                .iter()
                .filter(|parent| {
                    observed
                        .get(*parent)
                        .is_some_and(|candidate| candidate.parents.contains(telemetry))
                })
                .cloned()
                .collect::<Vec<_>>();
            let parents = normalized
                .get_mut(telemetry)
                .expect("every observed source has normalized parents");
            for parent in reciprocal {
                parents.remove(&parent);
            }
        }

        let mut parents = HashMap::with_capacity(semantic.len());
        let mut memo = HashMap::new();
        for key in sorted_source_keys(&semantic) {
            let mut frontier = BTreeSet::new();
            for parent in normalized.get(&key).into_iter().flatten() {
                frontier.extend(semantic_frontier(
                    parent,
                    &semantic,
                    &normalized,
                    &mut memo,
                )?);
            }
            parents.insert(key, frontier);
        }

        // Full-corpus evidence for this quotient (2026-08-09): 126,058
        // explicit aliases collapse into 796,984 classes; unioning every
        // non-fork parent assertion leaves 793,454 class edges, 199 genuine
        // multi-parent classes, and no semantic SCC. Fourteen native records
        // refer to keys defined only by fork receipts, so alias resolution is
        // not merely cosmetic.
        validate_semantic_dag(&semantic, &parents)?;

        // Each class is a chain of nodes, one per fact, placed in the order
        // the agent experienced it. Claude Code runs a call as soon as the
        // call is complete and logs its result before the message's next
        // call, but the model emitted every call of a message before it
        // perceived any result: what the harness logged inside a message
        // (`Flow::Inside`) is deferred until the message is complete, which
        // the transcript shows by a record of another message or a user
        // prompt. A message still open where a branch ends may yet grow, so
        // what was deferred inside it is held back and lands on a later
        // import: growth only ever appends nodes.
        let order = topological_order(&semantic, &parents)?;
        let mut class_scans: HashMap<SourceKey, BTreeSet<usize>> = HashMap::new();
        for (key, indices) in &key_scans {
            class_scans
                .entry(canonical_key(key))
                .or_default()
                .extend(indices.iter().copied());
        }
        let mut children: HashMap<&SourceKey, usize> = HashMap::new();
        for key in &order {
            for parent in &parents[key] {
                if semantic.contains(parent) {
                    *children.entry(parent).or_default() += 1;
                }
            }
        }
        // The branch each unfinished file wrote last: of its branch ends, the
        // one whose content it wrote most recently. Every other branch end of
        // that file lies before the file's last record; the file has moved
        // on from it (a compaction, a rewind), so its open message is done.
        let mut trailing: HashSet<SourceKey> = HashSet::new();
        for scan in scans.iter().filter(|scan| !scan.finished) {
            let last = scan
                .last_lines
                .iter()
                .map(|(key, line)| (canonical_key(key), *line))
                .filter(|(class, _)| semantic.contains(class) && !children.contains_key(class))
                .max_by_key(|(_, line)| *line);
            trailing.extend(last.map(|(class, _)| class));
        }
        let mut placer = Placer {
            observed: &observed,
            nodes: HashMap::with_capacity(order.len()),
            firsts: HashMap::with_capacity(order.len()),
        };
        let mut states: HashMap<&SourceKey, (Placement, usize)> = HashMap::new();
        let mut held = HashSet::new();
        for key in &order {
            let incoming: Vec<&SourceKey> = parents[key]
                .iter()
                .filter(|parent| semantic.contains(*parent))
                .collect();
            let mut taken = Vec::with_capacity(incoming.len());
            for parent in &incoming {
                let (state, remaining) = states
                    .get_mut(*parent)
                    .expect("a parent is placed before its children");
                *remaining -= 1;
                taken.push(if *remaining == 0 {
                    states.remove(*parent).expect("present").0
                } else {
                    state.clone()
                });
            }
            let mut state = match taken.len() {
                0 => Placement::default(),
                1 => taken.pop().expect("one state"),
                _ => {
                    let mut tail = BTreeSet::new();
                    for state in taken {
                        tail.extend(placer.close(state)?);
                    }
                    Placement {
                        tail,
                        ..Placement::default()
                    }
                }
            };
            match observed[key]
                .flow
                .clone()
                .expect("a class with content has a flow")
            {
                Flow::Act(message) => {
                    if state.message.as_ref() != Some(&message) {
                        state.tail = placer.close(std::mem::take(&mut state))?;
                        state.message = Some(message);
                    }
                    state.tail = placer.place(key, std::mem::take(&mut state.tail))?;
                }
                Flow::Inside if state.message.is_some() => state.deferred.push(key.clone()),
                Flow::Inside => {
                    state.tail = placer.place(key, std::mem::take(&mut state.tail))?;
                }
                Flow::Prompt => {
                    let tail = placer.close(std::mem::take(&mut state))?;
                    state.tail = placer.place(key, tail)?;
                }
            }
            match children.get(key).copied().unwrap_or(0) {
                // The branch ends here: what the open message deferred waits
                // for the transcript to show the message complete, unless
                // every file holding it is known finished or no file wrote
                // this branch last.
                0 => {
                    let finished = !trailing.contains(key)
                        || std::iter::once(key).chain(&state.deferred).all(|class| {
                            class_scans.get(class).is_some_and(|indices| {
                                !indices.is_empty()
                                    && indices.iter().all(|&index| scans[index].finished)
                            })
                        });
                    if finished {
                        placer.close(state)?;
                    } else {
                        held.extend(state.deferred);
                    }
                }
                1 => {
                    states.insert(key, (state, 1));
                }
                // Branches would each place the deferred records anew, so
                // the message closes where the history forks.
                several => {
                    let tail = placer.close(state)?;
                    states.insert(
                        key,
                        (
                            Placement {
                                tail,
                                ..Placement::default()
                            },
                            several,
                        ),
                    );
                }
            }
        }
        let Placer { nodes, firsts, .. } = placer;
        // A node is followed exactly when some node names it as previous.
        let mut followed = HashSet::new();
        for (key, chain) in &nodes {
            followed.extend(firsts[key].iter().copied());
            followed.extend(chain[..chain.len() - 1].iter().copied());
        }

        // Copies of one record (an original and its fork's replay) must say
        // the same thing; time is not part of what they say.
        for (key, observation) in &raw {
            let class = canonical_key(key);
            let Some(chain) = nodes.get(&class) else {
                continue;
            };
            let expected = &observed[&class].shapes[0];
            for shape in &observation.shapes {
                let facts: Vec<Id> = shape.parts.iter().map(|part| part.fact).collect();
                let planned: Vec<Id> = expected.parts.iter().map(|part| part.fact).collect();
                if facts != planned || chain.len() != facts.len() {
                    bail!(
                        "Claude Code alias class {} carries conflicting semantic payloads",
                        class.locator()
                    );
                }
            }
        }

        let mut tool_nodes = HashMap::new();
        let mut session_tool_nodes = HashMap::new();
        for (key, observation) in &raw {
            let class = canonical_key(key);
            let Some(chain) = nodes.get(&class) else {
                continue;
            };
            for shape in &observation.shapes {
                for (position, part) in shape.parts.iter().enumerate() {
                    let Some(correlator) = &part.tool_call_id else {
                        continue;
                    };
                    let node = chain[position];
                    if tool_nodes
                        .insert((class.clone(), correlator.clone()), node)
                        .is_some_and(|previous| previous != node)
                    {
                        bail!(
                            "Claude Code tool correlator {:?} has conflicting calls in alias class {}",
                            correlator,
                            class.locator()
                        );
                    }
                    if session_tool_nodes
                        .insert((key.session.clone(), correlator.clone()), node)
                        .is_some_and(|previous| previous != node)
                    {
                        bail!(
                            "Claude Code tool correlator {:?} has conflicting calls in raw session {:?}",
                            correlator,
                            key.session
                        );
                    }
                }
            }
        }
        let resolve = |reference: &ToolReference| -> Option<Id> {
            match &reference.source {
                Some(source) => tool_nodes
                    .get(&(canonical_key(source), reference.correlator.clone()))
                    .copied(),
                None => session_tool_nodes
                    .get(&(reference.session.clone(), reference.correlator.clone()))
                    .copied(),
            }
        };
        let mut responds = HashMap::with_capacity(order.len());
        let mut unresolved_results = HashMap::with_capacity(order.len());
        for key in &order {
            let shape = &observed[key].shapes[0];
            let mut targets = Vec::with_capacity(shape.parts.len());
            let mut unresolved = 0usize;
            for part in &shape.parts {
                targets.push(part.responds_to.as_ref().and_then(|reference| {
                    let target = resolve(reference);
                    unresolved += usize::from(target.is_none());
                    target
                }));
            }
            responds.insert(key.clone(), targets);
            unresolved_results.insert(key.clone(), unresolved);
        }

        Ok(Self {
            canonical,
            parents,
            nodes,
            firsts,
            responds,
            unresolved_results,
            followed,
            held,
        })
    }

    fn canonical_key(&self, key: &SourceKey) -> SourceKey {
        self.canonical
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.clone())
    }
}

/// The alias edges that join only copies saying the same thing.
///
/// Edges are taken in their sorted order; an edge is kept unless it would put
/// two different semantic payloads into one class. A key without content
/// joins any class.
fn equal_aliases(
    raw: &HashMap<SourceKey, SourceObservation>,
    aliases: BTreeSet<(SourceKey, SourceKey)>,
) -> BTreeSet<(SourceKey, SourceKey)> {
    let facts_of = |key: &SourceKey| -> Option<Vec<Id>> {
        raw.get(key)
            .and_then(|observation| observation.shapes.first())
            .map(|shape| shape.parts.iter().map(|part| part.fact).collect())
    };
    let mut roots: HashMap<SourceKey, SourceKey> = HashMap::new();
    let mut payloads: HashMap<SourceKey, Option<Vec<Id>>> = HashMap::new();
    fn find(roots: &mut HashMap<SourceKey, SourceKey>, key: &SourceKey) -> SourceKey {
        let mut root = key.clone();
        while let Some(parent) = roots.get(&root) {
            if *parent == root {
                break;
            }
            root = parent.clone();
        }
        roots.insert(key.clone(), root.clone());
        root
    }
    let mut kept = BTreeSet::new();
    for (destination, origin) in aliases {
        for key in [&destination, &origin] {
            if !roots.contains_key(key) {
                roots.insert(key.clone(), key.clone());
                payloads.insert(key.clone(), facts_of(key));
            }
        }
        let (left, right) = (find(&mut roots, &destination), find(&mut roots, &origin));
        if left == right {
            kept.insert((destination, origin));
            continue;
        }
        let joined = match (&payloads[&left], &payloads[&right]) {
            (Some(left), Some(right)) if left != right => continue,
            (Some(payload), _) | (_, Some(payload)) => Some(payload.clone()),
            (None, None) => None,
        };
        roots.insert(right.clone(), left.clone());
        payloads.insert(left, joined);
        kept.insert((destination, origin));
    }
    kept
}

/// Classes in a deterministic topological order.
fn topological_order(
    semantic: &HashSet<SourceKey>,
    parents: &HashMap<SourceKey, BTreeSet<SourceKey>>,
) -> Result<Vec<SourceKey>> {
    let mut dependencies = HashMap::new();
    let mut children: HashMap<SourceKey, BTreeSet<SourceKey>> = HashMap::new();
    let mut ready = BTreeSet::new();
    for key in semantic {
        let known: Vec<_> = parents
            .get(key)
            .into_iter()
            .flatten()
            .filter(|parent| semantic.contains(*parent))
            .collect();
        dependencies.insert(key.clone(), known.len());
        if known.is_empty() {
            ready.insert(key.clone());
        }
        for parent in known {
            children
                .entry(parent.clone())
                .or_default()
                .insert(key.clone());
        }
    }
    let mut order = Vec::with_capacity(semantic.len());
    while let Some(key) = ready.pop_first() {
        for child in children.get(&key).into_iter().flatten() {
            let remaining = dependencies
                .get_mut(child)
                .expect("every semantic child has a dependency count");
            *remaining -= 1;
            if *remaining == 0 {
                ready.insert(child.clone());
            }
        }
        order.push(key);
    }
    if order.len() != semantic.len() {
        bail!("semantic Claude Code source cycle remains after normalization");
    }
    Ok(order)
}

fn alias_quotient(
    keys: &BTreeSet<SourceKey>,
    aliases: &BTreeSet<(SourceKey, SourceKey)>,
) -> HashMap<SourceKey, SourceKey> {
    let ordered = keys.iter().cloned().collect::<Vec<_>>();
    let indices = ordered
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    let mut disjoint = DisjointSet::new(ordered.len());
    for (destination, origin) in aliases {
        disjoint.union(indices[destination], indices[origin]);
    }
    let mut representatives = HashMap::<usize, SourceKey>::new();
    for (index, key) in ordered.iter().enumerate() {
        let root = disjoint.find(index);
        representatives
            .entry(root)
            .and_modify(|current| {
                if key < current {
                    *current = key.clone();
                }
            })
            .or_insert_with(|| key.clone());
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, key)| {
            let representative = representatives[&disjoint.find(index)].clone();
            (key, representative)
        })
        .collect()
}

struct DisjointSet {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl DisjointSet {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            rank: vec![0; len],
        }
    }

    fn find(&mut self, index: usize) -> usize {
        if self.parent[index] != index {
            self.parent[index] = self.find(self.parent[index]);
        }
        self.parent[index]
    }

    fn union(&mut self, left: usize, right: usize) {
        let left = self.find(left);
        let right = self.find(right);
        if left == right {
            return;
        }
        match self.rank[left].cmp(&self.rank[right]) {
            std::cmp::Ordering::Less => self.parent[left] = right,
            std::cmp::Ordering::Greater => self.parent[right] = left,
            std::cmp::Ordering::Equal => {
                self.parent[right] = left;
                self.rank[left] += 1;
            }
        }
    }
}

/// The key of a further version of a provider uuid: derived from what the
/// version says, so the same version gets the same key in every file.
fn content_version(key: &SourceKey, facts: &[Id]) -> SourceKey {
    let mut hasher = blake3::Hasher::new();
    for fact in facts {
        hasher.update(&fact.raw());
    }
    let digest = hasher.finalize();
    SourceKey::new(
        key.session.clone(),
        format!("{}#{}", key.uuid, &digest.to_hex()[..16]),
    )
}

/// Copies of one record agree on their flow; where they would not, the
/// least wins, whatever the order they were read in.
fn merge_flow(left: Option<Flow>, right: Option<Flow>) -> Option<Flow> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

fn node_id(previous: &BTreeSet<Id>, fact: Id) -> Result<Id> {
    blockdag::block(
        previous.iter().copied(),
        None,
        Fragment::new([fact], TribleSet::new()),
    )?
    .root()
    .ok_or_else(|| anyhow!("canonical node constructor returned no root"))
}

fn sorted_source_keys(keys: &HashSet<SourceKey>) -> Vec<SourceKey> {
    let mut keys = keys.iter().cloned().collect::<Vec<_>>();
    keys.sort();
    keys
}

fn semantic_frontier(
    key: &SourceKey,
    semantic: &HashSet<SourceKey>,
    normalized: &HashMap<SourceKey, BTreeSet<SourceKey>>,
    memo: &mut HashMap<SourceKey, BTreeSet<SourceKey>>,
) -> Result<BTreeSet<SourceKey>> {
    if semantic.contains(key) {
        return Ok(BTreeSet::from([key.clone()]));
    }
    if let Some(frontier) = memo.get(key) {
        return Ok(frontier.clone());
    }
    if !normalized.contains_key(key) {
        // A referenced source absent from this corpus remains explicit so the
        // projection pass can account it as unresolved rather than silently
        // turning a non-root into a root.
        return Ok(BTreeSet::from([key.clone()]));
    }

    // Source histories can be hundreds of thousands of records deep. Keep
    // that depth on the heap rather than coupling it to the process stack.
    let mut active = HashSet::new();
    let mut stack = vec![(key.clone(), false)];
    while let Some((current, exiting)) = stack.pop() {
        if semantic.contains(&current) || !normalized.contains_key(&current) {
            continue;
        }
        if memo.contains_key(&current) {
            continue;
        }
        if exiting {
            let mut frontier = BTreeSet::new();
            for parent in normalized
                .get(&current)
                .expect("known transparent source retains its parent set")
            {
                if semantic.contains(parent) || !normalized.contains_key(parent) {
                    frontier.insert(parent.clone());
                } else {
                    frontier.extend(
                        memo.get(parent)
                            .expect("transparent predecessor exits before its child")
                            .iter()
                            .cloned(),
                    );
                }
            }
            active.remove(&current);
            memo.insert(current, frontier);
            continue;
        }
        if !active.insert(current.clone()) {
            bail!(
                "transparent Claude Code source cycle remains after telemetry normalization at {}",
                current.locator()
            );
        }
        stack.push((current.clone(), true));
        for parent in normalized
            .get(&current)
            .expect("known transparent source retains its parent set")
            .iter()
            .rev()
        {
            if active.contains(parent) {
                bail!(
                    "transparent Claude Code source cycle remains after telemetry normalization at {} -> {}",
                    current.locator(),
                    parent.locator()
                );
            }
            if !semantic.contains(parent)
                && normalized.contains_key(parent)
                && !memo.contains_key(parent)
            {
                stack.push((parent.clone(), false));
            }
        }
    }
    Ok(memo
        .get(key)
        .expect("requested transparent source receives a memoized frontier")
        .clone())
}

fn validate_semantic_dag(
    semantic: &HashSet<SourceKey>,
    parents: &HashMap<SourceKey, BTreeSet<SourceKey>>,
) -> Result<()> {
    let mut visited = HashSet::new();
    for key in sorted_source_keys(semantic) {
        if visited.contains(&key) {
            continue;
        }
        let mut active = HashSet::new();
        let mut stack = vec![(key, false)];
        while let Some((current, exiting)) = stack.pop() {
            if visited.contains(&current) {
                continue;
            }
            if exiting {
                active.remove(&current);
                visited.insert(current);
                continue;
            }
            if !active.insert(current.clone()) {
                bail!(
                    "semantic Claude Code source cycle remains after normalization at {}",
                    current.locator()
                );
            }
            stack.push((current.clone(), true));
            for parent in parents.get(&current).into_iter().flatten().rev() {
                if !semantic.contains(parent) || visited.contains(parent) {
                    continue;
                }
                if active.contains(parent) {
                    bail!(
                        "semantic Claude Code source cycle remains after normalization at {} -> {}",
                        current.locator(),
                        parent.locator()
                    );
                }
                stack.push((parent.clone(), false));
            }
        }
    }
    Ok(())
}

fn project_directory<F>(
    root: &Path,
    finished_before: Option<SystemTime>,
    emit: &mut F,
) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    let mut paths = Vec::new();
    collect_jsonl_files(root, &mut paths)
        .with_context(|| format!("scan Claude Code source {}", root.display()))?;
    paths.sort();

    let scanned: Vec<Result<PreScan>> = paths
        .par_iter()
        .map(|path| prescan_file(path, finished_before))
        .collect();
    let mut prescans = Vec::with_capacity(scanned.len());
    for (path, scan) in paths.iter().zip(scanned) {
        prescans.push(scan.with_context(|| format!("pre-scan {}", path.display()))?);
    }
    let plan = SourcePlan::from_scans(&prescans).context("plan canonical Claude Code lineage")?;
    let mut summary = ProjectionSummary {
        files_scanned: paths.len(),
        ..ProjectionSummary::default()
    };

    // The complete record DAG has already fixed every intrinsic id. Freeze and
    // project one physical file at a time: pre-scan may exploit parallelism,
    // but projection never retains several multi-gigabyte snapshots or
    // per-file fragment unions concurrently.
    for (path, prescan) in paths.iter().zip(&prescans) {
        let snapshot = archive_source::freeze_file(path)?;
        let meta = subagent_meta(path)?;
        let projected = project_snapshot(path, snapshot, prescan, &plan, meta, emit)
            .with_context(|| format!("project Claude Code file {}", path.display()))?;
        summary.fragments_emitted += projected.fragments_emitted;
        summary.stats.absorb(projected.stats);
    }

    let history = root.join(HISTORY_FILE);
    if history.is_file() {
        let projected = project_history(&history, emit)?;
        summary.files_scanned += 1;
        summary.fragments_emitted += projected.fragments_emitted;
        summary.stats.absorb(projected.stats);
    }

    Ok(summary)
}

fn collect_jsonl_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("read directory {}", path.display()))? {
        let entry = entry.context("read directory entry")?;
        let entry_path = entry.path();
        let file_type = entry.file_type().context("read directory-entry type")?;
        if file_type.is_dir() {
            collect_jsonl_files(&entry_path, out)?;
        } else if file_type.is_file()
            && entry_path
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("jsonl")
            && entry_path.file_name().and_then(|name| name.to_str()) != Some(HISTORY_FILE)
        {
            out.push(entry_path);
        }
    }
    Ok(())
}

fn prescan_file(path: &Path, finished_before: Option<SystemTime>) -> Result<PreScan> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    // Read before the records, so a write after it can only make the file
    // look unfinished; the frozen digest then still guards its bytes.
    let finished = match finished_before {
        Some(cutoff) => {
            file.metadata()
                .and_then(|metadata| metadata.modified())
                .with_context(|| format!("read the modification time of {}", path.display()))?
                < cutoff
        }
        None => false,
    };
    let mut reader = BufReader::new(file);
    let mut scan = prescan_reader(&mut reader, path)?;
    scan.finished = finished;
    Ok(scan)
}

fn prescan_reader<R: BufRead>(reader: &mut R, source_path: &Path) -> Result<PreScan> {
    let mut scan = PreScan::default();
    // Each uuid's versions seen so far, and the one written most recently.
    let mut versions: HashMap<SourceKey, Vec<(Vec<Id>, SourceKey)>> = HashMap::new();
    let mut current: HashMap<SourceKey, SourceKey> = HashMap::new();
    let mut digest = blake3::Hasher::new();
    for_each_jsonl_line(reader, Some(&mut digest), |line_number, raw| {
        let mut bytes = scanner_bytes(raw)?;
        let record = scan_record(&mut bytes).map_err(|error| {
            anyhow!(
                "pre-scan Claude Code record at {}:{}: {error}",
                source_path.display(),
                line_number
            )
        })?;
        let Some(raw_key) = record.source_key() else {
            return Ok(());
        };
        let shape = record_parts(&record, raw, &raw_key, &mut ProjectionStats::default())?
            .map(|draft| draft.shape());
        // A provider uuid is parse-only. Copies under one uuid that say the
        // same thing are one record; a copy that says something else (a
        // fork replayed by an older Claude Code, an attachment rendered
        // anew after an upgrade) is a record of its own, under a key derived
        // from what it says. References resolve to the version of a uuid
        // written most recently before them in this file.
        let key = match &shape {
            Some(shape) => {
                let facts: Vec<Id> = shape.parts.iter().map(|part| part.fact).collect();
                let seen = versions.entry(raw_key.clone()).or_default();
                match seen.iter().find(|(known, _)| *known == facts) {
                    Some((_, key)) => key.clone(),
                    None => {
                        let key = if seen.is_empty() {
                            raw_key.clone()
                        } else {
                            content_version(&raw_key, &facts)
                        };
                        seen.push((facts, key.clone()));
                        key
                    }
                }
            }
            None => current
                .get(&raw_key)
                .cloned()
                .unwrap_or_else(|| raw_key.clone()),
        };
        let resolve = |reference: SourceKey| current.get(&reference).cloned().unwrap_or(reference);
        let raw_parent = record
            .parent_uuid
            .as_ref()
            .map(|parent| resolve(SourceKey::new(raw_key.session.clone(), parent.clone())));
        let fork_origin = record.fork_origin_key()?.map(resolve);
        current.insert(raw_key.clone(), key.clone());
        if key != raw_key {
            scan.versions.insert(line_number, key.clone());
        }
        let turn_duration =
            record.record_type == "system" && record.subtype.as_deref() == Some("turn_duration");
        // A compaction boundary starts a new physical chain. Its
        // `logicalParentUuid` is not used as an edge: in live files it can
        // name a record that itself descends from the boundary (observed in
        // f03d61a4, 2026-10-11), which would close a cycle.
        if let Some(origin) = &fork_origin {
            scan.aliases.insert((key.clone(), origin.clone()));
        }
        // `forkedFrom` is an explicit identity alias to the named origin. Its
        // destination `parentUuid` describes replay serialization (and can
        // reverse tool chronology), so it is not a canonical semantic
        // ancestry assertion.
        let parent = fork_origin
            .is_none()
            .then_some(raw_parent.clone())
            .flatten();
        let observation = scan.sources.entry(key.clone()).or_default();
        observation.content |= shape.is_some();
        observation.turn_duration |= turn_duration;
        observation.non_turn_duration |= !turn_duration;
        if observation.content && observation.turn_duration {
            bail!(
                "Claude Code source key {}/{} is both content and turn-duration telemetry",
                key.session,
                key.uuid
            );
        }
        if let Some(parent) = parent {
            observation.parents.insert(parent);
        }
        if fork_origin.is_some() {
            // Where the replay put it: its position if it turns out not to
            // be a copy of its origin.
            observation.replay_parents.extend(raw_parent);
        }
        if let Some(shape) = shape {
            scan.last_lines.insert(key.clone(), line_number);
            let flow = match record.record_type.as_str() {
                "assistant" => Flow::Act(
                    record
                        .message_id
                        .clone()
                        .unwrap_or_else(|| format!("record/{}", key.locator())),
                ),
                "user" if !shape.parts.iter().any(|part| part.responds_to.is_some()) => {
                    Flow::Prompt
                }
                _ => Flow::Inside,
            };
            observation.flow = merge_flow(observation.flow.take(), Some(flow));
            if !observation.shapes.contains(&shape) {
                observation.shapes.push(shape);
            }
        }
        Ok(())
    })?;
    scan.digest = *digest.finalize().as_bytes();
    Ok(scan)
}

fn project_snapshot<F>(
    source_path: &Path,
    snapshot: archive_source::FrozenSource,
    prescan: &PreScan,
    plan: &SourcePlan,
    meta: Option<SubagentMeta>,
    emit: &mut F,
) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    if snapshot.digest != prescan.digest {
        bail!(
            "Claude Code source {} changed between dependency pre-scan and immutable snapshot",
            source_path.display()
        );
    }

    let mut summary = ProjectionSummary {
        files_scanned: 1,
        ..ProjectionSummary::default()
    };
    let mut reader = std::io::Cursor::new(snapshot.bytes.as_ref());
    // Receipts of the nodes this file projects, for the leaf annotation.
    let mut receipts = BTreeMap::<Id, Id>::new();
    let mut session = None::<String>;

    for_each_jsonl_line(&mut reader, None, |line_number, raw| {
        let mut row_stats = ProjectionStats {
            records_seen: 1,
            ..ProjectionStats::default()
        };
        let mut bytes = scanner_bytes(raw)?;
        let record = scan_record(&mut bytes).map_err(|error| {
            anyhow!(
                "scan Claude Code record at {}:{}: {error}",
                source_path.display(),
                line_number
            )
        })?;
        if session.is_none() {
            session = record.session_id.clone();
        }
        let version = prescan.versions.get(&line_number);
        let fragments = project_record(record, version, raw, plan, &mut receipts, &mut row_stats)
            .with_context(|| {
            format!(
                "project Claude Code record at {}:{}",
                source_path.display(),
                line_number
            )
        })?;
        summary.stats.absorb(row_stats);
        let mut stats = Some(row_stats);
        for (fragment, identity) in fragments {
            emit(ProjectedFile {
                source_path: source_path.to_path_buf(),
                fragment,
                identity,
                stats: stats.take().unwrap_or_default(),
            })?;
            summary.fragments_emitted += 1;
        }
        Ok(())
    })?;

    // The session id and a subagent's spawn metadata annotate the leaves the
    // file projects, never every node. Titles and the file's path are not
    // kept: they describe the machinery around the session, not the session.
    let detail = meta.map(|meta| meta.detail);
    for (node, receipt) in receipts {
        if plan.followed.contains(&node) {
            continue;
        }
        let fragment = blockdag::source_occurrence(
            receipt,
            blockdag::ProjectionAnnotations {
                source_locator: session.as_ref().map(|id| format!("session/{id}")),
                source_detail: detail.clone(),
                ..blockdag::ProjectionAnnotations::default()
            },
        )?;
        emit(ProjectedFile {
            source_path: source_path.to_path_buf(),
            fragment,
            identity: blockdag::Identity::none(),
            stats: ProjectionStats::default(),
        })?;
        summary.fragments_emitted += 1;
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// history.jsonl: the prompt log
// ---------------------------------------------------------------------------

/// One prompt of the log, with pasted text expanded.
struct HistoryPrompt {
    text: String,
    display: String,
    timestamp: Option<Inline<NsTAIInterval>>,
}

/// Project the prompts of `history.jsonl` that no transcript beside it holds.
///
/// A prompt a transcript holds (as a user message, a queued prompt, or a
/// slash command) is that transcript's block; it projects nothing here, so it
/// cannot land anywhere else. The others (sessions whose transcripts are gone,
/// prompts a transcript never recorded) become user TEXT blocks of their own.
/// The log keeps no acts, so consecutive prompts are not one perception and
/// no prompt is claimed to follow another directly: each is a root, and the
/// session id, the log's path and the prompt's time place it.
fn project_history<F>(path: &Path, emit: &mut F) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    let root = path.parent().unwrap_or_else(|| Path::new("."));
    let bytes = archive_source::read_file(path)?;
    let mut sessions: std::collections::BTreeMap<String, Vec<HistoryPrompt>> = Default::default();
    let mut order = Vec::<String>::new();
    let mut stats = ProjectionStats::default();
    let text = bytes.as_ref();
    let complete = text
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |end| end + 1);
    for line in text[..complete].split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        stats.records_seen += 1;
        let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) else {
            stats.skipped_records += 1;
            continue;
        };
        let (Some(session), Some(display)) = (
            entry.get("sessionId").and_then(serde_json::Value::as_str),
            entry.get("display").and_then(serde_json::Value::as_str),
        ) else {
            stats.missing_source_identity += 1;
            stats.skipped_records += 1;
            continue;
        };
        stats.history_prompts += 1;
        let text = expand_pasted(root, display, entry.get("pastedContents"));
        let timestamp = entry
            .get("timestamp")
            .and_then(serde_json::Value::as_f64)
            .map(Epoch::from_unix_milliseconds)
            .and_then(epoch_interval);
        if !sessions.contains_key(session) {
            order.push(session.to_owned());
        }
        sessions
            .entry(session.to_owned())
            .or_default()
            .push(HistoryPrompt {
                text,
                display: display.to_owned(),
                timestamp,
            });
    }

    let mut summary = ProjectionSummary {
        files_scanned: 1,
        ..ProjectionSummary::default()
    };
    for session in order {
        let prompts = sessions.remove(&session).unwrap_or_default();
        let known = transcript_prompts(root, &session)?;
        for prompt in prompts {
            if known.holds(&prompt) {
                stats.history_in_transcripts += 1;
                continue;
            }
            if prompt.text.trim().is_empty() {
                stats.skipped_records += 1;
                continue;
            }
            let fact = blockdag::text_fact(
                schema::content_fact::modality::TEXT,
                schema::content_fact::direction::IN,
                prompt.text,
            )?;
            stats.content_parts += 1;
            let (block, identity) = blockdag::block_parts(None, prompt.timestamp, fact)?;
            let receipt = blockdag::block_source_projection(
                schema::source_projection::SOURCE_CLAUDE_CODE,
                block,
            )?;
            let receipt = blockdag::annotate_source_projection(
                receipt,
                blockdag::ProjectionAnnotations {
                    source_timestamp: prompt.timestamp,
                    raw_role: Some("user".to_owned()),
                    ..blockdag::ProjectionAnnotations::default()
                },
            )?;
            let receipt = blockdag::annotate_source_projection(
                receipt,
                blockdag::ProjectionAnnotations {
                    source_locator: Some(format!("session/{session}")),
                    ..blockdag::ProjectionAnnotations::default()
                },
            )?;
            emit(ProjectedFile {
                source_path: path.to_path_buf(),
                fragment: receipt,
                identity,
                stats: ProjectionStats::default(),
            })?;
            summary.fragments_emitted += 1;
            stats.source_projections += 1;
        }
    }
    summary.stats = stats;
    Ok(summary)
}

/// Replace `[Pasted text #N ...]` placeholders with the pasted text: inline
/// `content`, or `paste-cache/<contentHash>.txt` beside the log.
fn expand_pasted(root: &Path, display: &str, pasted: Option<&serde_json::Value>) -> String {
    let mut text = display.to_owned();
    let Some(pasted) = pasted.and_then(serde_json::Value::as_object) else {
        return text;
    };
    for (id, entry) in pasted {
        let content = match entry.get("content").and_then(serde_json::Value::as_str) {
            Some(content) => Some(content.to_owned()),
            None => entry
                .get("contentHash")
                .and_then(serde_json::Value::as_str)
                .and_then(|hash| {
                    fs::read_to_string(root.join("paste-cache").join(format!("{hash}.txt"))).ok()
                }),
        };
        let Some(content) = content else {
            continue;
        };
        let marker = format!("[Pasted text #{id}");
        if let Some(start) = text.find(&marker) {
            if let Some(length) = text[start..].find(']') {
                text.replace_range(start..start + length + 1, &content);
            }
        }
    }
    text
}

/// The prompts the transcripts of one session hold.
#[derive(Default)]
struct TranscriptPrompts {
    texts: HashSet<String>,
    commands: HashSet<String>,
}

impl TranscriptPrompts {
    fn holds(&self, prompt: &HistoryPrompt) -> bool {
        if self.texts.contains(&prompt.text)
            || self.texts.contains(&prompt.display)
            || self.texts.contains(prompt.text.trim())
        {
            return true;
        }
        prompt
            .display
            .strip_prefix('/')
            .and_then(|command| command.split_whitespace().next())
            .is_some_and(|command| self.commands.contains(command))
    }
}

/// Read every `projects/*/<session>.jsonl` beside the log for its prompts.
fn transcript_prompts(root: &Path, session: &str) -> Result<TranscriptPrompts> {
    let mut prompts = TranscriptPrompts::default();
    let projects = root.join("projects");
    let Ok(entries) = fs::read_dir(&projects) else {
        return Ok(prompts);
    };
    for entry in entries {
        let transcript = entry?.path().join(format!("{session}.jsonl"));
        let Ok(file) = File::open(&transcript) else {
            continue;
        };
        for line in BufReader::new(file).split(b'\n') {
            let line = line?;
            let user = memchr::memmem::find(&line, b"\"type\":\"user\"").is_some();
            let queued = memchr::memmem::find(&line, b"queued_command").is_some();
            let command = memchr::memmem::find(&line, b"<command-name>").is_some();
            if !(user || queued || command) {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some(content) = record
                .get("message")
                .and_then(|message| message.get("content"))
            {
                let text = match content {
                    serde_json::Value::String(text) => Some(text.clone()),
                    serde_json::Value::Array(blocks) => {
                        let texts: Vec<&str> = blocks
                            .iter()
                            .filter(|block| {
                                block.get("type").and_then(serde_json::Value::as_str)
                                    == Some("text")
                            })
                            .filter_map(|block| block.get("text")?.as_str())
                            .collect();
                        (!texts.is_empty()).then(|| texts.concat())
                    }
                    _ => None,
                };
                if let Some(text) = text {
                    collect_commands(&text, &mut prompts.commands);
                    prompts.texts.insert(text.trim().to_owned());
                    prompts.texts.insert(text);
                }
            }
            if let Some(prompt) = record
                .get("attachment")
                .and_then(|attachment| attachment.get("prompt"))
                .and_then(serde_json::Value::as_str)
            {
                prompts.texts.insert(prompt.trim().to_owned());
                prompts.texts.insert(prompt.to_owned());
            }
            if let Some(content) = record.get("content").and_then(serde_json::Value::as_str) {
                collect_commands(content, &mut prompts.commands);
            }
        }
    }
    Ok(prompts)
}

fn collect_commands(text: &str, commands: &mut HashSet<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("<command-name>") {
        rest = &rest[start + "<command-name>".len()..];
        let Some(end) = rest.find("</command-name>") else {
            break;
        };
        commands.insert(rest[..end].trim_start_matches('/').to_owned());
        rest = &rest[end..];
    }
}

#[cfg(test)]
struct Projected {
    fragment: Fragment,
    stats: ProjectionStats,
}

fn for_each_jsonl_line<R, F>(
    reader: &mut R,
    mut digest: Option<&mut blake3::Hasher>,
    mut visit: F,
) -> Result<()>
where
    R: BufRead,
    F: FnMut(usize, &[u8]) -> Result<()>,
{
    let mut buffer = Vec::new();
    let mut line_number = 0usize;
    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer)?;
        if read == 0 {
            break;
        }
        if let Some(digest) = digest.as_deref_mut() {
            digest.update(&buffer);
        }
        line_number += 1;
        if buffer.last() == Some(&b'\n') {
            buffer.pop();
        }
        if buffer.last() == Some(&b'\r') {
            buffer.pop();
        }
        if buffer.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        visit(line_number, &buffer)?;
    }
    Ok(())
}

fn scanner_bytes(raw: &[u8]) -> Result<Bytes> {
    let first = raw
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .ok_or_else(|| anyhow!("empty JSONL record"))?;
    let last = raw
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .expect("a first non-whitespace byte implies a last byte");
    Ok(Bytes::from_source(raw[first..=last].to_vec()))
}

/// One record's content, in the order the agent experienced it.
struct RecordDraft {
    parts: Vec<PartDraft>,
}

struct PartDraft {
    fact: Fragment,
    tool_call_id: Option<String>,
    responds_to: Option<ToolReference>,
}

impl RecordDraft {
    fn shape(&self) -> RecordShape {
        RecordShape {
            parts: self
                .parts
                .iter()
                .map(|part| PartShape {
                    fact: part.fact.root().expect("canonical content fact is rooted"),
                    tool_call_id: part.tool_call_id.clone(),
                    responds_to: part.responds_to.clone(),
                })
                .collect(),
        }
    }
}

/// The content of one record, or `None` when it has none.
///
/// A user record is what the agent perceived; an assistant record is one of
/// the records of an assistant message, with its sealed reasoning beside the
/// thinking it seals. Harness context (`attachment`) and `system` records are
/// what the agent was shown: one EVENT part holding the canonical JSON of the
/// record without its envelope, direction `in`.
fn record_parts(
    record: &RawRecord,
    raw_record: &[u8],
    source: &SourceKey,
    stats: &mut ProjectionStats,
) -> Result<Option<RecordDraft>> {
    let direction = match record.record_type.as_str() {
        "user" => schema::content_fact::direction::IN,
        "assistant" => schema::content_fact::direction::OUT,
        "attachment" if record.attachment_type.as_deref() == Some("total_tokens_reminder") => {
            return Ok(None)
        }
        "system" if record.subtype.as_deref() == Some("turn_duration") => return Ok(None),
        "attachment" | "system" => {
            let body =
                archive_source::canonical_json_without(scanner_bytes(raw_record)?, ENVELOPE_KEYS)
                    .map_err(|error| anyhow!("canonicalize Claude Code harness record: {error}"))?;
            if body == "{}" {
                return Ok(None);
            }
            let fact = blockdag::text_fact(
                schema::content_fact::modality::EVENT,
                schema::content_fact::direction::IN,
                body,
            )?;
            return Ok(Some(RecordDraft {
                parts: vec![PartDraft {
                    fact,
                    tool_call_id: None,
                    responds_to: None,
                }],
            }));
        }
        _ => return Ok(None),
    };
    let explicit_tool_source = record
        .source_tool_assistant_uuid
        .as_ref()
        .map(|uuid| SourceKey::new(source.session.clone(), uuid.clone()));
    let mut parts = Vec::new();
    for block in &record.blocks {
        let responds_to = (block.kind == BlockKind::ToolResult)
            .then(|| {
                block.correlator.as_ref().map(|correlator| ToolReference {
                    source: explicit_tool_source.clone(),
                    session: source.session.clone(),
                    correlator: correlator.clone(),
                })
            })
            .flatten();
        for item in &block.items {
            let Some(fact) = project_item(item, block.kind, direction, stats)? else {
                continue;
            };
            parts.push(PartDraft {
                fact,
                tool_call_id: (block.kind == BlockKind::ToolUse)
                    .then(|| block.correlator.clone())
                    .flatten(),
                responds_to: responds_to.clone(),
            });
        }
        if let Some((token, media_type)) = &block.sealed {
            parts.push(PartDraft {
                fact: archive_source::sealed_fact(
                    token,
                    media_type,
                    archive_source::SealedAlphabet::Standard,
                )?,
                tool_call_id: None,
                responds_to: None,
            });
            stats.sealed_tokens += 1;
        }
    }
    Ok((!parts.is_empty()).then_some(RecordDraft { parts }))
}

/// Project one record into its nodes: one whole node (the node and its fact)
/// per fact, each with its receipt.
fn project_record(
    record: RawRecord,
    version: Option<&SourceKey>,
    raw_record: &[u8],
    plan: &SourcePlan,
    receipts: &mut BTreeMap<Id, Id>,
    stats: &mut ProjectionStats,
) -> Result<Vec<(Fragment, blockdag::Identity)>> {
    // A further version of its uuid is planned under its own key.
    let Some(key) = version.cloned().or_else(|| record.source_key()) else {
        if matches!(
            record.record_type.as_str(),
            "user" | "assistant" | "attachment" | "system"
        ) {
            stats.missing_source_identity += 1;
        }
        stats.skipped_records += 1;
        return Ok(Vec::new());
    };
    if plan.held.contains(&plan.canonical_key(&key)) {
        // Inside a message the transcript does not yet show complete: it
        // lands on the import that sees the message end.
        stats.held_back_records += 1;
        if let Some(draft) =
            record_parts(&record, raw_record, &key, &mut ProjectionStats::default())?
        {
            stats.held_back_facts += draft.parts.len();
        }
        return Ok(Vec::new());
    }
    let Some(draft) = record_parts(&record, raw_record, &key, stats)? else {
        stats.skipped_records += 1;
        return Ok(Vec::new());
    };
    let class = plan.canonical_key(&key);
    let chain = plan.nodes.get(&class).ok_or_else(|| {
        anyhow!(
            "projectable Claude Code source {} was absent from its source plan",
            key.locator()
        )
    })?;
    let planned_parents = plan.parents.get(&class).cloned().unwrap_or_default();
    if record.forked_from.is_none() && record.parent_uuid.is_some() && planned_parents.is_empty() {
        // The observed edge ended at a contentless root after canonical
        // contraction. This is distinct from a corpus-external predecessor,
        // which remains in `planned_parents` and is counted as unresolved.
        stats.skipped_parents += 1;
    }
    stats.unresolved_parents += planned_parents
        .iter()
        .filter(|parent| !plan.nodes.contains_key(*parent))
        .count();
    stats.unresolved_tool_results += plan.unresolved_results[&class];

    let timestamp = record.timestamp.and_then(epoch_interval);
    let annotations = blockdag::ProjectionAnnotations {
        source_timestamp: timestamp,
        raw_role: record.role,
        raw_model: record.model,
        ..blockdag::ProjectionAnnotations::default()
    };
    // The model an assistant record names wrote its `out` parts only. Claude
    // Code itself writes some assistant records (an API error, an
    // interruption) and labels them `<synthetic>`: no model wrote those, so
    // they keep `raw_model` and get no `generated_by`.
    let generator = annotations
        .raw_model
        .as_deref()
        .filter(|label| *label != SYNTHETIC_MODEL)
        .map(blockdag::vendor_model);
    let mut previous = plan.firsts[&class].clone();
    let mut fragments = Vec::with_capacity(draft.parts.len());
    for (position, part) in draft.parts.into_iter().enumerate() {
        let produced = exists!(pattern!(&part.fact, [{
            _?fact @ schema::content_fact::direction: schema::content_fact::direction::OUT
        }]));
        let (mut node, identity) =
            blockdag::block_parts(previous.iter().copied(), timestamp, part.fact)?;
        let node_id = chain[position];
        if node.root() != Some(node_id) {
            bail!(
                "Claude Code source {} materialized node {position} differently from its plan",
                key.locator()
            );
        }
        if let Some(call) = plan.responds[&class][position] {
            node += blockdag::responds_to(node_id, call);
        }
        let receipt =
            blockdag::block_source_projection(schema::source_projection::SOURCE_CLAUDE_CODE, node)?;
        let receipt = match generator.as_ref().filter(|_| produced) {
            Some(model) => {
                blockdag::annotate_generated_projection(receipt, annotations.clone(), model)?
            }
            None => blockdag::annotate_source_projection(receipt, annotations.clone())?,
        };
        // An alias line repeats its nodes: each is counted once.
        if let std::collections::btree_map::Entry::Vacant(entry) = receipts.entry(node_id) {
            entry.insert(receipt.root().expect("canonical receipt is rooted"));
            stats.source_projections += 1;
        }
        fragments.push((receipt, identity));
        stats.content_parts += 1;
        previous = BTreeSet::from([node_id]);
    }
    Ok(fragments)
}

fn project_item(
    item: &RawItem,
    block_kind: BlockKind,
    record_direction: Id,
    stats: &mut ProjectionStats,
) -> Result<Option<Fragment>> {
    match item {
        RawItem::Text(text) => {
            if text.trim().is_empty() {
                return Ok(None);
            }
            let (modality, direction) = match block_kind {
                BlockKind::Text => (schema::content_fact::modality::TEXT, record_direction),
                BlockKind::Thinking => (
                    schema::content_fact::modality::THINKING,
                    schema::content_fact::direction::OUT,
                ),
                BlockKind::ToolUse => (
                    schema::content_fact::modality::TOOL_CALL,
                    schema::content_fact::direction::OUT,
                ),
                BlockKind::ToolResult => (
                    schema::content_fact::modality::TOOL_RESULT,
                    schema::content_fact::direction::IN,
                ),
                BlockKind::Image | BlockKind::Other => return Ok(None),
            };
            Ok(Some(blockdag::text_fact(
                modality,
                direction,
                text.clone(),
            )?))
        }
        RawItem::Image(image) => {
            let direction = if block_kind == BlockKind::ToolResult {
                schema::content_fact::direction::IN
            } else {
                record_direction
            };
            project_image(image, direction, stats)
        }
        RawItem::ToolResultStatus(is_error) => {
            let status = if *is_error {
                TOOL_RESULT_STATUS_ERROR
            } else {
                TOOL_RESULT_STATUS_OK
            };
            Ok(Some(blockdag::text_fact(
                schema::content_fact::modality::EVENT,
                schema::content_fact::direction::IN,
                status,
            )?))
        }
    }
}

fn project_image(
    image: &RawImageSource,
    direction: Id,
    stats: &mut ProjectionStats,
) -> Result<Option<Fragment>> {
    if image.source_type == "base64" {
        let Some(data) = image.data.as_deref() else {
            stats.undecodable_images += 1;
            return Ok(None);
        };
        let bytes = match BASE64_STANDARD.decode(data.as_bytes()) {
            Ok(bytes) => bytes,
            Err(_) => {
                stats.undecodable_images += 1;
                return Ok(None);
            }
        };
        let media_type = image
            .media_type
            .as_deref()
            .map(files::normalize_media_type_or_default)
            .unwrap_or_else(|| files::DEFAULT_MEDIA_TYPE.to_owned());
        return Ok(Some(blockdag::blob_fact(
            schema::content_fact::modality::IMAGE,
            direction,
            bytes,
            &media_type,
        )?));
    }

    let Some(pointer) = usable_pointer(image) else {
        stats.undecodable_images += 1;
        return Ok(None);
    };
    let media_type = image
        .media_type
        .as_deref()
        .map(files::normalize_media_type_or_default);
    Ok(Some(blockdag::asset_pointer_fact(
        schema::content_fact::modality::IMAGE,
        direction,
        schema::source_projection::SOURCE_CLAUDE_CODE,
        pointer.clone(),
        media_type.as_deref(),
        image.size,
    )?))
}

fn epoch_interval(epoch: Epoch) -> Option<Inline<NsTAIInterval>> {
    (epoch, epoch).try_to_inline().ok()
}

// ---------------------------------------------------------------------------
// Streaming field scanner
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RawRecord {
    record_type: String,
    subtype: Option<String>,
    session_id: Option<String>,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    attachment_type: Option<String>,
    forked_from: Option<ForkedFrom>,
    source_tool_assistant_uuid: Option<String>,
    timestamp: Option<Epoch>,
    role: Option<String>,
    model: Option<String>,
    /// The provider's message id: the records of one message share it.
    message_id: Option<String>,
    blocks: Vec<RawBlock>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ForkedFrom {
    session_id: Option<String>,
    message_uuid: Option<String>,
}

impl RawRecord {
    fn source_key(&self) -> Option<SourceKey> {
        Some(SourceKey::new(self.session_id.clone()?, self.uuid.clone()?))
    }

    fn fork_origin_key(&self) -> Result<Option<SourceKey>> {
        let Some(forked_from) = &self.forked_from else {
            return Ok(None);
        };
        match (&forked_from.session_id, &forked_from.message_uuid) {
            (Some(session), Some(uuid)) => Ok(Some(SourceKey::new(session.clone(), uuid.clone()))),
            _ => bail!("Claude Code forkedFrom must name both sessionId and messageUuid"),
        }
    }
}

struct RawBlock {
    kind: BlockKind,
    correlator: Option<String>,
    items: Vec<RawItem>,
    /// A sealed reasoning token and its media type: a thinking block's
    /// `signature` or a `redacted_thinking` block's `data`.
    sealed: Option<(String, &'static str)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
    ToolResult,
    Image,
    Other,
}

enum RawItem {
    Text(String),
    Image(RawImageSource),
    ToolResultStatus(bool),
}

fn usable_pointer(image: &RawImageSource) -> Option<&String> {
    let preferred = if image.source_type == "url" {
        [&image.url, &image.file_id]
    } else {
        [&image.file_id, &image.url]
    };
    preferred
        .into_iter()
        .flatten()
        .find(|pointer| !pointer.trim().is_empty())
}

#[derive(Default)]
struct RawImageSource {
    source_type: String,
    media_type: Option<String>,
    data: Option<String>,
    url: Option<String>,
    file_id: Option<String>,
    size: Option<u128>,
}

#[derive(Default)]
struct BlockAccum {
    block_type: String,
    signature: Option<String>,
    data: Option<String>,
    text: Option<String>,
    thinking: Option<String>,
    tool_id: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    input_raw: Option<Bytes>,
    is_error: Option<bool>,
    tool_result_items: Vec<RawItem>,
    image_source: Option<RawImageSource>,
}

#[derive(Default)]
struct MessagePart {
    id: Option<String>,
    role: Option<String>,
    model: Option<String>,
    blocks: Vec<RawBlock>,
}

fn scan_syntax(message: &str) -> sc::ScanError {
    sc::ScanError::Syntax(message.to_owned())
}

fn bytes_to_string(bytes: Bytes) -> std::result::Result<String, sc::ScanError> {
    Ok(bytes
        .view::<str>()
        .map_err(|_| scan_syntax("invalid UTF-8 string"))?
        .as_ref()
        .to_owned())
}

fn parse_optional_string(bytes: &mut Bytes) -> std::result::Result<Option<String>, sc::ScanError> {
    if bytes.first().copied() == Some(b'"') {
        Ok(Some(bytes_to_string(sc::parse_string(bytes)?)?))
    } else {
        sc::skip_value(bytes)?;
        Ok(None)
    }
}

fn capture_raw_json(bytes: &mut Bytes) -> std::result::Result<String, sc::ScanError> {
    let before = bytes.clone();
    sc::skip_value(bytes)?;
    let consumed = before.len() - bytes.len();
    Ok(String::from_utf8_lossy(before.slice(0..consumed).as_ref()).into_owned())
}

fn scan_record(bytes: &mut Bytes) -> std::result::Result<RawRecord, sc::ScanError> {
    let mut record = RawRecord::default();
    sc::object(bytes, &mut record, |record, key, value| {
        let key = key
            .view::<str>()
            .map_err(|_| scan_syntax("invalid UTF-8 object key"))?;
        match key.as_ref() {
            "type" => record.record_type = parse_optional_string(value)?.unwrap_or_default(),
            "subtype" => record.subtype = parse_optional_string(value)?,
            "sessionId" => record.session_id = parse_optional_string(value)?,
            "uuid" => record.uuid = parse_optional_string(value)?,
            "parentUuid" => record.parent_uuid = parse_optional_string(value)?,
            "attachment" => record.attachment_type = scan_attachment_type(value)?,
            "forkedFrom" => record.forked_from = scan_optional_forked_from(value)?,
            "sourceToolAssistantUUID" => {
                record.source_tool_assistant_uuid = parse_optional_string(value)?
            }
            "timestamp" => {
                record.timestamp = parse_optional_string(value)?
                    .as_deref()
                    .and_then(parse_iso_timestamp)
            }
            "message" => {
                let message = scan_message(value)?;
                record.role = message.role;
                record.message_id = message.id;
                record.model = message.model;
                record.blocks = message.blocks;
            }
            _ => sc::skip_value(value)?,
        }
        Ok(record)
    })?;
    Ok(record)
}

fn scan_attachment_type(bytes: &mut Bytes) -> std::result::Result<Option<String>, sc::ScanError> {
    if bytes.first().copied() != Some(b'{') {
        sc::skip_value(bytes)?;
        return Ok(None);
    }
    let mut kind = None;
    sc::object(bytes, &mut kind, |kind, key, value| {
        let key = key
            .view::<str>()
            .map_err(|_| scan_syntax("invalid UTF-8 object key"))?;
        match key.as_ref() {
            "type" => *kind = parse_optional_string(value)?,
            _ => sc::skip_value(value)?,
        }
        Ok(kind)
    })?;
    Ok(kind)
}

fn scan_optional_forked_from(
    bytes: &mut Bytes,
) -> std::result::Result<Option<ForkedFrom>, sc::ScanError> {
    if bytes.first().copied() != Some(b'{') {
        sc::skip_value(bytes)?;
        return Ok(None);
    }
    let mut forked_from = ForkedFrom::default();
    sc::object(bytes, &mut forked_from, |forked_from, key, value| {
        let key = key
            .view::<str>()
            .map_err(|_| scan_syntax("invalid UTF-8 object key"))?;
        match key.as_ref() {
            "sessionId" => forked_from.session_id = parse_optional_string(value)?,
            "messageUuid" => forked_from.message_uuid = parse_optional_string(value)?,
            _ => sc::skip_value(value)?,
        }
        Ok(forked_from)
    })?;
    Ok(Some(forked_from))
}

fn scan_message(bytes: &mut Bytes) -> std::result::Result<MessagePart, sc::ScanError> {
    let mut message = MessagePart::default();
    sc::object(bytes, &mut message, |message, key, value| {
        let key = key
            .view::<str>()
            .map_err(|_| scan_syntax("invalid UTF-8 object key"))?;
        match key.as_ref() {
            "id" => message.id = parse_optional_string(value)?,
            "role" => message.role = parse_optional_string(value)?,
            "model" => message.model = parse_optional_string(value)?,
            "content" => message.blocks = scan_content(value)?,
            _ => sc::skip_value(value)?,
        }
        Ok(message)
    })?;
    Ok(message)
}

fn scan_content(bytes: &mut Bytes) -> std::result::Result<Vec<RawBlock>, sc::ScanError> {
    match bytes.first().copied() {
        Some(b'"') => Ok(vec![RawBlock {
            kind: BlockKind::Text,
            correlator: None,
            items: vec![RawItem::Text(bytes_to_string(sc::parse_string(bytes)?)?)],
            sealed: None,
        }]),
        Some(b'[') => {
            let mut blocks = Vec::new();
            sc::array(bytes, &mut blocks, |blocks, element| {
                blocks.push(scan_content_block(element)?);
                Ok(blocks)
            })?;
            Ok(blocks)
        }
        _ => {
            sc::skip_value(bytes)?;
            Ok(Vec::new())
        }
    }
}

fn scan_content_block(bytes: &mut Bytes) -> std::result::Result<RawBlock, sc::ScanError> {
    let mut block = BlockAccum::default();
    sc::object(bytes, &mut block, |block, key, value| {
        let key = key
            .view::<str>()
            .map_err(|_| scan_syntax("invalid UTF-8 object key"))?;
        match key.as_ref() {
            "type" => block.block_type = parse_optional_string(value)?.unwrap_or_default(),
            "text" => block.text = parse_optional_string(value)?,
            "thinking" => block.thinking = parse_optional_string(value)?,
            "signature" => block.signature = parse_optional_string(value)?,
            "data" => block.data = parse_optional_string(value)?,
            "id" => block.tool_id = parse_optional_string(value)?,
            "name" => block.tool_name = parse_optional_string(value)?,
            "tool_use_id" => block.tool_use_id = parse_optional_string(value)?,
            "input" => block.input_raw = Some(sc::take_value(value)?),
            "is_error" => block.is_error = parse_optional_bool(value)?,
            "content" => block.tool_result_items = scan_tool_result_content(value)?,
            "source" => block.image_source = scan_optional_image_source(value)?,
            _ => sc::skip_value(value)?,
        }
        Ok(block)
    })?;
    build_block(block)
}

fn build_block(block: BlockAccum) -> std::result::Result<RawBlock, sc::ScanError> {
    let sealed = match block.block_type.as_str() {
        "thinking" => block
            .signature
            .clone()
            .filter(|token| !token.is_empty())
            .map(|token| {
                (
                    token,
                    schema::content_fact::sealed::ANTHROPIC_THINKING_SIGNATURE,
                )
            }),
        "redacted_thinking" => block
            .data
            .clone()
            .filter(|token| !token.is_empty())
            .map(|token| {
                (
                    token,
                    schema::content_fact::sealed::ANTHROPIC_REDACTED_THINKING,
                )
            }),
        _ => None,
    };
    let mut raw = build_unsealed_block(block)?;
    raw.sealed = sealed;
    Ok(raw)
}

fn build_unsealed_block(block: BlockAccum) -> std::result::Result<RawBlock, sc::ScanError> {
    Ok(match block.block_type.as_str() {
        "text" => RawBlock {
            kind: BlockKind::Text,
            correlator: None,
            items: block.text.into_iter().map(RawItem::Text).collect(),
            sealed: None,
        },
        "thinking" => RawBlock {
            kind: BlockKind::Thinking,
            correlator: None,
            items: block.thinking.into_iter().map(RawItem::Text).collect(),
            sealed: None,
        },
        "tool_use" => RawBlock {
            kind: BlockKind::ToolUse,
            correlator: block.tool_id,
            items: vec![RawItem::Text(canonical_tool_call(
                block.tool_name,
                block.input_raw,
            )?)],
            sealed: None,
        },
        "tool_result" => {
            let mut items = Vec::with_capacity(
                block.tool_result_items.len() + usize::from(block.is_error.is_some()),
            );
            items.extend(block.is_error.map(RawItem::ToolResultStatus));
            items.extend(block.tool_result_items);
            RawBlock {
                kind: BlockKind::ToolResult,
                correlator: block.tool_use_id,
                items,
                sealed: None,
            }
        }
        "image" => RawBlock {
            kind: BlockKind::Image,
            correlator: None,
            items: block.image_source.into_iter().map(RawItem::Image).collect(),
            sealed: None,
        },
        _ => RawBlock {
            kind: BlockKind::Other,
            correlator: None,
            items: Vec::new(),
            sealed: None,
        },
    })
}

fn parse_optional_bool(bytes: &mut Bytes) -> std::result::Result<Option<bool>, sc::ScanError> {
    let raw = capture_raw_json(bytes)?;
    Ok(match raw.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    })
}

fn canonical_tool_call(
    name: Option<String>,
    input: Option<Bytes>,
) -> std::result::Result<String, sc::ScanError> {
    let name = match name {
        Some(name) => archive_source::canonical_json_string(&name),
        None => "null".to_owned(),
    };
    let input = match input {
        Some(raw) => archive_source::canonical_json(raw)?,
        None => "null".to_owned(),
    };
    Ok(format!(r#"{{"name":{name},"input":{input}}}"#))
}

fn scan_tool_result_content(bytes: &mut Bytes) -> std::result::Result<Vec<RawItem>, sc::ScanError> {
    match bytes.first().copied() {
        Some(b'"') => Ok(vec![RawItem::Text(bytes_to_string(sc::parse_string(
            bytes,
        )?)?)]),
        Some(b'[') => {
            let mut items = Vec::new();
            sc::array(bytes, &mut items, |items, element| {
                if let Some(item) = scan_tool_result_item(element)? {
                    items.push(item);
                }
                Ok(items)
            })?;
            Ok(items)
        }
        _ => Ok(vec![RawItem::Text(capture_raw_json(bytes)?)]),
    }
}

#[derive(Default)]
struct ToolResultItem {
    item_type: String,
    text: Option<String>,
    source: Option<RawImageSource>,
}

fn scan_tool_result_item(bytes: &mut Bytes) -> std::result::Result<Option<RawItem>, sc::ScanError> {
    if bytes.first().copied() == Some(b'"') {
        return Ok(Some(RawItem::Text(bytes_to_string(sc::parse_string(
            bytes,
        )?)?)));
    }
    if bytes.first().copied() != Some(b'{') {
        sc::skip_value(bytes)?;
        return Ok(None);
    }

    let mut item = ToolResultItem::default();
    sc::object(bytes, &mut item, |item, key, value| {
        let key = key
            .view::<str>()
            .map_err(|_| scan_syntax("invalid UTF-8 object key"))?;
        match key.as_ref() {
            "type" => item.item_type = parse_optional_string(value)?.unwrap_or_default(),
            "text" => item.text = parse_optional_string(value)?,
            "source" => item.source = scan_optional_image_source(value)?,
            _ => sc::skip_value(value)?,
        }
        Ok(item)
    })?;
    if item.item_type == "image" {
        Ok(item.source.map(RawItem::Image))
    } else {
        Ok(item.text.map(RawItem::Text))
    }
}

fn scan_optional_image_source(
    bytes: &mut Bytes,
) -> std::result::Result<Option<RawImageSource>, sc::ScanError> {
    if bytes.first().copied() != Some(b'{') {
        sc::skip_value(bytes)?;
        return Ok(None);
    }
    let mut source = RawImageSource::default();
    let mut file_size = None;
    let mut legacy_size = None;
    sc::object(bytes, &mut source, |source, key, value| {
        let key = key
            .view::<str>()
            .map_err(|_| scan_syntax("invalid UTF-8 object key"))?;
        match key.as_ref() {
            "type" => source.source_type = parse_optional_string(value)?.unwrap_or_default(),
            "media_type" => source.media_type = parse_optional_string(value)?,
            "data" => source.data = parse_optional_string(value)?,
            "url" => source.url = parse_optional_string(value)?,
            "file_id" => source.file_id = parse_optional_string(value)?,
            "file_size" => file_size = Some(capture_raw_json(value)?.trim().parse::<u128>().ok()),
            "size" => legacy_size = Some(capture_raw_json(value)?.trim().parse::<u128>().ok()),
            _ => sc::skip_value(value)?,
        }
        Ok(source)
    })?;
    source.size = file_size.unwrap_or_else(|| legacy_size.flatten());
    Ok(Some(source))
}

fn parse_iso_timestamp(value: &str) -> Option<Epoch> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        value.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::Cursor;

    use tempfile::TempDir;
    use triblespace::core::metadata;
    use triblespace::core::repo::pile::{Pile, PileSnapshot};
    use triblespace::core::repo::BlobStoreGet;
    use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
    use triblespace::prelude::inlineencodings::Handle;

    use super::*;
    const SINGLE_FILE: &str = r#"{"type":"user","sessionId":"session-1","uuid":"u1","parentUuid":null,"timestamp":"2026-03-01T15:34:01.542Z","message":{"role":"user","content":"hello there"}}
{"type":"assistant","sessionId":"session-1","uuid":"a1","parentUuid":"u1","timestamp":"2026-03-01T15:34:02.000Z","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"thinking","thinking":"consider it","signature":"opaque"},{"type":"text","text":"hi!"},{"type":"tool_use","id":"toolu_1","name":"Screenshot","input":{"display":1}}]}}
{"type":"user","sessionId":"session-1","uuid":"u2","parentUuid":"a1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"screenshot below"},{"type":"image","source":{"type":"base64","media_type":"IMAGE/PNG; charset=binary","data":"iVBORw=="}}]}]}}
{"type":"assistant","sessionId":"session-1","uuid":"a2","parentUuid":"u2","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"text","text":"seen it"}]}}"#;

    fn prescan_text(text: &str, path: &Path) -> PreScan {
        let mut cursor = Cursor::new(text.as_bytes());
        prescan_reader(&mut cursor, path).unwrap()
    }

    fn project_text(text: &str, path: &Path) -> Projected {
        let prescan = prescan_text(text, path);
        let plan = SourcePlan::from_scans(std::slice::from_ref(&prescan)).unwrap();
        let bytes = Bytes::from_source(text.as_bytes().to_vec());
        let snapshot = archive_source::FrozenSource {
            digest: *blake3::hash(bytes.as_ref()).as_bytes(),
            bytes,
        };
        let mut fragment = Fragment::empty();
        let summary = project_snapshot(path, snapshot, &prescan, &plan, None, &mut |projected| {
            fragment += projected.fragment;
            Ok(())
        })
        .unwrap();
        Projected {
            fragment,
            stats: summary.stats,
        }
    }

    #[test]
    fn compact_summary_flag_is_additive_provenance_not_user_authorship() {
        let mut variants = Vec::new();
        for (flag, marked) in [
            ("", false),
            (",\"isCompactSummary\":false", false),
            (",\"isCompactSummary\":true", true),
        ] {
            let source = format!(
                r#"{{"type":"user","sessionId":"s","uuid":"u","parentUuid":null{flag},"message":{{"role":"user","content":"the same words"}}}}"#
            );
            let projected = project_text(&source, Path::new("summary.jsonl"));
            let block = block_with_payload(&projected.fragment, "the same words");
            let receipt = projection_for_block(&projected.fragment, block);
            assert_eq!(
                exists!(pattern!(&projected.fragment, [{
                    receipt @ metadata::tag: schema::source_projection::COMPACTION_SUMMARY
                }])),
                marked,
            );
            assert!(exists!(pattern!(&projected.fragment, [{
                receipt @ schema::source_projection::raw_role: "user"
            }])));
            assert!(exists!(pattern!(&projected.fragment, [
                { block @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::modality: schema::content_fact::modality::TEXT,
                    schema::content_fact::direction: schema::content_fact::direction::IN },
            ])));
            assert!(!exists!(pattern!(&projected.fragment, [{
                receipt @ schema::source_projection::author: _?author
            }])));
            assert!(!exists!(pattern!(&projected.fragment, [{
                receipt @ schema::source_projection::generated_by: _?model
            }])));
            variants.push((block, receipt, projected.fragment));
        }
        assert_eq!(
            variants[0].0, variants[2].0,
            "provenance does not change the node"
        );
        assert_eq!(
            variants[0].1, variants[2].1,
            "reimport enriches the same receipt"
        );
        assert_eq!(variants[0].2.facts(), variants[1].2.facts());
        assert!(variants[0]
            .2
            .facts()
            .iter()
            .all(|fact| variants[2].2.facts().contains(&fact)));
    }

    #[test]
    fn generated_by_names_the_model_on_what_it_wrote_and_never_on_synthetic_records() {
        let text = r#"{"type":"user","sessionId":"s","uuid":"u1","parentUuid":null,"message":{"role":"user","content":"hello there"}}
{"type":"assistant","sessionId":"s","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"text","text":"hi!"}]}}
{"type":"assistant","sessionId":"s","uuid":"a2","parentUuid":"a1","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"API Error: overloaded"}]}}"#;
        let projected = project_text(text, Path::new("synthetic.jsonl"));
        let generators = |block: Id| -> BTreeSet<Id> {
            find!(
                model: Id,
                pattern!(projected.fragment.facts(), [{
                    _?receipt @
                    schema::source_projection::projects_to: block,
                    schema::source_projection::generated_by: ?model,
                }])
            )
            .collect()
        };
        let opus = blockdag::vendor_model("claude-opus-4")
            .root()
            .expect("vendor model is rooted");
        let reply = block_with_payload(&projected.fragment, "hi!");
        let error = block_with_payload(&projected.fragment, "API Error: overloaded");
        let prompt = block_with_payload(&projected.fragment, "hello there");
        assert_eq!(generators(reply), BTreeSet::from([opus]));
        assert!(
            generators(error).is_empty(),
            "no model wrote a synthetic record"
        );
        assert!(generators(prompt).is_empty(), "the prompt was perceived");
        assert_eq!(
            ids_with_tag(&projected.fragment, schema::vendor_model::KIND),
            BTreeSet::from([opus])
        );
        let (_directory, reader) = empty_reader();
        blockdag::validate_catalog_union(&reader, &TribleSet::new(), &projected.fragment).unwrap();
    }

    fn empty_reader() -> (TempDir, PileSnapshot) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.pile");
        File::create(&path).unwrap();
        let mut pile = Pile::open(&path).unwrap();
        let reader = pile.snapshot().unwrap();
        pile.close().unwrap();
        (directory, reader)
    }

    fn ids_with_tag(fragment: &Fragment, tag: Id) -> BTreeSet<Id> {
        find!(
            entity: Id,
            pattern!(fragment.facts(), [{ ?entity @ metadata::tag: &tag }])
        )
        .collect()
    }

    fn block_with_payload(fragment: &Fragment, needle: &str) -> Id {
        let mut fragment = fragment.clone();
        let reader = fragment.blobs_mut().snapshot().unwrap();
        for (block, payload) in find!(
            (block: Id, payload: Inline<Handle<UTF8String>>),
            pattern!(fragment.facts(), [
                { ?block @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::payload: ?payload },
            ])
        ) {
            let text: anybytes::View<str> = reader.get(payload).unwrap();
            if text.as_ref() == needle {
                return block;
            }
        }
        panic!("no node holds payload {needle:?}");
    }

    fn projection_for_block(fragment: &Fragment, block: Id) -> Id {
        find!(
            projection: Id,
            pattern!(fragment.facts(), [{
                ?projection @ schema::source_projection::projects_to: &block
            }])
        )
        .next()
        .expect("block has a source projection")
    }

    fn projections_for_block(fragment: &Fragment, block: Id) -> BTreeSet<Id> {
        find!(
            projection: Id,
            pattern!(fragment.facts(), [{
                ?projection @ schema::source_projection::projects_to: &block
            }])
        )
        .collect()
    }

    #[test]
    fn sealed_thinking_is_kept_as_bytes_beside_the_thinking_it_seals() {
        let token = BASE64_STANDARD.encode([7u8; 48]);
        let signed = SINGLE_FILE.replace(
            r#""signature":"opaque""#,
            &format!(r#""signature":"{token}""#),
        );
        let signed = project_text(&signed, Path::new("signed.jsonl"));
        assert_eq!(signed.stats.sealed_tokens, 1);
        // The token is a THINKING blob of its decoded bytes, typed as what it
        // is, produced, and the node right after the thinking it seals.
        let thinking = block_with_payload(&signed.fragment, "consider it");
        let mut probe = signed.fragment.clone();
        let bytes = probe.put::<RawBytes, _>(vec![7u8; 48]);
        let media = probe.put::<UTF8String, _>(
            schema::content_fact::sealed::ANTHROPIC_THINKING_SIGNATURE.to_owned(),
        );
        let sealed: Vec<Id> = find!(
            sealed: Id,
            pattern!(&signed.fragment, [
                { ?sealed @ schema::block::previous: thinking,
                    schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::modality: &schema::content_fact::modality::THINKING,
                    schema::content_fact::direction: &schema::content_fact::direction::OUT,
                    schema::content_fact::blob: bytes,
                    schema::content_fact::media_type: _?media_type },
                { _?media_type @ metadata::name: media },
            ])
        )
        .collect();
        assert_eq!(sealed.len(), 1);
    }

    #[test]
    fn one_message_is_one_act_and_the_results_it_ran_early_follow_it() {
        // Claude Code runs a call as soon as the call is complete and logs
        // its result before the message's next call. The model emitted every
        // call before it perceived any result, so the results follow the
        // whole message, in the order they were logged.
        let source = concat!(
            r#"{"type":"user","sessionId":"act","uuid":"u","parentUuid":null,"message":{"role":"user","content":"look at both"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"act","uuid":"t","parentUuid":"u","message":{"id":"msg_1","role":"assistant","content":[{"type":"thinking","thinking":"two reads"}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"act","uuid":"c1","parentUuid":"t","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file":"a"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"act","uuid":"r1","parentUuid":"c1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"contents of a"}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"act","uuid":"c2","parentUuid":"r1","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file":"b"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"act","uuid":"r2","parentUuid":"c2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"contents of b"}]}}"#,
            "\n",
            r#"{"type":"attachment","sessionId":"act","uuid":"note","parentUuid":"r2","attachment":{"type":"date_change","date":"tomorrow"}}"#,
            "\n",
            r#"{"type":"user","sessionId":"act","uuid":"u2","parentUuid":"note","message":{"role":"user","content":"and now?"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"act","uuid":"answer","parentUuid":"u2","message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"both read"}]}}"#,
        );
        let projected = project_text(source, Path::new("act.jsonl"));
        assert_eq!(projected.stats.source_projections, 9, "one node per fact");
        assert_eq!(projected.stats.unresolved_tool_results, 0);
        assert_eq!(projected.stats.held_back_records, 0);
        let fragment = &projected.fragment;
        let node = |text: &str| block_with_payload(fragment, text);
        let prompt = node("look at both");
        let thinking = node("two reads");
        let first_call = node(r#"{"name":"Read","input":{"file":"a"}}"#);
        let first_result = node("contents of a");
        let second_call = node(r#"{"name":"Read","input":{"file":"b"}}"#);
        let second_result = node("contents of b");
        let note = node(r#"{"attachment":{"date":"tomorrow","type":"date_change"}}"#);
        let question = node("and now?");
        let answer = node("both read");
        let facts = fragment.facts();
        assert_eq!(
            crate::blockdag::turns_along(facts, answer),
            vec![
                vec![prompt],
                vec![thinking, first_call, second_call],
                vec![first_result, second_result, note, question],
                vec![answer],
            ],
            "one message is one act; what arrived inside it is perceived after it"
        );
        // Each result's node names its call's node.
        assert!(exists!(pattern!(fragment, [
            { first_result @ schema::block::responds_to: first_call },
            { second_result @ schema::block::responds_to: second_call },
        ])));
        assert_eq!(
            find!(
                (result: Id, call: Id),
                pattern!(fragment, [{ ?result @ schema::block::responds_to: ?call }])
            )
            .count(),
            2
        );
    }

    #[test]
    fn a_longer_transcript_only_appends_nodes() {
        // Cut at every line, including between a message's calls and their
        // results: what a cut emits is always a prefix of the whole log's
        // thread, so the longer log only appends. What arrived inside a
        // message the cut does not show complete is held back.
        let full = concat!(
            r#"{"type":"user","sessionId":"grow","uuid":"u","parentUuid":null,"timestamp":"2026-03-01T10:00:00Z","message":{"role":"user","content":"look at both"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"grow","uuid":"t","parentUuid":"u","message":{"id":"msg_1","role":"assistant","content":[{"type":"thinking","thinking":"two reads"},{"type":"text","text":"reading"}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"grow","uuid":"c1","parentUuid":"t","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file":"a"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"grow","uuid":"r1","parentUuid":"c1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"contents of a","is_error":false}]}}"#,
            "\n",
            r#"{"type":"attachment","sessionId":"grow","uuid":"h1","parentUuid":"r1","attachment":{"type":"hook_success","content":"checked"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"grow","uuid":"c2","parentUuid":"h1","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file":"b"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"grow","uuid":"r2","parentUuid":"c2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"contents of b"}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"grow","uuid":"m2","parentUuid":"r2","message":{"id":"msg_2","role":"assistant","content":[{"type":"tool_use","id":"toolu_3","name":"Read","input":{"file":"c"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"grow","uuid":"r3","parentUuid":"m2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_3","content":"contents of c"}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"grow","uuid":"p","parentUuid":"r3","message":{"role":"user","content":"and now?"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"grow","uuid":"answer","parentUuid":"p","message":{"id":"msg_3","role":"assistant","content":[{"type":"text","text":"all read"}]}}"#,
            "\n",
        );
        let whole = project_text(full, Path::new("grow.jsonl"));
        assert_eq!(
            whole.stats.held_back_records, 0,
            "a finished log emits everything"
        );
        let whole_receipts = ids_with_tag(&whole.fragment, schema::source_projection::KIND);
        let leaf = block_with_payload(&whole.fragment, "all read");
        let thread = crate::blockdag::thread_to(whole.fragment.facts(), leaf);
        assert_eq!(thread.len(), 13);
        assert_eq!(
            ids_with_tag(&whole.fragment, schema::block::KIND),
            thread.iter().copied().collect::<BTreeSet<_>>()
        );
        let lines: Vec<&str> = full.split_inclusive('\n').collect();
        let mut held = Vec::new();
        for cut in 1..=lines.len() {
            let prefix: String = lines[..cut].concat();
            let part = project_text(&prefix, Path::new("grow.jsonl"));
            let nodes = ids_with_tag(&part.fragment, schema::block::KIND);
            assert_eq!(
                nodes,
                thread[..nodes.len()]
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>(),
                "cut after line {cut}: a prefix of the whole thread, with no dead end"
            );
            assert!(
                ids_with_tag(&part.fragment, schema::source_projection::KIND)
                    .is_subset(&whole_receipts),
                "cut after line {cut}"
            );
            held.push(part.stats.held_back_records);
        }
        // Held back: the results and hook inside msg_1 until its next call
        // is followed by another message, and msg_2's result until the
        // prompt shows msg_2 complete.
        assert_eq!(held, vec![0, 0, 0, 1, 2, 2, 3, 0, 1, 0, 0]);
    }

    #[test]
    fn a_finished_transcript_emits_its_held_back_tail_and_still_only_grows() {
        // A session stopped after its tool ran: it ends on a result logged
        // inside its last message, which the transcript cannot show complete.
        let stopped = concat!(
            r#"{"type":"user","sessionId":"stopped","uuid":"u","parentUuid":null,"message":{"role":"user","content":"look"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"stopped","uuid":"c","parentUuid":"u","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file":"a"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"stopped","uuid":"r","parentUuid":"c","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"contents of a"}]}}"#,
            "\n",
        );
        // Resumed later: a new prompt and a new message.
        let resumed = format!(
            "{stopped}{}\n{}\n",
            r#"{"type":"user","sessionId":"stopped","uuid":"p","parentUuid":"r","message":{"role":"user","content":"go on"}}"#,
            r#"{"type":"assistant","sessionId":"stopped","uuid":"a","parentUuid":"p","message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stopped.jsonl");
        let project = |finished_before: Option<SystemTime>| {
            let mut fragment = Fragment::empty();
            let summary = project_path_finished(&path, finished_before, |projected| {
                fragment += projected.fragment;
                Ok(())
            })
            .unwrap();
            (summary, fragment)
        };
        fs::write(&path, stopped).unwrap();
        let (held, held_fragment) = project(None);
        assert_eq!(
            held.stats.held_back_records, 1,
            "without the option, held back"
        );
        assert_eq!(ids_with_tag(&held_fragment, schema::block::KIND).len(), 2);
        // Last modified before a cutoff an hour from now: finished.
        let cutoff = SystemTime::now() + std::time::Duration::from_secs(3600);
        let (flushed, flushed_fragment) = project(Some(cutoff));
        assert_eq!(flushed.stats.held_back_records, 0);
        let call = block_with_payload(&flushed_fragment, r#"{"name":"Read","input":{"file":"a"}}"#);
        let result = block_with_payload(&flushed_fragment, "contents of a");
        assert!(exists!(pattern!(&flushed_fragment, [
            { result @ schema::block::previous: call, schema::block::responds_to: call },
        ])));
        // Nothing but when: the flushed nodes are exactly what a later
        // record closing the message places.
        let flushed_nodes = ids_with_tag(&flushed_fragment, schema::block::KIND);
        assert!(ids_with_tag(&held_fragment, schema::block::KIND).is_subset(&flushed_nodes));

        fs::write(&path, resumed).unwrap();
        for finished_before in [None, Some(cutoff)] {
            let (grown, grown_fragment) = project(finished_before);
            assert_eq!(grown.stats.held_back_records, 0);
            let leaf = block_with_payload(&grown_fragment, "done");
            let thread = crate::blockdag::thread_to(grown_fragment.facts(), leaf);
            assert_eq!(
                thread[..flushed_nodes.len()]
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>(),
                flushed_nodes,
                "the resumed session only appends to what was flushed"
            );
            assert_eq!(thread.len(), flushed_nodes.len() + 2);
        }
    }

    #[test]
    fn a_dead_branch_tail_lands_while_the_file_is_live() {
        // History before a compaction ends inside msg_1, after its tool ran;
        // the file then starts a new chain, which ends inside msg_2.
        let dead = concat!(
            r#"{"type":"user","sessionId":"s","uuid":"u","parentUuid":null,"message":{"role":"user","content":"look"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","uuid":"c","parentUuid":"u","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file":"a"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"s","uuid":"r","parentUuid":"c","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"contents of a"}]}}"#,
            "\n",
        );
        let live = format!(
            "{dead}{}\n{}\n{}\n",
            r#"{"type":"user","sessionId":"s","uuid":"u2","parentUuid":null,"isCompactSummary":true,"message":{"role":"user","content":"summary, then go on"}}"#,
            r#"{"type":"assistant","sessionId":"s","uuid":"c2","parentUuid":"u2","message":{"id":"msg_2","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file":"b"}}]}}"#,
            r#"{"type":"user","sessionId":"s","uuid":"r2","parentUuid":"c2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"contents of b"}]}}"#,
        );
        let grown = format!(
            "{live}{}\n",
            r#"{"type":"user","sessionId":"s","uuid":"p","parentUuid":"r2","message":{"role":"user","content":"thanks"}}"#,
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.jsonl");
        let project = |body: &str, finished_before: Option<SystemTime>| {
            fs::write(&path, body).unwrap();
            let mut fragment = Fragment::empty();
            let summary = project_path_finished(&path, finished_before, |projected| {
                fragment += projected.fragment;
                Ok(())
            })
            .unwrap();
            (summary, fragment)
        };
        let finished = Some(SystemTime::now() + std::time::Duration::from_secs(3600));
        // Alone, the dead branch is the file's last branch: held.
        let (alone, _) = project(dead, None);
        assert_eq!(alone.stats.held_back_records, 1);
        // Once the file has moved on, only the trailing branch is held.
        let (summary, fragment) = project(&live, None);
        assert_eq!(summary.stats.held_back_records, 1);
        let compact = block_with_payload(&fragment, "summary, then go on");
        let receipt = projection_for_block(&fragment, compact);
        assert!(exists!(pattern!(&fragment, [{
            receipt @ metadata::tag: schema::source_projection::COMPACTION_SUMMARY
        }])));
        assert!(
            !exists!(pattern!(&fragment, [{
                compact @ schema::block::previous: _?parent
            }])),
            "the marker does not invent an edge across the physical boundary"
        );
        let call = block_with_payload(&fragment, r#"{"name":"Read","input":{"file":"a"}}"#);
        let result = block_with_payload(&fragment, "contents of a");
        assert!(exists!(pattern!(&fragment, [
            { result @ schema::block::previous: call, schema::block::responds_to: call },
        ])));
        // Exactly where a finished import, and the grown file, put it.
        let live_nodes = ids_with_tag(&fragment, schema::block::KIND);
        for (body, finished_before) in [(live.as_str(), finished), (grown.as_str(), None)] {
            let (later, later_fragment) = project(body, finished_before);
            assert_eq!(later.stats.held_back_records, 0);
            let later_nodes = ids_with_tag(&later_fragment, schema::block::KIND);
            assert!(live_nodes.is_subset(&later_nodes));
            assert_eq!(
                later_nodes.len(),
                live_nodes.len() + 1 + usize::from(finished_before.is_none())
            );
        }
    }

    #[test]
    fn copies_under_one_uuid_that_say_different_things_are_different_records() {
        // As in a long session file: a fork copy replayed by an older Claude
        // Code (forkedFrom names the origin) and, later in the same file,
        // the original record under the same uuid, its attachment rendered
        // differently after an upgrade.
        let source = concat!(
            r#"{"type":"user","sessionId":"dest","uuid":"u1","parentUuid":null,"message":{"role":"user","content":"start"}}"#,
            "\n",
            r#"{"type":"attachment","sessionId":"dest","uuid":"x","parentUuid":"u1","version":"2.1.114","cwd":"/a","forkedFrom":{"sessionId":"origin","messageUuid":"x"},"attachment":{"type":"file","filename":"notes.txt","content":{"file":{"content":"snippet"}}}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"dest","uuid":"a1","parentUuid":"x","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"after the copy"}]}}"#,
            "\n",
            r#"{"type":"attachment","sessionId":"dest","uuid":"x","parentUuid":"a1","version":"2.1.121","cwd":"/b","slug":"later","attachment":{"type":"file","filename":"notes.txt","displayPath":"notes.txt","content":{"file":{"content":"rendered anew"}}}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"dest","uuid":"a2","parentUuid":"x","message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"after the original"}]}}"#,
            "\n",
            // An equal copy of the original under the same uuid collapses.
            r#"{"type":"attachment","sessionId":"dest","uuid":"x","parentUuid":"a1","version":"2.1.121","cwd":"/c","slug":"again","attachment":{"type":"file","filename":"notes.txt","displayPath":"notes.txt","content":{"file":{"content":"rendered anew"}}}}"#,
            "\n",
        );
        let projected = project_text(source, Path::new("dest.jsonl"));
        assert_eq!(projected.stats.held_back_records, 0);
        let fragment = &projected.fragment;
        let payloads: Vec<Id> = find!(
            node: Id,
            pattern!(fragment.facts(), [
                { ?node @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::modality: &schema::content_fact::modality::EVENT },
            ])
        )
        .collect();
        assert_eq!(
            payloads.len(),
            2,
            "both versions are kept, the equal copy collapses"
        );
        let first = block_with_payload(fragment, "after the copy");
        let second = block_with_payload(fragment, "after the original");
        // A fork copy is placed by its origin, which this file does not
        // hold, so it starts the thread: copy, reply, original, reply.
        let thread = crate::blockdag::thread_to(fragment.facts(), second);
        assert_eq!(thread.len(), 4);
        assert_eq!(thread[1], first);
        assert!(payloads.contains(&thread[0]) && payloads.contains(&thread[2]));
        assert_ne!(thread[0], thread[2]);
        assert_eq!(ids_with_tag(fragment, schema::block::KIND).len(), 5);
    }

    #[test]
    fn a_fork_copy_that_differs_from_its_origin_keeps_its_replay_position() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("origin.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"origin","uuid":"q","message":{"role":"user","content":"question"}}"#,
                "\n",
                r#"{"type":"attachment","sessionId":"origin","uuid":"x","parentUuid":"q","attachment":{"type":"file","filename":"notes.txt","content":"old rendering"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            directory.path().join("fork.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"fork","uuid":"q","forkedFrom":{"sessionId":"origin","messageUuid":"q"},"message":{"role":"user","content":"question"}}"#,
                "\n",
                r#"{"type":"attachment","sessionId":"fork","uuid":"x","parentUuid":"q","forkedFrom":{"sessionId":"origin","messageUuid":"x"},"attachment":{"type":"file","filename":"notes.txt","content":"new rendering"}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"fork","uuid":"a","parentUuid":"x","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"answer"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut union = Fragment::empty();
        let summary = project_path(directory.path(), |projected| {
            union += projected.fragment;
            Ok(())
        })
        .unwrap();
        assert_eq!(summary.stats.held_back_records, 0);
        // The question is one record (an equal copy); the two renderings are
        // two, each after it; the answer follows the fork's own rendering.
        let question = block_with_payload(&union, "question");
        let answer = block_with_payload(&union, "answer");
        let thread = crate::blockdag::thread_to(union.facts(), answer);
        assert_eq!(thread.len(), 3);
        assert_eq!(thread[0], question);
        assert_eq!(ids_with_tag(&union, schema::block::KIND).len(), 4);
        let renderings: BTreeSet<Id> = find!(
            node: Id,
            pattern!(union.facts(), [{ ?node @ schema::block::previous: question }])
        )
        .collect();
        assert_eq!(renderings.len(), 2);
        assert!(renderings.contains(&thread[1]));
    }

    #[test]
    fn a_fork_copy_that_differs_only_in_time_converges_on_its_origin() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("origin.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"origin","uuid":"q","timestamp":"2026-03-01T10:00:00Z","message":{"role":"user","content":"question"}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"origin","uuid":"a","parentUuid":"q","timestamp":"2026-03-01T10:00:05Z","message":{"id":"msg_a","role":"assistant","content":"answer"}}"#,
            ),
        )
        .unwrap();
        fs::write(
            directory.path().join("fork.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"fork","uuid":"q2","timestamp":"2026-03-09T08:00:00Z","forkedFrom":{"sessionId":"origin","messageUuid":"q"},"message":{"role":"user","content":"question"}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"fork","uuid":"a2","parentUuid":"q2","timestamp":"2026-03-09T08:00:00Z","forkedFrom":{"sessionId":"origin","messageUuid":"a"},"message":{"id":"msg_a","role":"assistant","content":"answer"}}"#,
            ),
        )
        .unwrap();
        let mut union = Fragment::empty();
        project_path(directory.path(), |projected| {
            union += projected.fragment;
            Ok(())
        })
        .unwrap();
        assert_eq!(ids_with_tag(&union, schema::block::KIND).len(), 2);
        let answer = block_with_payload(&union, "answer");
        assert_eq!(projections_for_block(&union, answer).len(), 1);
        // Both times annotate the one receipt.
        let receipt = projection_for_block(&union, answer);
        assert_eq!(
            find!(
                timestamp: Inline<NsTAIInterval>,
                pattern!(&union, [{
                    receipt @ schema::source_projection::source_timestamp: ?timestamp
                }])
            )
            .count(),
            2
        );
    }

    #[test]
    fn history_prompts_land_on_transcript_blocks_or_chain_on_their_own() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("projects/p")).unwrap();
        fs::create_dir_all(root.join("paste-cache")).unwrap();
        fs::write(root.join("paste-cache/abc.txt"), "pasted body").unwrap();
        fs::write(
            root.join("projects/p/kept.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"kept","uuid":"u1","message":{"role":"user","content":"in the transcript"}}"#,
                "\n",
                r#"{"type":"system","subtype":"local_command","sessionId":"kept","uuid":"s1","parentUuid":"u1","content":"<command-name>/model</command-name>"}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            root.join("history.jsonl"),
            concat!(
                r#"{"display":"in the transcript","pastedContents":{},"timestamp":1772379241539,"project":"/p","sessionId":"kept"}"#,
                "\n",
                r#"{"display":"/model","pastedContents":{},"timestamp":1772379241600,"project":"/p","sessionId":"kept"}"#,
                "\n",
                r#"{"display":"queued while busy","pastedContents":{},"timestamp":1772379241700,"project":"/p","sessionId":"kept"}"#,
                "\n",
                r#"{"display":"first of a lost session","pastedContents":{},"timestamp":1772379242000,"project":"/p","sessionId":"lost"}"#,
                "\n",
                r#"{"display":"see [Pasted text #1 +3 lines]","pastedContents":{"1":{"id":1,"type":"text","contentHash":"abc"}},"timestamp":1772379243000,"project":"/p","sessionId":"lost"}"#,
                "\n",
            ),
        )
        .unwrap();

        let mut history = Fragment::empty();
        let summary = project_path(&root.join("history.jsonl"), |projected| {
            history += projected.fragment;
            Ok(())
        })
        .unwrap();
        assert_eq!(summary.stats.history_prompts, 5);
        assert_eq!(summary.stats.history_in_transcripts, 2);
        assert_eq!(summary.stats.source_projections, 3);
        // A prompt the transcript holds projects nothing from the log, so it
        // lands only on the transcript's own block.
        let transcript = project_text(
            &fs::read_to_string(root.join("projects/p/kept.jsonl")).unwrap(),
            Path::new("kept.jsonl"),
        );
        let kept = block_with_payload(&transcript.fragment, "in the transcript");
        assert!(!ids_with_tag(&history, schema::block::KIND).contains(&kept));
        // The log keeps no acts: each of the lost session's prompts is a
        // perception of its own, placed by its session and time, with the
        // paste expanded.
        let first = block_with_payload(&history, "first of a lost session");
        let second = block_with_payload(&history, "see pasted body");
        assert!(!exists!(
            pattern!(&history, [{ second @ schema::block::previous: _?any }])
        ));
        let lost = history.put::<UTF8String, _>("session/lost".to_owned());
        for block in [first, second] {
            assert!(exists!(pattern!(&history, [{
                _?receipt @ schema::source_projection::projects_to: block,
                schema::source_projection::source_locator: lost
            }])));
        }
        // Importing the whole directory lands the same blocks.
        let mut union = Fragment::empty();
        project_path(root, |projected| {
            union += projected.fragment;
            Ok(())
        })
        .unwrap();
        assert!(ids_with_tag(&history, schema::block::KIND)
            .is_subset(&ids_with_tag(&union, schema::block::KIND)));
        assert!(ids_with_tag(&union, schema::block::KIND).contains(&kept));
    }

    /// A subagent's `.meta.json` may be beside its transcript on one host and
    /// missing on another. The same nodes then arrive with and without that
    /// leaf annotation: the identity COMMIT is the same record, and only the
    /// annotation COMMIT differs. This is a case the two-COMMIT split exists
    /// for (here in the unit's last piece, where the leaf annotations ride).
    #[test]
    fn a_missing_subagent_meta_changes_only_the_annotation_commit() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = concat!(
            r#"{"type":"user","sessionId":"session","agentId":"a1","isSidechain":true,"uuid":"q","message":{"role":"user","content":"audit this"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"session","agentId":"a1","isSidechain":true,"uuid":"a","parentUuid":"q","message":{"role":"assistant","content":"done"}}"#,
            "\n",
        );
        let mut paths = Vec::new();
        for (host, meta) in [("with", true), ("without", false)] {
            let agents = directory.path().join(host).join("session/subagents");
            fs::create_dir_all(&agents).unwrap();
            let path = agents.join("agent-a1.jsonl");
            fs::write(&path, transcript).unwrap();
            if meta {
                fs::write(
                    agents.join("agent-a1.meta.json"),
                    r#"{"description":"Audit the code","agentType":"general-purpose"}"#,
                )
                .unwrap();
            }
            paths.push(path);
        }
        let pile = directory.path().join("archive.pile");
        let key = directory.path().join("archive.key");
        fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let mut published = Vec::new();
        for path in &paths {
            let mut writer =
                crate::archive_collection::ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
            project_path(path, |projected| {
                writer.stage(projected.fragment, projected.identity)
            })
            .unwrap();
            published.push(writer.commit_unit().unwrap());
            writer.close(Ok(())).unwrap();
        }
        let (with, without) = (&published[0], &published[1]);
        assert_eq!(with.pieces, 1);
        assert_eq!(with.commits.len(), 3, "vocabulary, annotations, identity");
        assert_eq!(with.commits[2], without.commits[2], "the identity COMMIT");
        assert_ne!(with.commits[1], without.commits[1], "the annotation COMMIT");
    }

    #[test]
    fn subagent_meta_annotates_the_transcript_leaves() {
        let directory = tempfile::tempdir().unwrap();
        let agents = directory.path().join("session/subagents");
        fs::create_dir_all(&agents).unwrap();
        let transcript = agents.join("agent-a1.jsonl");
        fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","sessionId":"session","agentId":"a1","isSidechain":true,"uuid":"q","message":{"role":"user","content":"audit this"}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"session","agentId":"a1","isSidechain":true,"uuid":"a","parentUuid":"q","message":{"role":"assistant","content":"done"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            agents.join("agent-a1.meta.json"),
            r#"{"description":"Audit the code","agentType":"general-purpose","toolUseId":"toolu_1","spawnDepth":1}"#,
        )
        .unwrap();
        for path in [transcript.clone(), agents.join("agent-a1.meta.json")] {
            let mut union = Fragment::empty();
            project_path(&path, |projected| {
                union += projected.fragment;
                Ok(())
            })
            .unwrap();
            let leaf = block_with_payload(&union, "done");
            let detail = union.put::<UTF8String, _>(
                r#"{"agentType":"general-purpose","description":"Audit the code","spawnDepth":1,"toolUseId":"toolu_1"}"#
                    .to_owned(),
            );
            assert!(exists!(pattern!(&union, [{
                _?receipt @ schema::source_projection::projects_to: leaf,
                schema::source_projection::source_detail: detail
            }])));
            assert!(!exists!(pattern!(&union, [{
                _?receipt @ metadata::name: _?title
            }])));
            let root = block_with_payload(&union, "audit this");
            assert!(!exists!(pattern!(&union, [{
                _?receipt @ schema::source_projection::projects_to: root,
                schema::source_projection::source_detail: _?any
            }])));
        }
    }

    #[test]
    fn projection_is_canonical_and_does_not_fabricate_missing_time_or_actors() {
        let projected = project_text(SINGLE_FILE, Path::new("/imports/session.jsonl"));
        // Seven readable facts, and the thinking block's signature: one node
        // each.
        assert_eq!(projected.stats.source_projections, 8);
        assert_eq!(projected.stats.content_parts, 8);
        assert_eq!(projected.stats.held_back_records, 0);
        assert_eq!(projected.stats.sealed_tokens, 1);
        assert_eq!(projected.stats.unresolved_tool_results, 0);
        assert_eq!(projected.stats.undecodable_images, 0);

        let (_directory, reader) = empty_reader();
        let (_, validation) =
            blockdag::validate_catalog_union(&reader, &TribleSet::new(), &projected.fragment)
                .unwrap();
        assert_eq!(validation, blockdag::CatalogValidation::Accepted);

        let without_time = block_with_payload(&projected.fragment, "screenshot below");
        assert!(!exists!(pattern!(&projected.fragment, [{
            without_time @ schema::block::timestamp: _?timestamp
        }])));

        for projection in ids_with_tag(&projected.fragment, schema::source_projection::KIND) {
            assert!(!exists!(pattern!(&projected.fragment, [{
                projection @ schema::source_projection::author: _?author
            }])));
            assert!(!exists!(pattern!(&projected.fragment, [{
                projection @ schema::source_projection::experiencer: _?experiencer
            }])));
        }
    }

    #[test]
    fn replay_and_source_move_preserve_intrinsic_identities() {
        let first = project_text(SINGLE_FILE, Path::new("/first/session.jsonl"));
        let replay = project_text(SINGLE_FILE, Path::new("/first/session.jsonl"));
        assert_eq!(first.fragment.facts(), replay.fragment.facts());

        let moved = project_text(SINGLE_FILE, Path::new("/moved/session.jsonl"));
        assert_eq!(
            ids_with_tag(&first.fragment, schema::block::KIND),
            ids_with_tag(&moved.fragment, schema::block::KIND),
        );
        assert_eq!(
            ids_with_tag(&first.fragment, schema::source_projection::KIND),
            ids_with_tag(&moved.fragment, schema::source_projection::KIND),
        );
        assert_eq!(
            first.fragment.facts(),
            moved.fragment.facts(),
            "the path is not kept"
        );
    }

    #[test]
    fn harness_and_system_records_are_perceived_in_the_main_chain() {
        let source = format!(
            "{}\n{}\n{}\n{}\n{}",
            r#"{"type":"user","sessionId":"contract","uuid":"root","parentUuid":null,"message":{"role":"user","content":"before metadata"}}"#,
            r#"{"type":"attachment","sessionId":"contract","uuid":"attachment","parentUuid":"root","cwd":"/a","attachment":{"type":"skill_listing","content":"skills"}}"#,
            r#"{"type":"system","sessionId":"contract","uuid":"system","parentUuid":"attachment","content":"metadata","timestamp":"2026-03-01T15:34:01.542Z"}"#,
            r#"{"type":"user","sessionId":"contract","uuid":"empty","parentUuid":"system","message":{"role":"user","content":[]}}"#,
            r#"{"type":"assistant","sessionId":"contract","uuid":"child","parentUuid":"empty","message":{"role":"assistant","model":"claude","content":"after metadata"}}"#,
        );
        let projected = project_text(&source, Path::new("contract.jsonl"));
        assert_eq!(
            projected.stats.skipped_records, 1,
            "only the empty user record"
        );
        assert_eq!(projected.stats.source_projections, 4);
        assert_eq!(projected.stats.unresolved_parents, 0);

        // The prompt, the harness context and the system record are one
        // perception, without their envelopes (`cwd`, ids, time); the reply
        // follows all of it.
        let prompt = block_with_payload(&projected.fragment, "before metadata");
        let harness = block_with_payload(
            &projected.fragment,
            r#"{"attachment":{"content":"skills","type":"skill_listing"}}"#,
        );
        let system = block_with_payload(&projected.fragment, r#"{"content":"metadata"}"#);
        let child = block_with_payload(&projected.fragment, "after metadata");
        let facts = projected.fragment.facts();
        assert_eq!(
            crate::blockdag::turns_along(facts, child),
            vec![vec![prompt, harness, system], vec![child]]
        );
        for node in [prompt, harness, system] {
            assert_eq!(
                crate::blockdag::node_directions(facts, node),
                BTreeSet::from([schema::content_fact::direction::IN])
            );
        }
    }

    #[test]
    fn no_source_bytes_are_stored_and_bookkeeping_is_skipped() {
        let semantic = r#"{"type":"user","sessionId":"raw-free","uuid":"root","message":{"role":"user","content":"hello"}}"#;
        let telemetry = r#"{"type":"progress","sessionId":"raw-free","uuid":"progress","parentUuid":"root","data":{"type":"agent_progress"}}"#;
        let tokens = r#"{"type":"attachment","sessionId":"raw-free","uuid":"tokens","parentUuid":"progress","attachment":{"type":"total_tokens_reminder","text":"<total_tokens>9 tokens left</total_tokens>"}}"#;
        let empty = r#"{"type":"user","sessionId":"raw-free","uuid":"empty","parentUuid":"tokens","message":{"role":"user","content":[]}}"#;
        let source = format!("{semantic}\n{telemetry}\n{tokens}\n{empty}\n");
        let projected = project_text(&source, Path::new("raw-free.jsonl"));

        assert_eq!(projected.stats.records_seen, 4);
        assert_eq!(projected.stats.skipped_records, 3);
        assert_eq!(projected.stats.source_projections, 1);
        assert!(!exists!(pattern!(&projected.fragment, [{
            _?receipt @ schema::source_projection::raw_record: _?raw
        }])));
        let mut blobs = projected.fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();
        for line in [semantic, telemetry, tokens, empty] {
            let mut probe = Fragment::empty();
            let handle = probe.put::<schema::RawBytes, _>(line.as_bytes().to_vec());
            assert!(
                !reader.contains_blob(handle).unwrap(),
                "a source line is stored verbatim"
            );
        }
    }

    #[test]
    fn append_adds_only_the_new_record_and_its_leaf_annotation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("growing.jsonl");
        let root = r#"{"type":"user","sessionId":"growing","uuid":"root","message":{"role":"user","content":"hello"}}"#;
        fs::write(&path, format!("{root}\n")).unwrap();
        let mut first = Fragment::empty();
        project_path(&path, |projected| {
            first += projected.fragment;
            Ok(())
        })
        .unwrap();

        let reply = r#"{"type":"assistant","sessionId":"growing","uuid":"reply","parentUuid":"root","message":{"role":"assistant","content":"hi"}}"#;
        fs::write(&path, format!("{root}\n{reply}\n")).unwrap();
        let mut second = Fragment::empty();
        project_path(&path, |projected| {
            second += projected.fragment;
            Ok(())
        })
        .unwrap();
        let reply_block = block_with_payload(&second, "hi");
        // Everything the first import wrote is part of the second, except
        // that the old leaf is no longer a leaf.
        let new: Vec<_> = second
            .facts()
            .iter()
            .filter(|trible| !first.facts().contains(trible))
            .collect();
        assert!(!new.is_empty());
        let leaf_receipt = projection_for_block(&second, reply_block);
        assert!(exists!(pattern!(&second, [{
            leaf_receipt @ schema::source_projection::source_locator: _?locator
        }])));
    }

    #[test]
    fn digest_mismatch_emits_no_fragment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mutable.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","sessionId":"mutable","uuid":"before","message":{"role":"user","content":"before"}}"#,
        )
        .unwrap();
        let prescan = prescan_file(&path, None).unwrap();
        let plan = SourcePlan::from_scans(std::slice::from_ref(&prescan)).unwrap();
        fs::write(
            &path,
            r#"{"type":"user","sessionId":"mutable","uuid":"after","message":{"role":"user","content":"after"}}"#,
        )
        .unwrap();
        let snapshot = archive_source::freeze_file(&path).unwrap();
        let mut emitted = 0usize;
        let error = project_snapshot(&path, snapshot, &prescan, &plan, None, &mut |_| {
            emitted += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(emitted, 0);
        assert!(format!("{error:#}").contains("changed between dependency pre-scan"));
    }

    #[test]
    fn source_plan_unions_duplicate_parent_evidence_and_normalizes_exact_turn_telemetry() {
        // Reduced, field-faithful form of the only record-level cycle found in
        // the complete 2026-08-08 corpus census. S is transparent turn-duration
        // telemetry. One A receipt reports S while a second reports the real
        // semantic predecessor P; S reciprocally reports A. The canonical
        // future is U -> P -> A -> N, independent of receipt order/history.
        let source = concat!(
            r#"{"type":"user","sessionId":"turn","uuid":"U","parentUuid":null,"message":{"role":"user","content":"before"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"turn","uuid":"P","parentUuid":"U","message":{"role":"assistant","content":[{"type":"thinking","thinking":"thought"}]}}"#,
            "\n",
            r#"{"type":"system","subtype":"turn_duration","sessionId":"turn","uuid":"S","parentUuid":"A","durationMs":36}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"turn","uuid":"A","parentUuid":"S","message":{"role":"assistant","content":"answer"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"turn","uuid":"A","parentUuid":"P","message":{"role":"assistant","content":"answer"}}"#,
            "\n",
            r#"{"type":"user","sessionId":"turn","uuid":"N","parentUuid":"A","message":{"role":"user","content":"next"}}"#,
        );
        let prescan = prescan_text(source, Path::new("turn-duration.jsonl"));
        let plan = SourcePlan::from_scans(&[prescan]).unwrap();
        let u = SourceKey::new("turn", "U");
        let p = SourceKey::new("turn", "P");
        let s = SourceKey::new("turn", "S");
        let a = SourceKey::new("turn", "A");
        let n = SourceKey::new("turn", "N");
        assert_eq!(plan.parents[&u], BTreeSet::new());
        assert_eq!(plan.parents[&p], BTreeSet::from([u]));
        assert_eq!(plan.parents[&a], BTreeSet::from([p]));
        assert_eq!(plan.parents[&n], BTreeSet::from([a]));
        assert!(!plan.nodes.contains_key(&s));

        let projected = project_text(source, Path::new("turn-duration.jsonl"));
        assert_eq!(projected.stats.source_projections, 4);
        assert_eq!(projected.stats.skipped_records, 1);
        assert_eq!(projected.stats.unresolved_parents, 0);
        let thought = block_with_payload(&projected.fragment, "thought");
        let answer = block_with_payload(&projected.fragment, "answer");
        let next = block_with_payload(&projected.fragment, "next");
        assert!(exists!(pattern!(&projected.fragment, [{
            answer @ schema::block::previous: &thought
        }])));
        assert!(exists!(pattern!(&projected.fragment, [{
            next @ schema::block::previous: &answer
        }])));
        let answer_receipts: BTreeSet<_> = find!(
            projection: Id,
            pattern!(&projected.fragment, [{
                ?projection @ schema::source_projection::projects_to: &answer
            }])
        )
        .collect();
        assert_eq!(
            answer_receipts.len(),
            1,
            "both occurrences of the answer land on one receipt"
        );
    }

    #[test]
    fn fork_alias_quotient_preserves_tool_chronology_not_replay_serialization() {
        // Reduced, field-faithful form of the d00f... origin / 77fe...
        // destination replay that exposed the fork cycle. Fork receipts are
        // exact evidence for their origin class, but their destination parent
        // links are serialization artifacts. The later native replay restores
        // the semantic B -> A -> U chronology even though U appears first.
        let source = concat!(
            r#"{"type":"user","sessionId":"d00f65db-0741-41e1-aa27-420e1423e24b","uuid":"2b46fe81-12d5-4291-a494-40b7130cf37e","parentUuid":null,"message":{"role":"user","content":"request"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"d00f65db-0741-41e1-aa27-420e1423e24b","uuid":"1fdd8a4d-15f2-4915-a4ab-0630bfbbaf76","parentUuid":"2b46fe81-12d5-4291-a494-40b7130cf37e","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_exemplar","name":"Read","input":{"file":"x"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"d00f65db-0741-41e1-aa27-420e1423e24b","uuid":"353e151f-7443-40f6-9dc1-7736b5f02db8","parentUuid":"1fdd8a4d-15f2-4915-a4ab-0630bfbbaf76","sourceToolAssistantUUID":"1fdd8a4d-15f2-4915-a4ab-0630bfbbaf76","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_exemplar","content":"result"}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"77fe8720-6f5b-42d9-8462-a3edd3445af9","uuid":"2b46fe81-12d5-4291-a494-40b7130cf37e","parentUuid":null,"forkedFrom":{"sessionId":"d00f65db-0741-41e1-aa27-420e1423e24b","messageUuid":"2b46fe81-12d5-4291-a494-40b7130cf37e"},"message":{"role":"user","content":"request"}}"#,
            "\n",
            // Replay serialization reverses U/A here; neither edge is a
            // semantic parent assertion because both receipts are forked.
            r#"{"type":"user","sessionId":"77fe8720-6f5b-42d9-8462-a3edd3445af9","uuid":"353e151f-7443-40f6-9dc1-7736b5f02db8","parentUuid":"2b46fe81-12d5-4291-a494-40b7130cf37e","sourceToolAssistantUUID":"1fdd8a4d-15f2-4915-a4ab-0630bfbbaf76","forkedFrom":{"sessionId":"d00f65db-0741-41e1-aa27-420e1423e24b","messageUuid":"353e151f-7443-40f6-9dc1-7736b5f02db8"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_exemplar","content":"result"}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"77fe8720-6f5b-42d9-8462-a3edd3445af9","uuid":"1fdd8a4d-15f2-4915-a4ab-0630bfbbaf76","parentUuid":"353e151f-7443-40f6-9dc1-7736b5f02db8","forkedFrom":{"sessionId":"d00f65db-0741-41e1-aa27-420e1423e24b","messageUuid":"1fdd8a4d-15f2-4915-a4ab-0630bfbbaf76"},"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_exemplar","name":"Read","input":{"file":"x"}}]}}"#,
            "\n",
            // Native replay order in the file is U then A, but its assertions
            // are A -> U and B -> A and therefore form the true DAG.
            r#"{"type":"user","sessionId":"77fe8720-6f5b-42d9-8462-a3edd3445af9","uuid":"353e151f-7443-40f6-9dc1-7736b5f02db8","parentUuid":"1fdd8a4d-15f2-4915-a4ab-0630bfbbaf76","sourceToolAssistantUUID":"1fdd8a4d-15f2-4915-a4ab-0630bfbbaf76","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_exemplar","content":"result"}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"77fe8720-6f5b-42d9-8462-a3edd3445af9","uuid":"1fdd8a4d-15f2-4915-a4ab-0630bfbbaf76","parentUuid":"2b46fe81-12d5-4291-a494-40b7130cf37e","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_exemplar","name":"Read","input":{"file":"x"}}]}}"#,
            "\n",
            // The next message shows the call's message complete.
            r#"{"type":"assistant","sessionId":"77fe8720-6f5b-42d9-8462-a3edd3445af9","uuid":"closing","parentUuid":"353e151f-7443-40f6-9dc1-7736b5f02db8","message":{"role":"assistant","content":[{"type":"text","text":"closed"}]}}"#,
        );

        let projected = project_text(source, Path::new("fork-exemplar.jsonl"));
        assert_eq!(projected.stats.unresolved_tool_results, 0);
        let request = block_with_payload(&projected.fragment, "request");
        let call = block_with_payload(
            &projected.fragment,
            r#"{"name":"Read","input":{"file":"x"}}"#,
        );
        let result = block_with_payload(&projected.fragment, "result");
        assert!(exists!(pattern!(&projected.fragment, [{
            call @ schema::block::previous: &request
        }])));
        assert!(exists!(pattern!(&projected.fragment, [{
            result @ schema::block::previous: &call
        }])));
        assert!(!exists!(pattern!(&projected.fragment, [{
            call @ schema::block::previous: &result
        }])));

        // The origin, the fork's replay and the fork's native copy are one
        // message: one block and one receipt each.
        assert_eq!(projections_for_block(&projected.fragment, call).len(), 1);
        assert_eq!(projections_for_block(&projected.fragment, result).len(), 1);
    }

    #[test]
    fn fork_origin_message_uuid_need_not_equal_the_destination_uuid() {
        let source = concat!(
            r#"{"type":"user","sessionId":"origin","uuid":"origin-message","message":{"role":"user","content":"same payload"}}"#,
            "\n",
            r#"{"type":"user","sessionId":"destination","uuid":"copy-message","forkedFrom":{"sessionId":"origin","messageUuid":"origin-message"},"message":{"role":"user","content":"same payload"}}"#,
        );
        let scan = prescan_text(source, Path::new("different-uuid.jsonl"));
        let plan = SourcePlan::from_scans(&[scan]).unwrap();
        let origin = SourceKey::new("origin", "origin-message");
        let copy = SourceKey::new("destination", "copy-message");
        assert_eq!(plan.canonical_key(&origin), plan.canonical_key(&copy));

        let projected = project_text(source, Path::new("different-uuid.jsonl"));
        let block = block_with_payload(&projected.fragment, "same payload");
        assert_eq!(projections_for_block(&projected.fragment, block).len(), 1);
    }

    #[test]
    fn one_key_in_two_files_with_different_payloads_fails_before_emission() {
        // A further version is told apart inside one file, by what it says
        // and where it is written; two files that each claim one session
        // record with different payloads stay a conflict.
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("one.jsonl"),
            r#"{"type":"user","sessionId":"shared","uuid":"message","message":{"role":"user","content":"one payload"}}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("two.jsonl"),
            r#"{"type":"user","sessionId":"shared","uuid":"message","message":{"role":"user","content":"another payload"}}"#,
        )
        .unwrap();
        let mut emitted = 0;
        let error = project_path(directory.path(), |_| {
            emitted += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(emitted, 0);
        assert!(format!("{error:#}").contains("alias class"));
        assert!(format!("{error:#}").contains("conflicting semantic payloads"));
    }

    #[test]
    fn alias_representative_and_blocks_are_independent_of_scan_order() {
        let origin = prescan_text(
            r#"{"type":"user","sessionId":"z-origin","uuid":"message","message":{"role":"user","content":"stable"}}"#,
            Path::new("origin.jsonl"),
        );
        let fork = prescan_text(
            r#"{"type":"user","sessionId":"a-fork","uuid":"copy","forkedFrom":{"sessionId":"z-origin","messageUuid":"message"},"message":{"role":"user","content":"stable"}}"#,
            Path::new("fork.jsonl"),
        );
        let forward = SourcePlan::from_scans(&[origin, fork]).unwrap();

        let fork = prescan_text(
            r#"{"type":"user","sessionId":"a-fork","uuid":"copy","forkedFrom":{"sessionId":"z-origin","messageUuid":"message"},"message":{"role":"user","content":"stable"}}"#,
            Path::new("fork.jsonl"),
        );
        let origin = prescan_text(
            r#"{"type":"user","sessionId":"z-origin","uuid":"message","message":{"role":"user","content":"stable"}}"#,
            Path::new("origin.jsonl"),
        );
        let reverse = SourcePlan::from_scans(&[fork, origin]).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(
            forward.canonical_key(&SourceKey::new("z-origin", "message")),
            SourceKey::new("a-fork", "copy")
        );
    }

    #[test]
    fn fourteen_native_bridge_references_resolve_through_fork_only_keys() {
        // The full corpus has exactly fourteen native parent assertions whose
        // destination-session key is defined only by a fork receipt. Freeze
        // that cardinality here so a future "forks never provide identities"
        // shortcut cannot silently reintroduce the missing bridges.
        let mut source = String::new();
        for index in 0..14 {
            let origin_session = format!("origin-{index:02}");
            let destination_session = format!("destination-{index:02}");
            let provider = format!("provider-{index:02}");
            let child = format!("child-{index:02}");
            let payload = format!("provider payload {index:02}");
            let child_payload = format!("child payload {index:02}");
            for line in [
                format!(
                    r#"{{"type":"user","sessionId":"{origin_session}","uuid":"{provider}","message":{{"role":"user","content":"{payload}"}}}}"#
                ),
                format!(
                    r#"{{"type":"user","sessionId":"{destination_session}","uuid":"{provider}","forkedFrom":{{"sessionId":"{origin_session}","messageUuid":"{provider}"}},"message":{{"role":"user","content":"{payload}"}}}}"#
                ),
                format!(
                    r#"{{"type":"assistant","sessionId":"{destination_session}","uuid":"{child}","parentUuid":"{provider}","message":{{"role":"assistant","content":"{child_payload}"}}}}"#
                ),
            ] {
                if !source.is_empty() {
                    source.push('\n');
                }
                source.push_str(&line);
            }
        }

        let projected = project_text(&source, Path::new("fourteen-bridges.jsonl"));
        assert_eq!(projected.stats.unresolved_parents, 0);
        for index in 0..14 {
            let provider =
                block_with_payload(&projected.fragment, &format!("provider payload {index:02}"));
            let child =
                block_with_payload(&projected.fragment, &format!("child payload {index:02}"));
            assert!(exists!(pattern!(&projected.fragment, [{
                child @ schema::block::previous: &provider
            }])));
        }
    }

    #[test]
    fn true_semantic_cycle_is_rejected_before_emission() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("cycle.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"cycle","uuid":"a","parentUuid":"b","message":{"role":"user","content":"a"}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"cycle","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":"b"}}"#,
            ),
        )
        .unwrap();
        let mut emitted = 0;
        let error = project_path(directory.path(), |_| {
            emitted += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(emitted, 0);
        assert!(format!("{error:#}").contains("semantic Claude Code source cycle"));
    }

    #[test]
    fn tool_semantics_include_canonical_name_input_and_tristate_result_status() {
        let bash_a = project_text(
            r#"{"type":"assistant","sessionId":"tools","uuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_a","name":"Bash","input":{"z":1,"a":2}}]}}"#,
            Path::new("bash-a.jsonl"),
        );
        let bash_b = project_text(
            r#"{"type":"assistant","sessionId":"tools","uuid":"b","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_b","name":"Bash","input":{"a":2,"z":1}}]}}"#,
            Path::new("bash-b.jsonl"),
        );
        let read = project_text(
            r#"{"type":"assistant","sessionId":"tools","uuid":"c","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_c","name":"Read","input":{"a":2,"z":1}}]}}"#,
            Path::new("read.jsonl"),
        );
        let bash_a_block =
            block_with_payload(&bash_a.fragment, r#"{"name":"Bash","input":{"a":2,"z":1}}"#);
        let bash_b_block =
            block_with_payload(&bash_b.fragment, r#"{"name":"Bash","input":{"a":2,"z":1}}"#);
        let read_block =
            block_with_payload(&read.fragment, r#"{"name":"Read","input":{"a":2,"z":1}}"#);
        assert_eq!(bash_a_block, bash_b_block);
        assert_ne!(bash_a_block, read_block);
        assert_eq!(
            projection_for_block(&bash_a.fragment, bash_a_block),
            projection_for_block(&bash_b.fragment, bash_b_block),
            "vendor correlators are never identity"
        );

        let missing = project_text(
            r#"{"type":"user","sessionId":"tools","uuid":"missing","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"unknown","content":"same output"}]}}"#,
            Path::new("missing.jsonl"),
        );
        let ok = project_text(
            r#"{"type":"user","sessionId":"tools","uuid":"ok","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"unknown","content":"same output","is_error":false}]}}"#,
            Path::new("ok.jsonl"),
        );
        let error = project_text(
            r#"{"type":"user","sessionId":"tools","uuid":"error","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"unknown","content":"same output","is_error":true}]}}"#,
            Path::new("error.jsonl"),
        );
        let missing_block = block_with_payload(&missing.fragment, "same output");
        let ok_block = block_with_payload(&ok.fragment, "same output");
        let error_block = block_with_payload(&error.fragment, "same output");
        assert_ne!(missing_block, ok_block);
        assert_ne!(ok_block, error_block);
        assert_ne!(missing_block, error_block);
        // The status leads the result: the output's node follows it.
        let ok_status = block_with_payload(&ok.fragment, TOOL_RESULT_STATUS_OK);
        let error_status = block_with_payload(&error.fragment, TOOL_RESULT_STATUS_ERROR);
        assert!(exists!(pattern!(&ok.fragment, [{
            ok_block @ schema::block::previous: ok_status
        }])));
        assert!(exists!(pattern!(&error.fragment, [{
            error_block @ schema::block::previous: error_status
        }])));
        let (_empty, reader) = empty_reader();
        let (_, validation) =
            blockdag::validate_catalog_union(&reader, &TribleSet::new(), &ok.fragment).unwrap();
        assert_eq!(validation, blockdag::CatalogValidation::Accepted);

        let linked = project_text(
            concat!(
                r#"{"type":"assistant","sessionId":"linked","uuid":"call","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_linked","name":"Bash","input":{"command":"true"}}]}}"#,
                "\n",
                r#"{"type":"user","sessionId":"linked","uuid":"result","parentUuid":"call","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_linked","content":"done","is_error":false}]}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"linked","uuid":"closing","parentUuid":"result","message":{"role":"assistant","content":[{"type":"text","text":"closed"}]}}"#,
            ),
            Path::new("linked.jsonl"),
        );
        let call_node = find!(
            node: Id,
            pattern!(linked.fragment.facts(), [
                { ?node @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::modality: &schema::content_fact::modality::TOOL_CALL },
            ])
        )
        .next()
        .unwrap();
        let response_modalities: BTreeSet<Id> = find!(
            modality: Id,
            pattern!(linked.fragment.facts(), [
                { _?node @ schema::block::responds_to: &call_node,
                    schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::modality: ?modality },
            ])
        )
        .collect();
        assert_eq!(
            response_modalities,
            BTreeSet::from([
                schema::content_fact::modality::EVENT,
                schema::content_fact::modality::TOOL_RESULT,
            ])
        );
    }

    #[test]
    fn canonical_tool_json_sorts_nested_objects_and_escapes_decoded_strings() {
        let input = Bytes::from_source(
            br#"{ "z":{"quote":"a\"b","line":"x\ny"},"a":[{"two":2,"one":1}],"dup":0,"dup":3}"#
                .to_vec(),
        );
        let canonical = canonical_tool_call(Some("say \"hi\"\n".to_owned()), Some(input)).unwrap();
        assert_eq!(
            canonical,
            "{\"name\":\"say \\\"hi\\\"\\n\",\"input\":{\"a\":[{\"one\":1,\"two\":2}],\"dup\":3,\"z\":{\"line\":\"x\\ny\",\"quote\":\"a\\\"b\"}}}"
        );
    }

    #[test]
    fn canonical_json_preserves_historical_number_spelling_at_boundaries() {
        for (raw, expected) in [
            ("-0", "-0.0"),
            ("-0.0", "-0.0"),
            ("0.0", "0.0"),
            ("1e2", "100.0"),
            ("1e-2", "0.01"),
            ("9223372036854775807", "9223372036854775807"),
            ("9223372036854775808", "9223372036854775808"),
            ("18446744073709551615", "18446744073709551615"),
            ("18446744073709551616", "1.8446744073709552e+19"),
            ("-9223372036854775808", "-9223372036854775808"),
            ("-9223372036854775809", "-9.223372036854776e+18"),
        ] {
            let actual =
                archive_source::canonical_json(Bytes::from_source(raw.as_bytes().to_vec()))
                    .unwrap();
            assert_eq!(actual, expected, "canonical spelling for {raw}");
        }
        assert!(archive_source::canonical_json(Bytes::from_source(b"1e400".to_vec())).is_err());
    }

    #[test]
    fn image_size_alias_priority_is_order_independent_and_null_authoritative() {
        fn size(source: &str) -> Option<u128> {
            let mut source = Bytes::from_source(source.as_bytes().to_vec());
            scan_optional_image_source(&mut source)
                .unwrap()
                .unwrap()
                .size
        }

        assert_eq!(size(r#"{"file_size":7,"size":99}"#), Some(7));
        assert_eq!(size(r#"{"size":99,"file_size":7}"#), Some(7));
        assert_eq!(size(r#"{"file_size":null,"size":99}"#), None);
        assert_eq!(size(r#"{"size":99,"file_size":null}"#), None);
        assert_eq!(size(r#"{"size":99}"#), Some(99));
    }

    #[test]
    fn explicit_tool_source_resolves_a_call_that_appears_later_in_the_file() {
        let projected = project_text(
            concat!(
                r#"{"type":"user","sessionId":"forward-tool","uuid":"result","parentUuid":"call","sourceToolAssistantUUID":"call","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_forward","content":"forward result"}]}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"forward-tool","uuid":"call","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_forward","name":"Read","input":{"file":"later"}}]}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"forward-tool","uuid":"closing","parentUuid":"result","message":{"role":"assistant","content":[{"type":"text","text":"closed"}]}}"#,
            ),
            Path::new("forward-tool.jsonl"),
        );
        assert_eq!(projected.stats.unresolved_tool_results, 0);
        let call_node = find!(
            node: Id,
            pattern!(projected.fragment.facts(), [
                { ?node @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::modality: &schema::content_fact::modality::TOOL_CALL },
            ])
        )
        .next()
        .unwrap();
        assert!(exists!(pattern!(projected.fragment.facts(), [
            { _?node @ schema::block::responds_to: &call_node,
                schema::block::holds: _?fact },
            { _?fact @ schema::content_fact::modality: &schema::content_fact::modality::TOOL_RESULT },
        ])));
    }

    #[test]
    fn alias_equivalent_tool_sources_do_not_conflict_before_resolution() {
        let source = concat!(
            r#"{"type":"assistant","sessionId":"alias-tools","uuid":"call-origin","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_alias","name":"Read","input":{"file":"x"}}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"alias-tools","uuid":"call-copy","forkedFrom":{"sessionId":"alias-tools","messageUuid":"call-origin"},"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_alias","name":"Read","input":{"file":"x"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"alias-tools","uuid":"result","sourceToolAssistantUUID":"call-origin","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_alias","content":"same result"}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"alias-tools","uuid":"result","sourceToolAssistantUUID":"call-copy","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_alias","content":"same result"}]}}"#,
        );
        let projected = project_text(source, Path::new("alias-tool-sources.jsonl"));
        assert_eq!(projected.stats.unresolved_tool_results, 0);
        let result = block_with_payload(&projected.fragment, "same result");
        assert_eq!(projections_for_block(&projected.fragment, result).len(), 1);
    }

    #[test]
    fn implicit_tool_resolution_is_stable_under_unrelated_sessions() {
        let base = concat!(
            r#"{"type":"assistant","sessionId":"local","uuid":"call","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_reused","name":"Read","input":{"file":"local"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"local","uuid":"result","parentUuid":"call","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_reused","content":"local result"}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"local","uuid":"closing","parentUuid":"result","message":{"role":"assistant","content":[{"type":"text","text":"closed"}]}}"#,
        );
        let extended = format!(
            "{base}\n{}",
            r#"{"type":"assistant","sessionId":"unrelated","uuid":"call","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_reused","name":"Read","input":{"file":"unrelated"}}]}}"#,
        );
        let base = project_text(base, Path::new("base.jsonl"));
        let extended = project_text(&extended, Path::new("extended.jsonl"));
        assert_eq!(base.stats.unresolved_tool_results, 0);
        assert_eq!(extended.stats.unresolved_tool_results, 0);
        assert_eq!(
            block_with_payload(&base.fragment, "local result"),
            block_with_payload(&extended.fragment, "local result"),
            "an unrelated session cannot retract an intrinsic tool edge"
        );
    }

    #[test]
    fn conflicting_implicit_tool_definition_rejects_the_enlarged_import() {
        let source = concat!(
            r#"{"type":"assistant","sessionId":"conflicting-tools","uuid":"one","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_same","name":"Read","input":{"file":"one"}}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"conflicting-tools","uuid":"two","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_same","name":"Read","input":{"file":"two"}}]}}"#,
        );
        let scan = prescan_text(source, Path::new("conflicting-tools.jsonl"));
        let error = SourcePlan::from_scans(&[scan]).unwrap_err();
        assert!(format!("{error:#}").contains("conflicting calls in raw session"));
    }

    #[test]
    fn corpus_plan_resolves_cross_file_support_independent_of_path_order() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("00-child.jsonl");
        let parent = directory.path().join("99-parent.jsonl");
        fs::write(
            &parent,
            r#"{"type":"user","sessionId":"cross","uuid":"root","parentUuid":null,"timestamp":"2026-03-01T15:34:01Z","message":{"role":"user","content":"parent payload"}}"#,
        )
        .unwrap();
        fs::write(
            &child,
            r#"{"type":"assistant","sessionId":"cross","uuid":"child","parentUuid":"root","timestamp":"2026-03-01T15:34:02Z","message":{"role":"assistant","model":"claude","content":[{"type":"text","text":"child payload"}]}}"#,
        )
        .unwrap();

        let mut union = Fragment::empty();
        project_path(directory.path(), |projected| {
            union += projected.fragment;
            Ok(())
        })
        .unwrap();

        let parent_id = block_with_payload(&union, "parent payload");
        let child_id = block_with_payload(&union, "child payload");
        assert!(exists!(pattern!(&union, [{
            child_id @ schema::block::previous: &parent_id
        }])));
        let (_empty, reader) = empty_reader();
        let (_, validation) =
            blockdag::validate_catalog_union(&reader, &TribleSet::new(), &union).unwrap();
        assert_eq!(validation, blockdag::CatalogValidation::Accepted);
    }

    #[test]
    fn absent_reference_does_not_strand_a_files_resolvable_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("00-child.jsonl");
        let provider = directory.path().join("99-provider.jsonl");
        fs::write(
            &provider,
            r#"{"type":"user","sessionId":"mixed","uuid":"provider","parentUuid":"absent","message":{"role":"user","content":"partially rooted provider"}}"#,
        )
        .unwrap();
        fs::write(
            &child,
            r#"{"type":"assistant","sessionId":"mixed","uuid":"child","parentUuid":"provider","message":{"role":"assistant","content":"dependent child"}}"#,
        )
        .unwrap();

        let mut union = Fragment::empty();
        let summary = project_path(directory.path(), |projected| {
            union += projected.fragment;
            Ok(())
        })
        .unwrap();
        assert_eq!(summary.stats.unresolved_parents, 1);
        let provider = block_with_payload(&union, "partially rooted provider");
        let child = block_with_payload(&union, "dependent child");
        assert!(exists!(pattern!(&union, [{
            child @ schema::block::previous: &provider
        }])));
    }

    #[test]
    fn transparent_source_chain_resolves_across_reversed_file_order() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("00-child.jsonl"),
            r#"{"type":"assistant","sessionId":"transparent-files","uuid":"child","parentUuid":"system","message":{"role":"assistant","content":"file child"}}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("33-system.jsonl"),
            r#"{"type":"system","sessionId":"transparent-files","uuid":"system","parentUuid":"attachment","content":"metadata"}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("66-attachment.jsonl"),
            r#"{"type":"attachment","sessionId":"transparent-files","uuid":"attachment","parentUuid":"root","attachment":{"type":"skill_listing"}}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("99-root.jsonl"),
            r#"{"type":"user","sessionId":"transparent-files","uuid":"root","parentUuid":null,"message":{"role":"user","content":"file root"}}"#,
        )
        .unwrap();

        let mut union = Fragment::empty();
        let summary = project_path(directory.path(), |projected| {
            union += projected.fragment;
            Ok(())
        })
        .unwrap();
        assert_eq!(summary.stats.unresolved_parents, 0);
        // No file holds the root, the attachment and the system record
        // together, so each is emitted whole by its own file, in the chain.
        let root = block_with_payload(&union, "file root");
        let attachment = block_with_payload(&union, r#"{"attachment":{"type":"skill_listing"}}"#);
        let system = block_with_payload(&union, r#"{"content":"metadata"}"#);
        let child = block_with_payload(&union, "file child");
        assert!(exists!(pattern!(&union, [
            { attachment @ schema::block::previous: &root },
            { system @ schema::block::previous: &attachment },
            { child @ schema::block::previous: &system },
        ])));
        let (_empty, reader) = empty_reader();
        let (_, validation) =
            blockdag::validate_catalog_union(&reader, &TribleSet::new(), &union).unwrap();
        assert_eq!(validation, blockdag::CatalogValidation::Accepted);
    }

    #[test]
    fn dead_progress_backlink_does_not_create_a_file_dependency_cycle() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("00-agent.jsonl"),
            r#"{"type":"assistant","sessionId":"live-dag","uuid":"agent","parentUuid":"root","message":{"role":"assistant","content":"agent reply"}}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("99-main.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"live-dag","uuid":"root","parentUuid":null,"message":{"role":"user","content":"main root"}}"#,
                "\n",
                // Claude's progress telemetry may point from the main file to
                // an agent message even though it has no semantic descendant.
                // File adjacency is cyclic; the live semantic source DAG is
                // simply root -> agent.
                r#"{"type":"progress","sessionId":"live-dag","uuid":"progress","parentUuid":"agent","data":{"type":"agent_progress"}}"#,
            ),
        )
        .unwrap();

        let mut union = Fragment::empty();
        let summary = project_path(directory.path(), |projected| {
            union += projected.fragment;
            Ok(())
        })
        .unwrap();
        assert_eq!(summary.stats.skipped_records, 1);
        let root = block_with_payload(&union, "main root");
        let agent = block_with_payload(&union, "agent reply");
        assert!(exists!(pattern!(&union, [{
            agent @ schema::block::previous: &root
        }])));
    }

    #[test]
    fn file_level_cycle_does_not_obscure_a_valid_record_dag() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("00-a.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"cycle","uuid":"p","parentUuid":null,"message":{"role":"user","content":"root p"}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"cycle","uuid":"a1","parentUuid":"b1","message":{"role":"assistant","content":"after b1"}}"#,
            ),
        )
        .unwrap();
        fs::write(
            directory.path().join("99-b.jsonl"),
            r#"{"type":"user","sessionId":"cycle","uuid":"b1","parentUuid":"p","message":{"role":"user","content":"after p"}}"#,
        )
        .unwrap();

        let mut union = Fragment::empty();
        project_path(directory.path(), |projected| {
            union += projected.fragment;
            Ok(())
        })
        .unwrap();
        let p = block_with_payload(&union, "root p");
        let b = block_with_payload(&union, "after p");
        let a = block_with_payload(&union, "after b1");
        assert!(exists!(
            pattern!(&union, [{ b @ schema::block::previous: &p }])
        ));
        assert!(exists!(
            pattern!(&union, [{ a @ schema::block::previous: &b }])
        ));
    }

    #[test]
    fn duplicate_source_receipts_union_when_their_canonical_block_agrees() {
        let directory = tempfile::tempdir().unwrap();
        let parent_a = directory.path().join("parent-a.jsonl");
        let parent_b = directory.path().join("parent-b.jsonl");
        let child = directory.path().join("child.jsonl");
        fs::write(
            &parent_a,
            r#"{"type":"user","sessionId":"duplicate","uuid":"root","parentUuid":null,"message":{"role":"user","content":"same parent"}}"#,
        )
        .unwrap();
        // Exact source bytes differ, while their canonical block is identical.
        fs::write(
            &parent_b,
            r#"{ "uuid":"root", "sessionId":"duplicate", "parentUuid":null, "type":"user", "message":{"content":"same parent","role":"user"} }"#,
        )
        .unwrap();
        fs::write(
            &child,
            r#"{"type":"assistant","sessionId":"duplicate","uuid":"child","parentUuid":"root","message":{"role":"assistant","model":"claude","content":"after both receipts"}}"#,
        )
        .unwrap();

        let mut union = Fragment::empty();
        project_path(directory.path(), |projected| {
            union += projected.fragment;
            Ok(())
        })
        .unwrap();
        let parent = block_with_payload(&union, "same parent");
        let child_block = block_with_payload(&union, "after both receipts");
        assert!(exists!(pattern!(&union, [{
            child_block @ schema::block::previous: &parent
        }])));

        let (_empty, reader) = empty_reader();
        let (_, validation) =
            blockdag::validate_catalog_union(&reader, &TribleSet::new(), &union).unwrap();
        assert_eq!(validation, blockdag::CatalogValidation::Accepted);
    }

    #[test]
    fn conflicting_duplicate_source_is_rejected_before_its_fragment_is_emitted() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("a.jsonl"),
            r#"{"type":"user","sessionId":"conflict","uuid":"same","message":{"role":"user","content":"first"}}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("b.jsonl"),
            r#"{"type":"user","sessionId":"conflict","uuid":"same","message":{"role":"user","content":"second"}}"#,
        )
        .unwrap();
        let mut emitted = 0usize;
        let error = project_path(directory.path(), |_| {
            emitted += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(emitted, 0);
        assert!(format!("{error:#}").contains("conflicting semantic payloads"));
    }

    #[test]
    fn dialogue_without_vendor_identity_is_counted_not_threaded() {
        let source = r#"{"type":"user","message":{"role":"user","content":"anonymous"}}"#;
        let projected = project_text(source, Path::new("anonymous.jsonl"));
        let receipts = ids_with_tag(&projected.fragment, schema::source_projection::KIND);
        assert!(receipts.is_empty());
        assert_eq!(projected.stats.source_projections, 0);
        assert_eq!(projected.stats.missing_source_identity, 1);
        assert_eq!(projected.stats.skipped_records, 1);
    }
}
