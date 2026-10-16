//! Projection of one Codex app-server rollout onto Archive's canonical block
//! DAG.
//!
//! Codex writes several overlapping views of a turn. `event_msg` user and
//! agent messages are the visible dialogue stream, while `agent_reasoning`
//! carries usable model reasoning. `response_item/message` mirrors dialogue
//! while also carrying harness-only context. This adapter projects the useful
//! event stream exactly once.
//!
//! The event stream has two spellings. Rollouts up to 2026-08 carry one
//! `event_msg` per event: `user_message` (`message`, `images`,
//! `local_images`), `agent_message` (`message`) and `agent_reasoning` (`text`,
//! one record per reasoning summary part). Rollouts from 2026-09 put an
//! `ordinal` on every record and carry the same three events as
//! `event_msg/item_completed`, whose `item.type` is `UserMessage` (`content`:
//! `text` parts, plus `image`/`localImage` pointers in Codex's input
//! protocol), `AgentMessage` (`content`: `Text` parts) or `Reasoning`
//! (`summary_text`: the summary parts, one string each). Both spellings
//! project to the same content facts. `Reasoning.raw_content` mirrors the old
//! `agent_reasoning_raw_content` event, which was never projected either.
//!
//! A Codex fork or resume replays its parent's records into a new file under
//! new timestamps. A node is its predecessors and its content, and a receipt
//! is the source namespace and the node, so a replayed message lands on the
//! parent's node and receipt; the replayed copy's time is not kept. The
//! file's first `session_meta` is its own; a 2025 resume follows it with
//! the header of every ancestor under the ancestor's own id. Those ids are
//! parse-only and never refuse the file: the replay converges with each
//! ancestor's own rollout by structure.
//!
//! Every record with content sits in the main chain in file order, which is
//! the order the model experienced it: the dialogue events, tool calls and
//! their outputs, harness context (developer and user-role `response_item`
//! messages), inter-agent messages, compaction summaries and `world_state`,
//! the last five perceived (`in`). Each fact is one node after the one
//! before it; acts and perceptions are the runs of one direction along that
//! chain. A tool output's node names the call node it answers; call ids and
//! envelope times stay out of every hashed text.
//!
//! Sealed tokens are kept as the bytes they are: a reasoning item's
//! `encrypted_content` (a token only the provider can decrypt) as THINKING
//! blob output, and a compaction's sealed history as THINKING blob input.
//!
//! No source bytes are stored. Skipped, with the reason:
//! `token_count`, `token_usage_record`, `task_started`, `task_complete`,
//! `turn_aborted`, `thread_settings_applied`, `inter_agent_communication_metadata`
//! and legacy `record_type/state` are bookkeeping; the summary of a
//! `response_item/reasoning` in a file with events mirrors the
//! `agent_reasoning`/`Reasoning` events (its sealed token is kept);
//! assistant `response_item` messages in a file with events mirror
//! `agent_message`; `*_begin`, `*_end`, `item_completed` tool items
//! (`CommandExecution`, `FileChange`, `McpToolCall`, ...) and `patch_apply_end`
//! are UI mirrors of the model-visible call and output items; a compaction's
//! `replacement_history` repeats earlier items plus sealed tokens;
//! `ghost_snapshot` holds git object references; `session_meta` and
//! `turn_context` are session state (the session id names the leaf, the model
//! annotates receipts).
//!
//! A live rollout is planned at its last newline-terminated record; appends
//! are harmless and mutation of already observed bytes aborts before
//! `ArchiveImportWriter` can publish its COMMIT. In a rollout that writes
//! response items, a prompt's event at the tail whose response item is not
//! written yet is held back with what follows it, so a longer rollout only
//! appends nodes.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anybytes::{Bytes, View};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use hifitime::Epoch;
use memchr::{memchr_iter, memmem};
use triblespace::core::import::scanner as sc;
use triblespace::core::inline::encodings::time::NsTAIInterval;
use triblespace::core::inline::{Inline, TryToInline};
use triblespace::core::trible::Fragment;
use triblespace::prelude::*;

use serde_json::Value;

use crate::archive_source::{SourceClaims, SourcePart, SourceRecord, ToolLink};
use crate::schemas::blockdag as schema;
use crate::{archive_source, files};

/// Observable projection accounting. Non-dialogue records are contracted, not
/// silently mistaken for additional messages.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectionStats {
    /// Non-empty, newline-terminated JSONL records in the frozen prefix.
    pub records_seen: usize,
    /// Nodes emitted: one per fact, in the main chain.
    pub source_projections: usize,
    /// Ordered content parts emitted before set-level deduplication.
    pub content_parts: usize,
    /// Bookkeeping, mirrors and other rows that project to nothing.
    pub skipped_records: usize,
    /// Source timestamp strings that were present but not decodable.
    pub invalid_timestamps: usize,
    /// Data-URI assets whose bytes could not be decoded.
    pub undecodable_assets: usize,
    /// Tool outputs whose call the rollout does not hold.
    pub unresolved_tool_results: usize,
    /// User-message events dropped because a response item carries the same
    /// prompt (counted among `skipped_records` too).
    pub mirrored_prompts: usize,
    /// Records held back at the tail: a prompt's event whose response item is
    /// not yet written, and what follows it. They land on a later import.
    pub held_back_records: usize,
    /// The facts of those records: the nodes a later import adds for them.
    pub held_back_facts: usize,
}

/// One frozen rollout projection.
#[derive(Debug)]
pub struct ProjectedFile {
    pub source_path: PathBuf,
    pub fragment: Fragment,
    pub stats: ProjectionStats,
}

/// Result returned after the frozen source prefix reaches the sink.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectionSummary {
    pub files_scanned: usize,
    pub fragments_emitted: usize,
    pub frozen_bytes: u64,
    /// A final non-newline record retained exactly but deferred semantically.
    pub trailing_bytes_ignored: u64,
    pub stats: ProjectionStats,
}

#[derive(Clone, Debug)]
struct PrefixPlan {
    session_id: String,
    /// The file carries an `event_msg` stream, which is then the dialogue.
    has_events: bool,
    /// The legacy bare format: a header line, then bare response items.
    legacy: bool,
    /// Creation time (Unix ms) of a fork that opens by replaying its
    /// parent's records; `None` for a file that replays nothing.
    fork_start_ms: Option<i64>,
    observed_bytes: u64,
    complete_bytes: u64,
    trailing_bytes: u64,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug)]
struct LineScan {
    complete_bytes: u64,
    records_seen: usize,
}

struct Projector<'a> {
    source_path: &'a Path,
    has_events: bool,
    legacy: bool,
    current_model: Option<String>,
    /// Every record with content, in file order: the main chain, linked and
    /// linked by [`Projector::chain`].
    records: Vec<SourceRecord>,
    /// For each record, the prompt texts of a user-message event; `None`
    /// for every other record.
    event_prompts: Vec<Option<Vec<String>>>,
    /// Text the model was given in user-role response items.
    response_prompts: std::collections::HashSet<String>,
    /// The rollout writes response items at all: a prompt's event is then
    /// mirrored by the response item that carries it.
    response_items: bool,
    /// The rollout is known finished: nothing at its tail is held back.
    finished: bool,
    /// Inside a fork's replay of its parent: record times are the copy's,
    /// not the event's, and are not kept.
    replaying: bool,
    fork_start_ms: i64,
    last_ms: i64,
    stats: ProjectionStats,
}

#[derive(Default)]
struct CodexRecord {
    record_type: Option<View<str>>,
    timestamp: Option<View<str>>,
    payload: Option<CodexPayload>,
}

#[derive(Default)]
struct CodexPayload {
    payload_type: Option<View<str>>,
    id: Option<View<str>>,
    session_id: Option<View<str>>,
    model: Option<View<str>>,
    message: Option<View<str>>,
    text: Option<View<str>>,
    images: Vec<View<str>>,
    local_images: Vec<View<str>>,
    audio: Vec<View<str>>,
    local_audio: Vec<View<str>>,
    item: Option<CodexItem>,
}

/// The `item` of a 2026-09 `event_msg/item_completed` record.
#[derive(Default)]
struct CodexItem {
    item_type: Option<View<str>>,
    content: Vec<ItemContent>,
    summary_text: Vec<View<str>>,
}

/// One element of an item's `content` array.
#[derive(Default)]
struct ItemContent {
    content_type: Option<View<str>>,
    text: Option<View<str>>,
    image_url: Option<View<str>>,
    url: Option<View<str>>,
    path: Option<View<str>>,
}

/// Project one explicit Codex rollout file.
///
/// Recursive session-directory ingestion is intentionally absent: Codex child
/// rollouts replay large parent prefixes, and a normal local installation can
/// contain hundreds of gigabytes of them. Callers choose each lived stream
/// deliberately. Records are emitted as bounded fragments as soon as they are
/// projected from the frozen prefix, then the leaf annotation. Callers must
/// stage every emitted fragment and publish only after this function returns
/// successfully.
pub fn project_path<F>(path: &Path, emit: F) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    project_path_finished(path, None, emit)
}

/// [`project_path`], treating a rollout last modified before
/// `finished_before` as finished: a prompt's event at its tail is emitted
/// rather than held back, exactly as if a later record had shown it is no
/// mirror. The modification time only decides WHEN that event is emitted,
/// never what any node or receipt is: identity stays free of time.
pub fn project_path_finished<F>(
    path: &Path,
    finished_before: Option<std::time::SystemTime>,
    emit: F,
) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    if path.is_dir() {
        bail!(
            "Codex Archive import requires one explicit rollout JSONL file; recursive session-directory ingestion is intentionally unsupported"
        );
    }

    // Read before the records, so a write after it can only make the file
    // look unfinished.
    let finished = match finished_before {
        Some(cutoff) => {
            std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .with_context(|| format!("read the modification time of {}", path.display()))?
                < cutoff
        }
        None => false,
    };
    let plan = plan_prefix(path)?;
    let frozen = archive_source::freeze_prefix(path, plan.observed_bytes)?;
    if frozen.digest != plan.digest {
        bail!(
            "Codex rollout {} changed inside its planned prefix before projection",
            path.display()
        );
    }
    project_planned(path, plan, frozen, finished, emit)
}

fn project_planned<F>(
    path: &Path,
    plan: PrefixPlan,
    frozen: archive_source::FrozenSource,
    finished: bool,
    mut emit: F,
) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    let mut projector = Projector {
        source_path: path,
        has_events: plan.has_events,
        legacy: plan.legacy,
        current_model: None,
        records: Vec::new(),
        event_prompts: Vec::new(),
        response_prompts: Default::default(),
        response_items: false,
        finished,
        replaying: plan.fork_start_ms.is_some(),
        fork_start_ms: plan.fork_start_ms.unwrap_or(0),
        last_ms: plan.fork_start_ms.unwrap_or(0),
        stats: ProjectionStats::default(),
    };
    let projected = scan_frozen_prefix(&frozen.bytes, |line, raw| {
        projector.project_record(line, raw)
    })?;
    if projected.complete_bytes != plan.complete_bytes {
        bail!(
            "Codex rollout {} frozen-prefix length changed during projection",
            path.display()
        );
    }
    projector.stats.records_seen = projected.records_seen;

    // One whole node per fact; the session id and path
    // annotate the file's leaf only.
    let mut fragments_emitted = 0usize;
    let common = archive_source::project_records(
        schema::source_projection::SOURCE_CODEX,
        path,
        projector.chain(),
        archive_source::UnitAnnotations {
            source_locator: Some(format!("session/{}", plan.session_id)),
            name: None,
        },
        |projected| {
            fragments_emitted += 1;
            emit(ProjectedFile {
                source_path: path.to_path_buf(),
                fragment: projected.fragment,
                stats: ProjectionStats::default(),
            })
        },
    )?;
    let mut stats = projector.stats;
    stats.source_projections = common.projections_emitted;
    stats.content_parts = common.content_parts;
    stats.unresolved_tool_results = common.unresolved_tool_results;
    Ok(ProjectionSummary {
        files_scanned: 1,
        fragments_emitted,
        frozen_bytes: plan.observed_bytes,
        trailing_bytes_ignored: plan.trailing_bytes,
        stats,
    })
}

/// What the planning pass learns from the records of one rollout.
#[derive(Default)]
struct PlanScan {
    session_id: Option<String>,
    legacy_id: Option<String>,
    has_events: bool,
    first: bool,
    /// Leading `session_meta` records; a fork names its ancestors after itself.
    metas: usize,
    first_meta_ms: Option<i64>,
    forked: bool,
    /// Headers naming another session than the rollout's own: ancestors a
    /// resume or fork replays.
    ancestor_sessions: usize,
}

impl PlanScan {
    fn observe(&mut self, raw: &[u8], path: &Path) -> Result<()> {
        let first = !self.first;
        self.first = true;
        if contains(raw, b"\"event_msg\"") {
            self.has_events = true;
        }
        if first && !contains(raw, b"\"payload\"") {
            // The legacy bare format opens with a header naming the session.
            if let Ok(header) = serde_json::from_slice::<serde_json::Value>(raw) {
                self.legacy_id = header
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_owned);
            }
        }
        if !contains(raw, b"\"session_meta\"") {
            return Ok(());
        }
        let record = parse_record(Bytes::from_source(raw.to_vec()), path)?;
        if record.record_type.as_deref() != Some("session_meta") {
            return Ok(());
        }
        self.metas += 1;
        if self.metas == 1 {
            self.first_meta_ms = record
                .timestamp
                .as_deref()
                .and_then(parse_iso_timestamp)
                .map(epoch_ms);
            self.forked = serde_json::from_slice::<serde_json::Value>(raw)
                .ok()
                .and_then(|value| {
                    value
                        .get("payload")?
                        .get("forked_from_id")?
                        .as_str()
                        .map(|id| !id.trim().is_empty())
                })
                .unwrap_or(false);
        }
        let payload = record
            .payload
            .as_ref()
            .ok_or_else(|| anyhow!("Codex session_meta has no object field \"payload\""))?;
        let id = consistent_session_id(payload)?;
        // The first header is the rollout's own. A resume or fork written
        // before `session_id` existed (2025) names each ancestor after it
        // under the ancestor's own id and replays the ancestor's history;
        // provider ids are parse-only, so a later header never decides
        // identity: the replay converges with the ancestor's own rollout by
        // structure, and this rollout's continuation follows it.
        if self.session_id.is_none() {
            self.session_id = Some(id);
        } else if self.session_id.as_ref() != Some(&id) {
            self.ancestor_sessions += 1;
        }
        Ok(())
    }

    /// The session id, whether the file is in the legacy bare format,
    /// whether it carries events, and a replaying fork's creation time.
    fn finish(self, path: &Path) -> Result<(String, bool, bool, Option<i64>)> {
        let fork_start_ms = (self.forked || self.metas >= 2)
            .then_some(self.first_meta_ms)
            .flatten();
        match (self.session_id, self.legacy_id) {
            (Some(id), _) => Ok((id, false, self.has_events, fork_start_ms)),
            (None, Some(id)) => Ok((id, true, self.has_events, None)),
            (None, None) => bail!(
                "Codex rollout {} has no stable session_meta.payload.id/session_id or legacy header id in its complete prefix",
                path.display()
            ),
        }
    }
}

/// Project resident immutable rollout bytes. Like live-file imports, a trailing
/// non-newline record is retained exactly but deferred semantically. The source
/// name is provenance only; local media pointers are never opened.
pub fn project_bytes<F>(source_name: &str, bytes: Bytes, emit: F) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedFile) -> Result<()>,
{
    let path = Path::new(source_name);
    let mut planned = PlanScan::default();
    let scan = scan_frozen_prefix(&bytes, |_line, raw| planned.observe(raw.as_ref(), path))?;
    let (session_id, legacy, has_events, fork_start_ms) = planned.finish(path)?;
    let observed_bytes = u64::try_from(bytes.len()).context("resident Codex length exceeds u64")?;
    let digest = *blake3::hash(bytes.as_ref()).as_bytes();
    let plan = PrefixPlan {
        session_id,
        has_events,
        legacy,
        fork_start_ms,
        observed_bytes,
        complete_bytes: scan.complete_bytes,
        trailing_bytes: observed_bytes.saturating_sub(scan.complete_bytes),
        digest,
    };
    project_planned(
        path,
        plan,
        archive_source::FrozenSource { bytes, digest },
        false,
        emit,
    )
}

fn plan_prefix(path: &Path) -> Result<PrefixPlan> {
    let source_len = File::open(path)
        .with_context(|| format!("open Codex rollout {}", path.display()))?
        .metadata()
        .with_context(|| format!("stat Codex rollout {}", path.display()))?
        .len();
    let mut planned = PlanScan::default();
    let (scan, digest) = scan_prefix(path, source_len, |_line, raw| planned.observe(raw, path))?;
    let (session_id, legacy, has_events, fork_start_ms) = planned.finish(path)?;
    Ok(PrefixPlan {
        session_id,
        has_events,
        legacy,
        fork_start_ms,
        observed_bytes: source_len,
        complete_bytes: scan.complete_bytes,
        trailing_bytes: source_len.saturating_sub(scan.complete_bytes),
        digest,
    })
}

fn scan_prefix<F>(path: &Path, byte_limit: u64, mut visit: F) -> Result<(LineScan, [u8; 32])>
where
    F: FnMut(u64, &[u8]) -> Result<()>,
{
    let file =
        File::open(path).with_context(|| format!("open Codex rollout {}", path.display()))?;
    let actual_len = file
        .metadata()
        .with_context(|| format!("stat Codex rollout {}", path.display()))?
        .len();
    if actual_len < byte_limit {
        bail!(
            "Codex rollout {} shrank from frozen length {byte_limit} to {actual_len}",
            path.display()
        );
    }
    let mut reader = BufReader::new(file.take(byte_limit));
    let mut buffer = Vec::new();
    let mut line = 0u64;
    let mut complete_bytes = 0u64;
    let mut records_seen = 0usize;
    let mut digest = blake3::Hasher::new();
    loop {
        buffer.clear();
        let read = reader
            .read_until(b'\n', &mut buffer)
            .with_context(|| format!("read Codex rollout {}", path.display()))?;
        if read == 0 {
            break;
        }
        // Retain/hash the entire observed file. A final non-newline row is
        // exact source evidence even though it is not yet a semantic record.
        digest.update(&buffer);
        if !buffer.ends_with(b"\n") {
            break;
        }
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("Codex rollout has more than u64::MAX lines"))?;
        complete_bytes = complete_bytes
            .checked_add(u64::try_from(read).expect("one read length fits u64"))
            .ok_or_else(|| anyhow!("Codex rollout complete prefix exceeds u64 bytes"))?;
        let raw = &buffer[..buffer.len() - 1];
        if trim_ascii_whitespace(raw).is_empty() {
            continue;
        }
        records_seen += 1;
        visit(line, raw)?;
    }
    Ok((
        LineScan {
            complete_bytes,
            records_seen,
        },
        *digest.finalize().as_bytes(),
    ))
}

/// Scan the immutable snapshot without allocating a buffer for every JSONL
/// row. Each callback receives a zero-copy view backed by the frozen mapping.
fn scan_frozen_prefix<F>(frozen: &Bytes, mut visit: F) -> Result<LineScan>
where
    F: FnMut(u64, Bytes) -> Result<()>,
{
    let mut line = 0u64;
    let mut start = 0usize;
    let mut records_seen = 0usize;
    for newline in memchr_iter(b'\n', frozen.as_ref()) {
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("Codex rollout has more than u64::MAX lines"))?;
        let raw = frozen.slice(start..newline);
        start = newline + 1;
        if trim_ascii_whitespace(raw.as_ref()).is_empty() {
            continue;
        }
        records_seen += 1;
        visit(line, raw)?;
    }
    Ok(LineScan {
        complete_bytes: u64::try_from(start).expect("frozen complete prefix length fits u64"),
        records_seen,
    })
}

impl Projector<'_> {
    /// End a fork's replay at its own first turn: a turn that starts at or
    /// after the fork's creation, or a pause the bulk copy never makes.
    fn observe_replay(&mut self, raw: &[u8]) {
        if contains(raw, b"\"session_meta\"") {
            return;
        }
        let at = line_ms(raw);
        if let Some(at) = at {
            if at > self.last_ms + 3_000 {
                self.replaying = false;
                return;
            }
            self.last_ms = self.last_ms.max(at);
        }
        let turn_start = if contains(raw, b"\"task_started\"") || contains(raw, b"\"turn_context\"")
        {
            serde_json::from_slice::<Value>(raw).ok().and_then(|value| {
                let payload = value.get("payload")?;
                payload
                    .get("started_at")
                    .and_then(Value::as_i64)
                    .map(|seconds| seconds * 1_000)
                    .or_else(|| payload.get("turn_id")?.as_str().and_then(uuid_v7_ms))
            })
        } else {
            None
        };
        if turn_start.is_some_and(|start| start >= self.fork_start_ms - 1_000) {
            self.replaying = false;
        }
    }

    fn project_record(&mut self, _line: u64, raw: Bytes) -> Result<()> {
        if self.replaying {
            self.observe_replay(raw.as_ref());
        }
        if self.legacy {
            return self.project_item_line(raw);
        }
        if contains(raw.as_ref(), b"\"turn_context\"") {
            let record = parse_record(raw.clone(), self.source_path)?;
            if record.record_type.as_deref() == Some("turn_context") {
                self.current_model = record
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.model.as_deref())
                    .filter(|model| !model.trim().is_empty())
                    .map(ToString::to_string);
                self.stats.skipped_records += 1;
                return Ok(());
            }
        }

        if !contains(raw.as_ref(), b"\"event_msg\"") {
            return self.project_item_line(raw);
        }
        let record = parse_record(raw.clone(), self.source_path)?;
        if record.record_type.as_deref() != Some("event_msg") {
            return self.project_item_line(raw);
        }
        let payload = record
            .payload
            .as_ref()
            .ok_or_else(|| anyhow!("Codex event_msg has no object field \"payload\""))?;
        let (parts, raw_role) = match payload.payload_type.as_deref() {
            Some("user_message") => (
                project_message_parts(
                    payload,
                    schema::content_fact::direction::IN,
                    &mut self.stats,
                ),
                "user_message",
            ),
            Some("agent_message") => (
                project_message_parts(
                    payload,
                    schema::content_fact::direction::OUT,
                    &mut self.stats,
                ),
                "agent_message",
            ),
            Some("agent_reasoning") => (project_reasoning_parts(payload), "agent_reasoning"),
            Some("item_completed") => {
                let Some(item) = payload.item.as_ref() else {
                    self.stats.skipped_records += 1;
                    return Ok(());
                };
                match item.item_type.as_deref() {
                    Some("UserMessage") => (
                        project_item_message_parts(
                            item,
                            schema::content_fact::direction::IN,
                            &mut self.stats,
                        ),
                        "UserMessage",
                    ),
                    Some("AgentMessage") => (
                        project_item_message_parts(
                            item,
                            schema::content_fact::direction::OUT,
                            &mut self.stats,
                        ),
                        "AgentMessage",
                    ),
                    Some("Reasoning") => (project_item_reasoning_parts(item), "Reasoning"),
                    _ => {
                        self.stats.skipped_records += 1;
                        return Ok(());
                    }
                }
            }
            _ => {
                self.stats.skipped_records += 1;
                return Ok(());
            }
        };
        let timestamp = self.timestamp(record.timestamp.as_deref());
        let prompt = matches!(raw_role, "user_message" | "UserMessage").then(|| {
            parts
                .iter()
                .filter_map(|part| match part {
                    SourcePart::Text {
                        modality, value, ..
                    } if *modality == schema::content_fact::modality::TEXT => {
                        Some(value.as_ref().trim().to_owned())
                    }
                    _ => None,
                })
                .collect()
        });
        if self.push(parts, Vec::new(), raw_role, timestamp) {
            *self
                .event_prompts
                .last_mut()
                .expect("a pushed record has a prompt slot") = prompt;
        }
        Ok(())
    }

    fn timestamp(&mut self, value: Option<&str>) -> Option<Inline<NsTAIInterval>> {
        value
            .and_then(|value| match parse_iso_timestamp(value) {
                Some(timestamp) => Some(timestamp),
                None => {
                    self.stats.invalid_timestamps += 1;
                    None
                }
            })
            .and_then(epoch_interval)
    }

    /// Put one record with content on the main chain. Role and model ride
    /// with its receipt; its time is kept unless it is a fork's replayed
    /// copy, whose time records a tool copying lines rather than anything
    /// that happened. Time never enters the block, because Codex restamps
    /// replayed records. Returns whether the record had content.
    fn push(
        &mut self,
        parts: Vec<SourcePart>,
        links: Vec<(usize, ToolLink)>,
        raw_role: &str,
        timestamp: Option<Inline<NsTAIInterval>>,
    ) -> bool {
        if parts.is_empty() {
            self.stats.skipped_records += 1;
            return false;
        }
        let mut record = SourceRecord::new(
            archive_source::owned_text(format!("record/{}", self.records.len())),
            Vec::new(),
            parts,
        );
        record.links = links;
        record.claims = SourceClaims {
            timestamp: if self.replaying { None } else { timestamp },
            raw_role: Some(archive_source::owned_text(raw_role.to_owned())),
            raw_model: self.current_model.clone().map(archive_source::owned_text),
            ..SourceClaims::default()
        };
        self.records.push(record);
        self.event_prompts.push(None);
        true
    }

    /// The main chain: every record after the one before it.
    ///
    /// A user-message event is the UI's notice of a prompt; what the model was
    /// given is the user-role response item carrying the same text. The
    /// event is kept only where no response item carries its prompt (formats
    /// without response items), so a prompt is never perceived twice.
    ///
    /// In a rollout that writes response items, an event whose response item
    /// is not written yet, with nothing the model produced after it, is a
    /// live tail: whether the event is a mirror is not known yet. It is held
    /// back with everything after it, so a later import appends rather than
    /// leaving a dead end.
    fn chain(&mut self) -> Vec<SourceRecord> {
        let mut records = std::mem::take(&mut self.records);
        let prompts = std::mem::take(&mut self.event_prompts);
        let response_prompts = std::mem::take(&mut self.response_prompts);
        let mirrored = |texts: &Vec<String>| {
            !texts.is_empty() && texts.iter().all(|text| response_prompts.contains(text))
        };
        let produced_after = |index: usize| {
            records[index + 1..].iter().any(|record| {
                record
                    .parts
                    .iter()
                    .any(|part| part.direction() == schema::content_fact::direction::OUT)
            })
        };
        let held = (self.response_items && !self.finished)
            .then(|| {
                prompts.iter().enumerate().position(|(index, prompt)| {
                    prompt
                        .as_ref()
                        .is_some_and(|texts| !texts.is_empty() && !mirrored(texts))
                        && !produced_after(index)
                })
            })
            .flatten();
        if let Some(index) = held {
            for record in records.drain(index..) {
                self.stats.held_back_records += 1;
                self.stats.held_back_facts += record.parts.len();
            }
        }
        let mut chained: Vec<SourceRecord> = Vec::with_capacity(records.len());
        for (mut record, prompt) in records.into_iter().zip(prompts) {
            if let Some(texts) = prompt {
                if mirrored(&texts) {
                    self.stats.mirrored_prompts += 1;
                    self.stats.skipped_records += 1;
                    continue;
                }
            }
            record.predecessors = chained
                .last()
                .map(|previous| previous.locator.clone())
                .into_iter()
                .collect();
            chained.push(record);
        }
        chained
    }

    /// Project one non-event record: a response item, a compaction, a world
    /// state, or (legacy format) a bare response item.
    fn project_item_line(&mut self, raw: Bytes) -> Result<()> {
        let cheap = raw.as_ref();
        if !self.legacy
            && !contains(cheap, b"\"response_item\"")
            && !contains(cheap, b"\"compacted\"")
            && !contains(cheap, b"\"world_state\"")
        {
            self.stats.skipped_records += 1;
            return Ok(());
        }
        let value: serde_json::Value = serde_json::from_slice(cheap)
            .with_context(|| format!("parse Codex record in {}", self.source_path.display()))?;
        let (kind, payload) = if self.legacy {
            ("response_item", &value)
        } else {
            (
                value.get("type").and_then(Value::as_str).unwrap_or(""),
                value.get("payload").unwrap_or(&Value::Null),
            )
        };
        let timestamp = self.timestamp(
            value
                .get("timestamp")
                .and_then(Value::as_str)
                .or_else(|| payload.get("timestamp").and_then(Value::as_str)),
        );
        match kind {
            "response_item" => self.response_item(payload, timestamp),
            "compacted" => {
                // A compaction is what the model is shown in place of its
                // history: its summary and its sealed state are perceived.
                let mut parts = Vec::new();
                if let Some(message) = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.trim().is_empty())
                {
                    parts.push(text_part(
                        schema::content_fact::modality::TEXT,
                        schema::content_fact::direction::IN,
                        message,
                    ));
                }
                // The sealed compaction tokens; the rest of the replacement
                // history repeats items the file already holds.
                for token in payload
                    .get("replacement_history")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|item| item.get("encrypted_content")?.as_str())
                    .filter(|token| !token.is_empty())
                {
                    parts.push(sealed(
                        token,
                        schema::content_fact::sealed::OPENAI_COMPACTION,
                        schema::content_fact::direction::IN,
                    ));
                }
                self.push(parts, Vec::new(), "compacted", timestamp);
                Ok(())
            }
            "world_state" => {
                let parts = event_parts(payload, schema::content_fact::direction::IN)?;
                self.push(parts, Vec::new(), "world_state", timestamp);
                Ok(())
            }
            _ => {
                self.stats.skipped_records += 1;
                Ok(())
            }
        }
    }

    fn response_item(
        &mut self,
        payload: &Value,
        timestamp: Option<Inline<NsTAIInterval>>,
    ) -> Result<()> {
        self.response_items = true;
        let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "message" => {
                let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                let direction = if role == "assistant" {
                    schema::content_fact::direction::OUT
                } else {
                    schema::content_fact::direction::IN
                };
                if self.has_events && role == "assistant" {
                    // The event stream carries the assistant's words.
                    self.stats.skipped_records += 1;
                    return Ok(());
                }
                // User-role items are the typed prompt and the harness
                // context around it; developer and system items are
                // instructions. All of it is what the model saw.
                if role == "user" {
                    for entry in payload
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if let Some(text) = entry.get("text").and_then(Value::as_str) {
                            self.response_prompts.insert(text.trim().to_owned());
                        }
                    }
                }
                let parts = response_content_parts(
                    payload.get("content"),
                    schema::content_fact::modality::TEXT,
                    direction,
                    &mut self.stats,
                )?;
                self.push(parts, Vec::new(), role, timestamp);
                Ok(())
            }
            "reasoning" => {
                // Readable reasoning, then the sealed token: one act with the
                // rest of the model's response.
                let mut parts = Vec::new();
                if !self.has_events {
                    // In a file with events the summary is the events'
                    // reasoning; only the sealed token is new here.
                    for field in ["summary", "content"] {
                        for entry in payload
                            .get(field)
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            let Some(text) = entry
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|text| !text.trim().is_empty())
                            else {
                                continue;
                            };
                            parts.push(text_part(
                                schema::content_fact::modality::THINKING,
                                schema::content_fact::direction::OUT,
                                text,
                            ));
                        }
                    }
                }
                if let Some(token) = payload
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|token| !token.is_empty())
                {
                    parts.push(sealed(
                        token,
                        schema::content_fact::sealed::OPENAI_REASONING,
                        schema::content_fact::direction::OUT,
                    ));
                }
                self.push(parts, Vec::new(), "reasoning", timestamp);
                Ok(())
            }
            // `ghost_snapshot` holds git object references; a record without
            // a kind is the legacy header or a legacy `record_type/state`.
            "ghost_snapshot" | "" => {
                self.stats.skipped_records += 1;
                Ok(())
            }
            kind if kind.ends_with("_output") => {
                let parts = tool_output_parts(payload.get("output"), &mut self.stats)?;
                let links = match payload.get("call_id").and_then(Value::as_str) {
                    Some(call_id) => (0..parts.len())
                        .map(|index| {
                            (
                                index,
                                ToolLink::Result(archive_source::owned_text(call_id.to_owned())),
                            )
                        })
                        .collect(),
                    None => Vec::new(),
                };
                self.push(parts, links, kind, timestamp);
                Ok(())
            }
            kind if kind.ends_with("_call") => {
                let body = canonical_value(&without(payload, CALL_ENVELOPE))?;
                let parts = vec![text_part(
                    schema::content_fact::modality::TOOL_CALL,
                    schema::content_fact::direction::OUT,
                    &body,
                )];
                let links = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(|call_id| {
                        (
                            0,
                            ToolLink::Call(archive_source::owned_text(call_id.to_owned())),
                        )
                    })
                    .into_iter()
                    .collect();
                self.push(parts, links, kind, timestamp);
                Ok(())
            }
            // Inter-agent messages and any item kind not named above.
            _ => {
                let parts = event_parts(
                    &without(payload, CALL_ENVELOPE),
                    schema::content_fact::direction::IN,
                )?;
                self.push(parts, Vec::new(), kind, timestamp);
                Ok(())
            }
        }
    }
}

/// Fields of a response item that are envelope, not content: the vendor's
/// item and call ids, its status, and its timing metadata. Call ids link a
/// result to its call through `responds_to` instead.
const CALL_ENVELOPE: &[&str] = &[
    "id",
    "call_id",
    "status",
    "internal_chat_message_metadata_passthrough",
    "timestamp",
    "created_at",
    "create_time",
];

fn without(value: &Value, keys: &[&str]) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !keys.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn text_part(modality: Id, direction: Id, text: &str) -> SourcePart {
    SourcePart::text(
        modality,
        direction,
        archive_source::owned_text(text.to_owned()),
    )
}

/// One sealed token as a THINKING blob part in the given direction.
fn sealed(token: &str, media_type: &str, direction: Id) -> SourcePart {
    let (bytes, media_type) =
        archive_source::sealed_bytes(token, media_type, archive_source::SealedAlphabet::UrlSafe);
    SourcePart::Blob {
        modality: schema::content_fact::modality::THINKING,
        direction,
        bytes: Bytes::from_source(bytes),
        media_type: archive_source::owned_text(media_type),
    }
}

/// One EVENT part holding the canonical JSON of `value`.
fn event_parts(value: &Value, direction: Id) -> Result<Vec<SourcePart>> {
    let body = canonical_value(value)?;
    Ok(if body != "{}" && body != "null" {
        vec![text_part(
            schema::content_fact::modality::EVENT,
            direction,
            &body,
        )]
    } else {
        Vec::new()
    })
}

/// Text and image items of a response item's `content`, in order.
fn response_content_parts(
    content: Option<&Value>,
    modality: Id,
    direction: Id,
    stats: &mut ProjectionStats,
) -> Result<Vec<SourcePart>> {
    let items: Vec<&Value> = match content {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    };
    let mut parts = Vec::new();
    for item in items {
        let part = match item {
            Value::String(text) if !text.trim().is_empty() => {
                Some(text_part(modality, direction, text))
            }
            Value::Object(_) => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    (!text.trim().is_empty()).then(|| text_part(modality, direction, text))
                } else if let Some(url) = item
                    .get("image_url")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("url").and_then(Value::as_str))
                {
                    project_asset(url, schema::content_fact::modality::IMAGE, direction, stats)
                } else {
                    Some(text_part(
                        schema::content_fact::modality::EVENT,
                        direction,
                        &canonical_value(item)?,
                    ))
                }
            }
            _ => None,
        };
        parts.extend(part);
    }
    Ok(parts)
}

fn canonical_value(value: &Value) -> Result<String> {
    archive_source::canonical_json(Bytes::from_source(serde_json::to_vec(value)?))
        .map_err(|error| anyhow!("canonicalize Codex JSON: {error}"))
}

/// The parts of a tool output: its text and images.
fn tool_output_parts(
    output: Option<&Value>,
    stats: &mut ProjectionStats,
) -> Result<Vec<SourcePart>> {
    let direction = schema::content_fact::direction::IN;
    let modality = schema::content_fact::modality::TOOL_RESULT;
    Ok(match output {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![text_part(modality, direction, text)]
        }
        Some(Value::String(_)) | Some(Value::Null) | None => Vec::new(),
        Some(Value::Array(_)) => response_content_parts(output, modality, direction, stats)?,
        Some(other) => vec![text_part(modality, direction, &canonical_value(other)?)],
    })
}

/// Project a 2026-09 `UserMessage` or `AgentMessage` item's content, in order.
///
/// User input spells a text part `text` and agent output spells it `Text`.
/// `image` (`url`) and `localImage` (`path`) are Codex's input-protocol
/// pointers, the same pointers the old format listed in `images` and
/// `local_images`. Any other content kind is skipped.
fn project_item_message_parts(
    item: &CodexItem,
    direction: Id,
    stats: &mut ProjectionStats,
) -> Vec<SourcePart> {
    let mut parts = Vec::new();
    for content in &item.content {
        let part = match content.content_type.as_deref() {
            Some("text" | "Text") => content
                .text
                .as_ref()
                .filter(|text| !text.trim().is_empty())
                .map(|text| {
                    SourcePart::text(
                        schema::content_fact::modality::TEXT,
                        direction,
                        text.clone(),
                    )
                }),
            Some("image") => content
                .image_url
                .as_ref()
                .or(content.url.as_ref())
                .and_then(|pointer| {
                    project_asset(
                        pointer,
                        schema::content_fact::modality::IMAGE,
                        direction,
                        stats,
                    )
                }),
            Some("localImage" | "local_image") => content.path.as_ref().and_then(|pointer| {
                project_asset(
                    pointer,
                    schema::content_fact::modality::IMAGE,
                    direction,
                    stats,
                )
            }),
            _ => None,
        };
        parts.extend(part);
    }
    parts
}

/// Project a 2026-09 `Reasoning` item's summary parts, in order: each is what
/// one old-format `agent_reasoning` event carried.
fn project_item_reasoning_parts(item: &CodexItem) -> Vec<SourcePart> {
    item.summary_text
        .iter()
        .filter(|text| !text.trim().is_empty())
        .map(|text| {
            SourcePart::text(
                schema::content_fact::modality::THINKING,
                schema::content_fact::direction::OUT,
                text.clone(),
            )
        })
        .collect()
}

fn project_reasoning_parts(payload: &CodexPayload) -> Vec<SourcePart> {
    payload
        .text
        .as_ref()
        .filter(|text| !text.trim().is_empty())
        .map(|text| {
            SourcePart::text(
                schema::content_fact::modality::THINKING,
                schema::content_fact::direction::OUT,
                text.clone(),
            )
        })
        .into_iter()
        .collect()
}

fn project_message_parts(
    payload: &CodexPayload,
    direction: Id,
    stats: &mut ProjectionStats,
) -> Vec<SourcePart> {
    let mut parts = Vec::new();
    if let Some(message) = payload
        .message
        .as_ref()
        .filter(|message| !message.trim().is_empty())
    {
        parts.push(SourcePart::text(
            schema::content_fact::modality::TEXT,
            direction,
            message.clone(),
        ));
    }
    for pointer in payload.images.iter().chain(&payload.local_images) {
        parts.extend(project_asset(
            pointer,
            schema::content_fact::modality::IMAGE,
            direction,
            stats,
        ));
    }
    for pointer in payload.audio.iter().chain(&payload.local_audio) {
        parts.extend(project_asset(
            pointer,
            schema::content_fact::modality::AUDIO,
            direction,
            stats,
        ));
    }
    parts
}

fn project_asset(
    pointer: &str,
    modality: Id,
    direction: Id,
    stats: &mut ProjectionStats,
) -> Option<SourcePart> {
    if let Some(data) = pointer.strip_prefix("data:") {
        let Some((header, payload)) = data.split_once(',') else {
            stats.undecodable_assets += 1;
            return None;
        };
        if !header
            .split(';')
            .any(|component| component.eq_ignore_ascii_case("base64"))
        {
            stats.undecodable_assets += 1;
            return None;
        }
        let bytes = match BASE64_STANDARD.decode(payload.as_bytes()) {
            Ok(bytes) => bytes,
            Err(_) => {
                stats.undecodable_assets += 1;
                return None;
            }
        };
        let media_type = header
            .split(';')
            .next()
            .filter(|value| !value.trim().is_empty())
            .map(files::normalize_media_type_or_default)
            .unwrap_or_else(|| files::DEFAULT_MEDIA_TYPE.to_owned());
        return Some(SourcePart::Blob {
            modality,
            direction,
            bytes: Bytes::from_source(bytes),
            media_type: archive_source::owned_text(media_type),
        });
    }

    Some(SourcePart::Pointer {
        modality,
        direction,
        namespace: schema::source_projection::SOURCE_CODEX,
        pointer: archive_source::owned_text(pointer.to_owned()),
        media_type: None,
        size: None,
        resolved: None,
    })
}

fn consistent_session_id(payload: &CodexPayload) -> Result<String> {
    let id = payload
        .id
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let session_id = payload
        .session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    // `id` and `session_id` are DIFFERENT THINGS in the post-2026-07 rollout
    // format, and requiring them equal refused every recent Codex session.
    // MEASURED 2026-09-01 over 8,265 rollouts in ~/.codex/sessions/2026:
    //
    //   2026-01..06   0% refused   (older shape: `id` only, no session_id)
    //   2026-07..09  99% refused   (3,161 files carrying both)
    //
    // That is a clean format cutover, not a data defect — and it meant the
    // Codex half of the archive silently stopped ingesting in July while
    // nothing announced it. Exactly the failure the archive epic exists to
    // prevent (compass 9d9768c9: "the absence of a transcript is
    // indistinguishable from its never having existed").
    //
    // `id` identifies THIS ROLLOUT and matches the filename; `session_id`
    // identifies the CONVERSATION and is stable across resumes. The archive's
    // unit is a conversation — a DAG whose complete ancestry `archive thread`
    // walks — so `session_id` is the correct identity and must WIN. Preferring
    // `id` would shatter one resumed conversation into as many conversations
    // as it had resumes.
    session_id
        .or(id)
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("Codex session_meta payload has neither id nor session_id"))
}

fn scan_syntax(message: &str) -> sc::ScanError {
    sc::ScanError::Syntax(message.to_owned())
}

fn scan_string(bytes: &mut Bytes) -> std::result::Result<Option<View<str>>, sc::ScanError> {
    if bytes.first().copied() != Some(b'"') {
        sc::skip_value(bytes)?;
        return Ok(None);
    }
    sc::parse_string(bytes)?
        .view::<str>()
        .map(Some)
        .map_err(|_| scan_syntax("JSON string is not UTF-8"))
}

#[derive(Default)]
struct PointerFields {
    image_url: Option<View<str>>,
    audio_url: Option<View<str>>,
    url: Option<View<str>>,
    path: Option<View<str>>,
}

fn scan_pointers(bytes: &mut Bytes) -> std::result::Result<Vec<View<str>>, sc::ScanError> {
    if bytes.first().copied() != Some(b'[') {
        sc::skip_value(bytes)?;
        return Ok(Vec::new());
    }
    sc::array(bytes, Vec::new(), |mut pointers, value| {
        let pointer = match value.first().copied() {
            Some(b'"') => scan_string(value)?,
            Some(b'{') => {
                let fields = sc::object(
                    value,
                    PointerFields::default(),
                    |mut fields, key, member| {
                        let key = key
                            .view::<str>()
                            .map_err(|_| scan_syntax("JSON object key is not UTF-8"))?;
                        match key.as_ref() {
                            "image_url" => fields.image_url = scan_string(member)?,
                            "audio_url" => fields.audio_url = scan_string(member)?,
                            "url" => fields.url = scan_string(member)?,
                            "path" => fields.path = scan_string(member)?,
                            _ => sc::skip_value(member)?,
                        }
                        Ok(fields)
                    },
                )?;
                fields
                    .image_url
                    .or(fields.audio_url)
                    .or(fields.url)
                    .or(fields.path)
            }
            _ => {
                sc::skip_value(value)?;
                None
            }
        };
        if let Some(pointer) = pointer.filter(|pointer| !pointer.as_ref().trim().is_empty()) {
            pointers.push(pointer);
        }
        Ok(pointers)
    })
}

fn scan_payload(bytes: &mut Bytes) -> std::result::Result<Option<CodexPayload>, sc::ScanError> {
    if bytes.first().copied() != Some(b'{') {
        sc::skip_value(bytes)?;
        return Ok(None);
    }
    sc::object(bytes, CodexPayload::default(), |mut payload, key, value| {
        let key = key
            .view::<str>()
            .map_err(|_| scan_syntax("JSON object key is not UTF-8"))?;
        match key.as_ref() {
            "type" => payload.payload_type = scan_string(value)?,
            "id" => payload.id = scan_string(value)?,
            "session_id" => payload.session_id = scan_string(value)?,
            "model" => payload.model = scan_string(value)?,
            "message" => payload.message = scan_string(value)?,
            "text" => payload.text = scan_string(value)?,
            "images" => payload.images = scan_pointers(value)?,
            "local_images" => payload.local_images = scan_pointers(value)?,
            "audio" => payload.audio = scan_pointers(value)?,
            "local_audio" => payload.local_audio = scan_pointers(value)?,
            "item" => payload.item = scan_item(value)?,
            _ => sc::skip_value(value)?,
        }
        Ok(payload)
    })
    .map(Some)
}

fn scan_item(bytes: &mut Bytes) -> std::result::Result<Option<CodexItem>, sc::ScanError> {
    if bytes.first().copied() != Some(b'{') {
        sc::skip_value(bytes)?;
        return Ok(None);
    }
    sc::object(bytes, CodexItem::default(), |mut item, key, value| {
        let key = key
            .view::<str>()
            .map_err(|_| scan_syntax("JSON object key is not UTF-8"))?;
        match key.as_ref() {
            "type" => item.item_type = scan_string(value)?,
            "content" => item.content = scan_item_content(value)?,
            "summary_text" => item.summary_text = scan_strings(value)?,
            _ => sc::skip_value(value)?,
        }
        Ok(item)
    })
    .map(Some)
}

fn scan_item_content(bytes: &mut Bytes) -> std::result::Result<Vec<ItemContent>, sc::ScanError> {
    if bytes.first().copied() != Some(b'[') {
        sc::skip_value(bytes)?;
        return Ok(Vec::new());
    }
    sc::array(bytes, Vec::new(), |mut contents, value| {
        if value.first().copied() != Some(b'{') {
            sc::skip_value(value)?;
            return Ok(contents);
        }
        let content = sc::object(value, ItemContent::default(), |mut content, key, member| {
            let key = key
                .view::<str>()
                .map_err(|_| scan_syntax("JSON object key is not UTF-8"))?;
            match key.as_ref() {
                "type" => content.content_type = scan_string(member)?,
                "text" => content.text = scan_string(member)?,
                "image_url" => content.image_url = scan_string(member)?,
                "url" => content.url = scan_string(member)?,
                "path" => content.path = scan_string(member)?,
                _ => sc::skip_value(member)?,
            }
            Ok(content)
        })?;
        contents.push(content);
        Ok(contents)
    })
}

fn scan_strings(bytes: &mut Bytes) -> std::result::Result<Vec<View<str>>, sc::ScanError> {
    if bytes.first().copied() != Some(b'[') {
        sc::skip_value(bytes)?;
        return Ok(Vec::new());
    }
    sc::array(bytes, Vec::new(), |mut strings, value| {
        if let Some(string) = scan_string(value)? {
            strings.push(string);
        }
        Ok(strings)
    })
}

fn parse_record(raw: Bytes, source_path: &Path) -> Result<CodexRecord> {
    std::str::from_utf8(raw.as_ref()).with_context(|| {
        format!(
            "parse candidate Codex record in {}: JSON is not UTF-8",
            source_path.display()
        )
    })?;
    let mut bytes = raw;
    sc::skip_ws(&mut bytes);
    if bytes.first().copied() != Some(b'{') {
        sc::skip_value(&mut bytes).with_context(|| {
            format!("parse candidate Codex record in {}", source_path.display())
        })?;
        sc::skip_ws(&mut bytes);
        if !bytes.is_empty() {
            bail!(
                "parse candidate Codex record in {}: trailing bytes after JSON value",
                source_path.display()
            );
        }
        return Ok(CodexRecord::default());
    }
    let record = sc::object(
        &mut bytes,
        CodexRecord::default(),
        |mut record, key, value| {
            let key = key
                .view::<str>()
                .map_err(|_| scan_syntax("JSON object key is not UTF-8"))?;
            match key.as_ref() {
                "type" => record.record_type = scan_string(value)?,
                "timestamp" => record.timestamp = scan_string(value)?,
                "payload" => record.payload = scan_payload(value)?,
                _ => sc::skip_value(value)?,
            }
            Ok(record)
        },
    )
    .with_context(|| format!("parse candidate Codex record in {}", source_path.display()))?;
    sc::skip_ws(&mut bytes);
    if !bytes.is_empty() {
        bail!(
            "parse candidate Codex record in {}: trailing bytes after JSON object",
            source_path.display()
        );
    }
    Ok(record)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memmem::find(haystack, needle).is_some()
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn parse_iso_timestamp(value: &str) -> Option<Epoch> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        value.parse().ok()
    }
}

fn epoch_ms(epoch: Epoch) -> i64 {
    epoch.to_unix_milliseconds() as i64
}

/// Unix ms of a UUIDv7 (its first 48 bits), if `id` is one.
fn uuid_v7_ms(id: &str) -> Option<i64> {
    let hex: String = id.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || hex.as_bytes()[12] != b'7' {
        return None;
    }
    i64::from_str_radix(&hex[..12], 16).ok()
}

/// The envelope timestamp of a record line, read without a full parse.
fn line_ms(raw: &[u8]) -> Option<i64> {
    let start = memmem::find(raw, b"\"timestamp\":\"")? + 13;
    let end = start + raw[start..].iter().position(|byte| *byte == b'"')?;
    std::str::from_utf8(&raw[start..end])
        .ok()
        .and_then(parse_iso_timestamp)
        .map(epoch_ms)
}

fn epoch_interval(epoch: Epoch) -> Option<Inline<NsTAIInterval>> {
    (epoch, epoch).try_to_inline().ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use tempfile::TempDir;
    use triblespace::core::metadata;
    use triblespace::core::repo::pile::{Pile, PileSnapshot};
    use triblespace::core::repo::BlobStoreGet;
    use triblespace::macros::{find, pattern};
    use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
    use triblespace::prelude::inlineencodings::Handle;

    use super::*;
    use crate::archive_collection::ArchiveImportWriter;
    use crate::blockdag;
    use crate::storage::FactView;

    const ROLLOUT: &str = concat!(
        r#"{"timestamp":"2026-08-16T08:00:00.000Z","type":"session_meta","payload":{"id":"thread-1","session_id":"thread-1"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-16T08:00:00.001Z","type":"turn_context","payload":{"model":"gpt-test"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-16T08:00:00.002Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-16T08:00:00.003Z","type":"event_msg","payload":{"type":"user_message","message":"hello","images":[],"local_images":["/moved/reference.png"]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-16T08:00:00.500Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"I should answer directly."}}"#,
        "\n",
        r#"{"timestamp":"2026-08-16T08:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","phase":"commentary","message":"hi"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-16T08:00:01.001Z","type":"response_item","payload":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-16T08:00:01.002Z","type":"event_msg","payload":{"type":"token_count"}}"#,
        "\n",
    );

    fn project_text(text: &str, name: &str) -> (TempDir, ProjectionSummary, Fragment) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(name);
        fs::write(&path, text).unwrap();
        let mut fragment = Fragment::empty();
        let summary = project_path(&path, |projected| {
            fragment += projected.fragment;
            Ok(())
        })
        .unwrap();
        (directory, summary, fragment)
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

    fn has_tag(fragment: &Fragment, tag: Id) -> bool {
        !ids_with_tag(fragment, tag).is_empty()
    }

    /// Every fragment a rollout emits, in order.
    fn project_fragments(text: &str, name: &str) -> (TempDir, Vec<Fragment>) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(name);
        fs::write(&path, text).unwrap();
        let mut fragments = Vec::new();
        project_path(&path, |projected| {
            fragments.push(projected.fragment);
            Ok(())
        })
        .unwrap();
        (directory, fragments)
    }

    #[test]
    fn every_record_with_content_is_one_chain_of_perceptions_and_acts() {
        let (_directory, summary, fragment) = project_text(ROLLOUT, "rollout.jsonl");
        // The user-role response item is the prompt (the user event only
        // mirrors it); the reasoning and the reply follow it, one node each.
        assert_eq!(summary.stats.source_projections, 3);
        assert_eq!(summary.stats.content_parts, 3);
        assert_eq!(summary.stats.records_seen, 8);
        // One node each, then the leaf.
        assert_eq!(summary.fragments_emitted, 4);
        assert_eq!(summary.stats.skipped_records, 5);
        assert_eq!(summary.stats.mirrored_prompts, 1);
        assert_eq!(
            ids_with_tag(&fragment, schema::source_projection::KIND).len(),
            3
        );
        assert_eq!(ids_with_tag(&fragment, schema::block::KIND).len(), 3);
        assert!(!exists!(pattern!(fragment.facts(), [{
            _?receipt @ schema::source_projection::raw_record: _?raw
        }])));

        let (_directory, reader) = empty_reader();
        let (_, validation) =
            blockdag::validate_catalog_union(&reader, &TribleSet::new(), &fragment).unwrap();
        assert_eq!(validation, blockdag::CatalogValidation::Accepted);

        assert_eq!(
            find!(
                block: Id,
                pattern!(fragment.facts(), [{
                    ?block @ schema::block::previous: _?previous
                }])
            )
            .count(),
            2
        );
        assert_eq!(
            find!(
                projection: Id,
                pattern!(fragment.facts(), [{
                    ?projection @ schema::source_projection::source_timestamp: _?timestamp
                }])
            )
            .count(),
            3,
            "each node's receipt carries the time of its record"
        );
        assert_eq!(
            find!(
                projection: Id,
                pattern!(fragment.facts(), [{
                    ?projection @ schema::source_projection::source_locator: _?locator
                }])
            )
            .count(),
            1,
            "the session names the leaf only"
        );
        let thinking_payloads: Vec<Inline<Handle<UTF8String>>> = find!(
            payload: Inline<Handle<UTF8String>>,
            pattern!(fragment.facts(), [
                {
                    _?fact @ schema::content_fact::modality:
                        schema::content_fact::modality::THINKING
                },
                {
                    _?fact @ schema::content_fact::direction:
                        schema::content_fact::direction::OUT
                },
                { _?fact @ schema::content_fact::payload: ?payload }
            ])
        )
        .collect();
        assert_eq!(thinking_payloads.len(), 1);
        let mut blobs = fragment.blobs().clone();
        let blob_reader = blobs.snapshot().unwrap();
        let reasoning = blob_reader
            .get::<View<str>, UTF8String>(thinking_payloads[0])
            .unwrap();
        assert_eq!(reasoning.as_ref(), "I should answer directly.");
        assert!(!exists!(pattern!(fragment.facts(), [{
            _?block @ schema::block::timestamp: _?timestamp
        }])));
    }

    #[test]
    fn append_and_source_move_preserve_every_existing_intrinsic_identity() {
        let (_first_dir, _, first) = project_text(ROLLOUT, "first.jsonl");
        let appended_source = format!(
            "{ROLLOUT}{}\n",
            r#"{"timestamp":"2026-08-16T08:00:02.000Z","type":"event_msg","payload":{"type":"user_message","message":"again"}}"#
        );
        let (_second_dir, _, appended) = project_text(&appended_source, "second.jsonl");
        assert!(ids_with_tag(&first, schema::block::KIND)
            .is_subset(&ids_with_tag(&appended, schema::block::KIND)));
        let first_semantic = ids_with_tag(&first, schema::source_projection::KIND);
        let appended_semantic = ids_with_tag(&appended, schema::source_projection::KIND);
        assert!(first_semantic.is_subset(&appended_semantic));

        let (_moved_dir, _, moved) = project_text(ROLLOUT, "moved.jsonl");
        assert_eq!(
            ids_with_tag(&first, schema::block::KIND),
            ids_with_tag(&moved, schema::block::KIND)
        );
        assert_eq!(
            ids_with_tag(&first, schema::source_projection::KIND),
            ids_with_tag(&moved, schema::source_projection::KIND)
        );
        assert_ne!(
            first.facts(),
            moved.facts(),
            "the movable path remains additive source evidence"
        );

        let spaced_source = ROLLOUT.replace(
            r#"{"timestamp":"2026-08-16T08:00:00.003Z","type":"event_msg""#,
            r#"  {"timestamp":"2026-08-16T08:00:00.003Z","type":"event_msg""#,
        );
        let (_spaced_dir, _, spaced) = project_text(&spaced_source, "spaced.jsonl");
        assert_eq!(
            ids_with_tag(&first, schema::block::KIND),
            ids_with_tag(&spaced, schema::block::KIND),
            "JSON whitespace is not semantic block identity"
        );
        assert_eq!(
            ids_with_tag(&first, schema::source_projection::KIND),
            ids_with_tag(&spaced, schema::source_projection::KIND),
            "nor receipt identity"
        );
    }

    fn chunked_telemetry_rollout(target_len: usize) -> String {
        let mut rollout = concat!(
            r#"{"timestamp":"2026-08-16T08:00:00Z","type":"session_meta","payload":{"id":"chunk-thread"}}"#,
            "\n"
        )
        .to_owned();
        let telemetry = concat!(
            r#"{"timestamp":"2026-08-16T08:00:01Z","type":"response_item","payload":{"type":"reasoning","encrypted_content":"opaque"}}"#,
            "\n"
        );
        for _ in 0..1_024 {
            rollout.push_str(telemetry);
        }
        let padding_prefix = r#"{"type":"response_item","padding":""#;
        let padding_suffix = "\"}\n";
        let padding = target_len
            .checked_sub(rollout.len() + padding_prefix.len() + padding_suffix.len())
            .expect("target leaves room for the final telemetry row");
        rollout.push_str(padding_prefix);
        rollout.extend(std::iter::repeat_n('x', padding));
        rollout.push_str(padding_suffix);
        assert_eq!(rollout.len(), target_len);
        rollout
    }

    #[test]
    fn sealed_only_reasoning_is_kept_as_bytes_in_one_act() {
        let source = chunked_telemetry_rollout(256 * 1024);
        let (_directory, summary, fragment) = project_text(&source, "sealed.jsonl");
        assert_eq!(summary.stats.records_seen, 1_026);
        assert_eq!(summary.stats.skipped_records, 2, "the meta and the padding");
        // One node per record, the same fact each time: the chain tells
        // them apart, and nothing was perceived between them, so one act.
        assert_eq!(summary.stats.source_projections, 1_024);
        assert_eq!(summary.stats.content_parts, 1_024);
        assert_eq!(
            blockdag::turn_of(fragment.facts(), leaf(&fragment)).len(),
            1_024
        );
        use schema::content_fact::{direction, modality};
        // One token, "opaque", which is not canonical base64: kept as its
        // exact text under the `+text` media type, as a THINKING blob.
        assert_eq!(
            content_facts(&fragment, modality::THINKING, direction::OUT).len(),
            1
        );
        assert!(!exists!(pattern!(fragment.facts(), [{
            _?fact @ schema::content_fact::payload: _?text
        }])));
    }

    #[test]
    fn encrypted_reasoning_is_kept_as_bytes_in_the_act_it_belongs_to() {
        use base64::engine::general_purpose::URL_SAFE;
        let token = URL_SAFE.encode([0x80u8; 45]);
        let with = ITEM_ROLLOUT.replace(
            r#""encrypted_content":"opaque""#,
            &format!(r#""encrypted_content":"{token}""#),
        );
        let (_with_dir, with_summary, with) = project_text(&with, "with.jsonl");
        assert_eq!(with_summary.stats.source_projections, 5);
        let mut probe = Fragment::empty();
        let bytes = probe.put::<RawBytes, _>(vec![0x80u8; 45]);
        let media =
            probe.put::<UTF8String, _>(schema::content_fact::sealed::OPENAI_REASONING.to_owned());
        let reply = probe.put::<UTF8String, _>("hi".to_owned());
        // The sealed token, the readable summary and the reply are one act.
        assert!(exists!(pattern!(with.facts(), [
            { _?fact @ schema::content_fact::modality: &schema::content_fact::modality::THINKING,
                schema::content_fact::direction: &schema::content_fact::direction::OUT,
                schema::content_fact::blob: bytes,
                schema::content_fact::media_type: _?media_type },
            { _?media_type @ metadata::name: media },
            { _?sealed @ schema::block::holds: _?fact },
        ])));
        let sealed = find!(
            node: Id,
            pattern!(with.facts(), [
                { ?node @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::blob: bytes },
            ])
        )
        .next()
        .unwrap();
        let reply = find!(
            node: Id,
            pattern!(with.facts(), [
                { ?node @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::payload: reply },
            ])
        )
        .next()
        .unwrap();
        assert!(blockdag::turn_of(with.facts(), reply).contains(&sealed));
    }

    /// The legacy bare format (2025): a header, then bare response items with
    /// no event stream, so messages and reasoning are the dialogue.
    const LEGACY_ROLLOUT: &str = concat!(
        r#"{"id":"legacy-thread","timestamp":"2025-08-17T01:22:00.411Z","instructions":"be brief"}"#,
        "\n",
        r#"{"record_type":"state"}"#,
        "\n",
        r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"list files"}]}"#,
        "\n",
        r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"**Listing**"}],"encrypted_content":"sealed"}"#,
        "\n",
        r#"{"type":"function_call","id":"fc_1","name":"shell","arguments":"{\"command\":[\"ls\"]}","call_id":"call_1"}"#,
        "\n",
        r#"{"type":"function_call_output","call_id":"call_1","output":"a.txt\nb.txt"}"#,
        "\n",
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"two files"}]}"#,
        "\n",
    );

    #[test]
    fn legacy_rollouts_project_messages_reasoning_and_tools() {
        let (_directory, summary, fragment) = project_text(LEGACY_ROLLOUT, "legacy.jsonl");
        assert_eq!(
            summary.stats.source_projections, 6,
            "the prompt; reasoning; sealed reasoning; the call; the output; the answer"
        );
        assert_eq!(summary.stats.skipped_records, 2, "header and state");
        use schema::content_fact::{direction, modality};
        assert_eq!(
            content_facts(&fragment, modality::TOOL_CALL, direction::OUT).len(),
            1
        );
        assert_eq!(
            content_facts(&fragment, modality::TOOL_RESULT, direction::IN).len(),
            1
        );
        assert_eq!(
            content_facts(&fragment, modality::THINKING, direction::OUT).len(),
            2,
            "the summary text and the sealed token"
        );
        // The output answers the call through a semantic edge, not the id.
        let call = find!(
            node: Id,
            pattern!(fragment.facts(), [
                { ?node @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::modality: &modality::TOOL_CALL },
            ])
        )
        .next()
        .unwrap();
        // The answer follows the output it saw, which follows the call it
        // answers.
        assert!(exists!(pattern!(fragment.facts(), [
            { _?output @ schema::block::previous: call,
                schema::block::responds_to: call },
            { _?answer @ schema::block::previous: _?output },
        ])));
        let (_reader_directory, reader) = empty_reader();
        let (_, validation) =
            blockdag::validate_catalog_union(&reader, &TribleSet::new(), &fragment).unwrap();
        assert_eq!(validation, blockdag::CatalogValidation::Accepted);
    }

    #[test]
    fn partial_live_tail_is_archived_but_deferred_and_missing_session_identity_is_rejected() {
        let partial = format!("{ROLLOUT}{{\"type\":\"event_msg\"");
        let (_directory, summary, fragment) = project_text(&partial, "live.jsonl");
        assert!(summary.trailing_bytes_ignored > 0);
        assert_eq!(
            ids_with_tag(&fragment, schema::source_projection::KIND).len(),
            3
        );

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing-meta.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"timestamp":"2026-08-16T08:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"orphan"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let error = project_path(&path, |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("no stable session_meta"));
    }

    #[test]
    fn scanner_decodes_escaped_fields_and_nested_pointer_objects_without_a_dom() {
        let record = parse_record(
            Bytes::from_source(
                br#" {"type":"event_msg","timestamp":"2026-08-16T08:00:00Z","payload":{"type":"user_message","message":"hello\nworld","images":[{"path":"fallback","url":"chosen\u002epng"},17,"plain.png"],"local_images":"not-an-array"}} "#
                    .to_vec(),
            ),
            Path::new("scanner.jsonl"),
        )
        .unwrap();
        assert_eq!(record.record_type.as_deref(), Some("event_msg"));
        let payload = record.payload.unwrap();
        assert_eq!(payload.message.as_deref(), Some("hello\nworld"));
        assert_eq!(
            payload
                .images
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<&str>>(),
            vec!["chosen.png", "plain.png"]
        );
        assert!(payload.local_images.is_empty());
    }

    /// The 2026-09 rollout shape: an `ordinal` on every record, and the
    /// dialogue as `event_msg/item_completed` items. Bookkeeping, tool items,
    /// an empty reasoning item and record kinds the old format never had are
    /// all here to be skipped.
    const ITEM_ROLLOUT: &str = concat!(
        r#"{"timestamp":"2026-09-10T08:00:00.000Z","ordinal":0,"type":"session_meta","payload":{"session_id":"thread-n","id":"thread-n"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:00.001Z","ordinal":1,"type":"turn_context","payload":{"turn_id":"turn-1","model":"gpt-test"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:00.002Z","ordinal":2,"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:00.003Z","ordinal":3,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:00.004Z","ordinal":4,"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-n","turn_id":"turn-1","item":{"type":"UserMessage","id":"item-1","content":[{"type":"text","text":"hello","text_elements":[]},{"type":"localImage","path":"/moved/reference.png"}]},"started_at_ms":1,"completed_at_ms":1}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:00.400Z","ordinal":5,"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"**Planning**"}],"encrypted_content":"opaque"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:00.500Z","ordinal":6,"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-n","turn_id":"turn-1","item":{"type":"Reasoning","id":"rs-1","summary_text":["**Planning**","**Answering directly**"],"raw_content":[]}}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:00.600Z","ordinal":7,"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-n","turn_id":"turn-1","item":{"type":"Reasoning","id":"rs-2","summary_text":[],"raw_content":[]}}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:00.700Z","ordinal":8,"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-n","turn_id":"turn-1","item":{"type":"CommandExecution","id":"exec-1","command":["true"],"stdout":"hi"}}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:01.000Z","ordinal":9,"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-n","turn_id":"turn-1","item":{"type":"AgentMessage","id":"msg-1","content":[{"type":"Text","text":"hi"}],"phase":"final_answer"}}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:01.001Z","ordinal":10,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:01.002Z","ordinal":11,"type":"event_msg","payload":{"type":"token_count","info":null}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:01.003Z","ordinal":12,"type":"token_usage_record","payload":{"total":1}}"#,
        "\n",
        r#"{"timestamp":"2026-09-10T08:00:01.004Z","ordinal":13,"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":"hi"}}"#,
        "\n",
    );

    fn content_facts(fragment: &Fragment, modality: Id, direction: Id) -> BTreeSet<Id> {
        find!(
            fact: Id,
            pattern!(fragment.facts(), [{
                ?fact @
                    schema::content_fact::modality: &modality,
                    schema::content_fact::direction: &direction,
            }])
        )
        .collect()
    }

    /// The one node nothing follows.
    fn leaf(fragment: &Fragment) -> Id {
        let leaves: Vec<Id> = ids_with_tag(fragment, schema::block::KIND)
            .into_iter()
            .filter(|node| {
                let node = *node;
                !exists!(pattern!(fragment.facts(), [{
                    _?next @ schema::block::previous: node
                }]))
            })
            .collect();
        assert_eq!(leaves.len(), 1, "one chain");
        leaves[0]
    }

    fn root_blocks(fragment: &Fragment) -> BTreeSet<Id> {
        ids_with_tag(fragment, schema::block::KIND)
            .into_iter()
            .filter(|block| {
                let block = *block;
                !exists!(pattern!(fragment.facts(), [{
                    block @ schema::block::previous: _?previous
                }]))
            })
            .collect()
    }

    #[test]
    fn item_completed_rollouts_project_into_the_old_vocabulary() {
        let (_directory, summary, fragment) = project_text(ITEM_ROLLOUT, "items.jsonl");
        assert_eq!(summary.stats.records_seen, 14);
        // The prompt the model was given is one node (the UserMessage item
        // only mirrors it); the sealed reasoning, the summary parts and the
        // reply follow it, one node each, and are one act.
        assert_eq!(summary.stats.source_projections, 5);
        // The prompt, two summary parts, one text, and the sealed reasoning
        // token.
        assert_eq!(summary.stats.content_parts, 5);
        assert_eq!(
            summary.stats.skipped_records, 10,
            "bookkeeping, tool items, the empty reasoning item and mirrors"
        );
        assert_eq!(summary.stats.mirrored_prompts, 1);
        assert_eq!(summary.fragments_emitted, 6);
        assert_eq!(
            ids_with_tag(&fragment, schema::source_projection::KIND).len(),
            5
        );
        assert_eq!(ids_with_tag(&fragment, schema::block::KIND).len(), 5);
        assert_eq!(root_blocks(&fragment).len(), 1, "one chain");

        let (_reader_directory, reader) = empty_reader();
        let (_, validation) =
            blockdag::validate_catalog_union(&reader, &TribleSet::new(), &fragment).unwrap();
        assert_eq!(validation, blockdag::CatalogValidation::Accepted);

        use schema::content_fact::{direction, modality};
        assert_eq!(
            content_facts(&fragment, modality::TEXT, direction::IN).len(),
            1
        );
        assert_eq!(
            content_facts(&fragment, modality::THINKING, direction::OUT).len(),
            3,
            "two summary parts and the sealed token"
        );
        assert_eq!(
            content_facts(&fragment, modality::TEXT, direction::OUT).len(),
            1
        );

        // Both summary parts follow each other along the chain, in their
        // source order, in the act that ends with the reply.
        let mut blobs = fragment.blobs().clone();
        let blob_reader = blobs.snapshot().unwrap();
        let tip = leaf(&fragment);
        let act = blockdag::turn_of(fragment.facts(), tip);
        assert_eq!(
            act.len(),
            4,
            "sealed reasoning, two summary parts, the reply"
        );
        let thinking: Vec<String> = blockdag::thread_to(fragment.facts(), tip)
            .into_iter()
            .flat_map(|node| {
                find!(
                    payload: Inline<Handle<UTF8String>>,
                    pattern!(fragment.facts(), [
                        { node @ schema::block::holds: _?fact },
                        { _?fact @
                            schema::content_fact::modality: &modality::THINKING,
                            schema::content_fact::payload: ?payload,
                        },
                    ])
                )
                .collect::<Vec<_>>()
            })
            .map(|payload| {
                blob_reader
                    .get::<View<str>, UTF8String>(payload)
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(thinking, ["**Planning**", "**Answering directly**"]);

        // The same words in the old spelling are the same content facts, and
        // an opening message spelled either way is the same block.
        let (_old_directory, _, old) = project_text(ROLLOUT, "old.jsonl");
        for (spoken, heading) in [
            (modality::TEXT, direction::IN),
            (modality::TEXT, direction::OUT),
        ] {
            assert_eq!(
                content_facts(&fragment, spoken, heading),
                content_facts(&old, spoken, heading)
            );
        }
        assert_eq!(root_blocks(&fragment), root_blocks(&old));
    }

    const PARENT_ROLLOUT: &str = concat!(
        r#"{"timestamp":"2026-08-01T10:00:00.000Z","type":"session_meta","payload":{"id":"parent-thread","session_id":"parent-thread"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-01T10:00:00.001Z","type":"turn_context","payload":{"model":"gpt-test"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-01T10:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"first question","images":[],"local_images":[]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-01T10:00:02.000Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"weighing the question"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-01T10:00:03.000Z","type":"event_msg","payload":{"type":"agent_message","message":"first answer"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-01T10:00:03.001Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#,
        "\n",
    );

    /// A fork as Codex writes one: its own session record, then every parent
    /// record again with only the timestamp rewritten, then its own turn.
    fn fork_rollout() -> String {
        let mut fork = String::from(concat!(
            r#"{"timestamp":"2026-08-03T09:00:00.000Z","type":"session_meta","payload":{"id":"fork-thread","session_id":"parent-thread","forked_from_id":"parent-thread"}}"#,
            "\n"
        ));
        for line in PARENT_ROLLOUT.lines() {
            let (_, rest) = line.split_once(r#"","type""#).unwrap();
            fork.push_str(r#"{"timestamp":"2026-08-03T09:00:00.500Z","type""#);
            fork.push_str(rest);
            fork.push('\n');
        }
        fork.push_str(concat!(
            r#"{"timestamp":"2026-08-03T09:00:05.000Z","type":"turn_context","payload":{"model":"gpt-test"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-03T09:00:06.000Z","type":"event_msg","payload":{"type":"user_message","message":"second question","images":[],"local_images":[]}}"#,
            "\n",
            r#"{"timestamp":"2026-08-03T09:00:07.000Z","type":"event_msg","payload":{"type":"agent_message","message":"second answer"}}"#,
            "\n",
        ));
        fork
    }

    /// A 2025 rollout: its header carries only `id`.
    const GRAND_2025: &str = concat!(
        r#"{"timestamp":"2025-11-26T16:56:48.273Z","type":"session_meta","payload":{"id":"grand-2025","timestamp":"2025-11-26T16:56:48.273Z","originator":"codex_cli_rs"}}"#,
        "\n",
        r#"{"timestamp":"2025-11-26T16:56:48.300Z","type":"turn_context","payload":{"model":"gpt-test"}}"#,
        "\n",
        r#"{"timestamp":"2025-11-26T16:56:49.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first question"}]}}"#,
        "\n",
        r#"{"timestamp":"2025-11-26T16:56:49.001Z","type":"event_msg","payload":{"type":"user_message","message":"first question","images":[]}}"#,
        "\n",
        r#"{"timestamp":"2025-11-26T16:56:50.000Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"weigh it"}}"#,
        "\n",
        r#"{"timestamp":"2025-11-26T16:56:51.000Z","type":"event_msg","payload":{"type":"agent_message","message":"first answer"}}"#,
        "\n",
    );

    /// A 2025 resume as Codex wrote it: its own header, then the header of
    /// every ancestor under the ancestor's own id, then the ancestor's
    /// history copied under the resume's creation time, then its own turn.
    /// The ancestor headers keep their own payload; every copied line is
    /// restamped with the resume's creation time.
    fn resume_2025(own: &str, created: &str, history: &str, turn: &str) -> String {
        let mut rollout = format!(
            r#"{{"timestamp":"{created}","type":"session_meta","payload":{{"id":"{own}","timestamp":"{created}","originator":"codex_cli_rs"}}}}"#
        );
        rollout.push('\n');
        for line in history.lines() {
            let (_, rest) = line.split_once(r#"","type""#).unwrap();
            rollout.push_str(&format!(r#"{{"timestamp":"{created}","type""#));
            rollout.push_str(rest);
            rollout.push('\n');
        }
        let later = |seconds: u32| created.replace(":00.000Z", &format!(":{seconds:02}.000Z"));
        for line in [
            format!(
                r#"{{"timestamp":"{}","type":"turn_context","payload":{{"model":"gpt-test"}}}}"#,
                later(10)
            ),
            format!(
                r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{turn} question"}}]}}}}"#,
                later(11)
            ),
            format!(
                r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"user_message","message":"{turn} question","images":[]}}}}"#,
                later(12)
            ),
            format!(
                r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"agent_message","message":"{turn} answer"}}}}"#,
                later(13)
            ),
        ] {
            rollout.push_str(&line);
            rollout.push('\n');
        }
        rollout
    }

    #[test]
    fn a_resume_naming_its_ancestors_converges_on_their_rollouts() {
        // As in the 2025 rollouts: a resume carries its ancestors' headers
        // under their own ids and replays their history.
        let parent = resume_2025(
            "parent-2025",
            "2025-12-11T01:58:00.000Z",
            GRAND_2025,
            "second",
        );
        let child = resume_2025("child-2025", "2025-12-11T22:16:00.000Z", &parent, "third");
        assert_eq!(child.matches(r#""type":"session_meta""#).count(), 3);
        let mut threads = Vec::new();
        for (rollout, name) in [
            (GRAND_2025, "grand"),
            (parent.as_str(), "parent"),
            (child.as_str(), "child"),
        ] {
            let (_directory, summary, fragment) = project_text(rollout, &format!("{name}.jsonl"));
            assert_eq!(summary.stats.held_back_records, 0, "{name}");
            assert_eq!(summary.stats.unresolved_tool_results, 0, "{name}");
            let tip = leaf(&fragment);
            let thread = blockdag::thread_to(fragment.facts(), tip);
            assert_eq!(
                ids_with_tag(&fragment, schema::block::KIND),
                thread.iter().copied().collect::<BTreeSet<_>>(),
                "{name} is one chain"
            );
            // The rollout's own session names its leaf.
            let locator = fragment
                .clone()
                .put::<UTF8String, _>(format!("session/{name}-2025"));
            assert!(
                exists!(pattern!(fragment.facts(), [{
                    _?receipt @ schema::source_projection::projects_to: tip,
                        schema::source_projection::source_locator: locator
                }])),
                "{name}"
            );
            threads.push(thread);
        }
        // Each replay is its ancestor's own thread; each continuation follows.
        assert_eq!(threads[0].len(), 3);
        assert_eq!(threads[1][..3], threads[0][..]);
        assert_eq!(threads[1].len(), 3 + 2);
        assert_eq!(threads[2][..5], threads[1][..]);
        assert_eq!(threads[2].len(), 5 + 2);
    }

    #[test]
    fn a_fork_adds_only_its_own_messages_and_occurrence_annotations() {
        let fork_text = fork_rollout();
        let (_parent_directory, parent) = project_fragments(PARENT_ROLLOUT, "parent.jsonl");
        let (_fork_directory, fork) = project_fragments(&fork_text, "fork.jsonl");

        let mut known = TribleSet::new();
        for fragment in &parent {
            known += fragment.facts().clone();
        }
        let novel = |fragment: &Fragment| fragment.facts().iter().any(|fact| !known.contains(fact));

        // Every fork node but its own two projects to a receipt closure the
        // parent already holds, node, fact and receipt alike: the replay's
        // copy times are not kept.
        let closures: Vec<&Fragment> = fork
            .iter()
            .filter(|fragment| has_tag(fragment, schema::block::KIND))
            .collect();
        assert_eq!(closures.len(), 5);
        assert_eq!(
            closures.iter().filter(|fragment| novel(fragment)).count(),
            2
        );

        // The only other fragment is the leaf's session: facts about a
        // receipt and nothing else.
        let mut union = Fragment::empty();
        for fragment in parent.iter().chain(&fork) {
            union += fragment.clone();
        }
        let receipts = ids_with_tag(&union, schema::source_projection::KIND);
        let occurrences: Vec<&Fragment> = fork
            .iter()
            .filter(|fragment| {
                fragment.root().is_some_and(|root| receipts.contains(&root))
                    && !has_tag(fragment, schema::source_projection::KIND)
            })
            .collect();
        assert_eq!(occurrences.len(), 1, "the leaf's session");
        for occurrence in occurrences {
            assert!(occurrence
                .facts()
                .iter()
                .all(|fact| receipts.contains(fact.e())));
        }

        // The shared prefix is projected once: one receipt per node. Each
        // replayed receipt carries its original record's time only.
        assert_eq!(receipts.len(), 5);
        assert_eq!(ids_with_tag(&union, schema::block::KIND).len(), 5);
        let copy_time =
            epoch_interval(parse_iso_timestamp("2026-08-03T09:00:00.500Z").unwrap()).unwrap();
        assert!(
            !exists!(pattern!(union.facts(), [{
                _?receipt @ schema::source_projection::source_timestamp: copy_time
            }])),
            "no receipt carries the time a record was copied"
        );
        let timed = receipts
            .iter()
            .filter(|receipt| {
                let receipt = **receipt;
                exists!(pattern!(union.facts(), [{
                    receipt @ schema::source_projection::source_timestamp: _?timestamp
                }]))
            })
            .count();
        assert_eq!(timed, 5, "the parent's three and the fork's own two");
        let (_reader_directory, reader) = empty_reader();
        let (_, validation) =
            blockdag::validate_catalog_union(&reader, &TribleSet::new(), &union).unwrap();
        assert_eq!(validation, blockdag::CatalogValidation::Accepted);

        // Through a real writer: after the parent's commit, the fork stages
        // the closures of its own two messages and no other.
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("archive.pile");
        let key = directory.path().join("archive.key");
        File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let parent_path = directory.path().join("parent.jsonl");
        let fork_path = directory.path().join("fork.jsonl");
        fs::write(&parent_path, PARENT_ROLLOUT).unwrap();
        fs::write(&fork_path, &fork_text).unwrap();

        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        project_path(&parent_path, |projected| {
            writer.stage_fragment(projected.fragment)
        })
        .unwrap();
        assert!(writer.commit_unit().unwrap().is_some());
        writer.close(Ok(())).unwrap();

        let mut writer = ArchiveImportWriter::open(&pile, Some(&key)).unwrap();
        let mut staged_closures = 0usize;
        project_path(&fork_path, |projected| {
            let closure = has_tag(&projected.fragment, schema::block::KIND);
            let before = writer.delta_len();
            writer.stage_fragment(projected.fragment)?;
            if closure && writer.delta_len() > before {
                staged_closures += 1;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(staged_closures, 2);
        assert!(writer.commit_unit().unwrap().is_some());
        writer.close(Ok(())).unwrap();

        let archive =
            pollster::block_on(crate::archive_collection::ensure_local(&pile, Some(&key))).unwrap();
        let facts = archive.facts().unwrap();
        assert_eq!(
            find!(
                receipt: Id,
                pattern!(&facts, [{
                    ?receipt @ metadata::tag: &schema::source_projection::KIND
                }])
            )
            .count(),
            5
        );
    }

    #[test]
    fn a_prompt_is_perceived_once_as_the_response_item_the_model_was_given() {
        // The response item carries the prompt and its image as the model got
        // them; the user-message event after it is the UI's notice.
        let with_item = concat!(
            r#"{"timestamp":"2026-09-10T08:00:00.000Z","type":"session_meta","payload":{"id":"thread-prompt"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-10T08:00:00.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<image name=[Image #1]>"},{"type":"input_image","image_url":"data:image/png;base64,iVBORw=="},{"type":"input_text","text":"</image>"},{"type":"input_text","text":"what is this?\n"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-09-10T08:00:00.200Z","type":"event_msg","payload":{"type":"user_message","message":"what is this?","images":[],"local_images":["/home/ada/shot.png"]}}"#,
            "\n",
            r#"{"timestamp":"2026-09-10T08:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"a picture"}}"#,
            "\n",
        );
        let (_directory, summary, fragment) = project_text(with_item, "prompt.jsonl");
        assert_eq!(summary.stats.mirrored_prompts, 1);
        use schema::content_fact::{direction, modality};
        let prompt = content_facts(&fragment, modality::TEXT, direction::IN);
        assert_eq!(
            prompt.len(),
            3,
            "the item's three texts, and no second copy"
        );
        // The image is the bytes the model was given, not the UI's path.
        assert!(!exists!(pattern!(fragment.facts(), [{
            _?fact @ schema::content_fact::asset_pointer: _?pointer
        }])));
        assert_eq!(
            content_facts(&fragment, modality::IMAGE, direction::IN).len(),
            1
        );

        // Without a response item (an older format) the event is the prompt.
        let event_only = with_item
            .lines()
            .filter(|line| !line.contains(r#""response_item""#))
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        let (_directory, summary, fragment) = project_text(&event_only, "event.jsonl");
        assert_eq!(summary.stats.mirrored_prompts, 0);
        assert_eq!(
            content_facts(&fragment, modality::TEXT, direction::IN).len(),
            1
        );
    }

    /// A prompt's event written before the response item that carries it,
    /// with harness context between them.
    const EVENT_FIRST_ROLLOUT: &str = concat!(
        r#"{"timestamp":"2026-09-20T08:00:00.000Z","type":"session_meta","payload":{"id":"thread-event-first"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-20T08:00:00.001Z","type":"turn_context","payload":{"model":"gpt-test"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-20T08:00:00.002Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"be brief"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-20T08:00:00.003Z","type":"event_msg","payload":{"type":"user_message","message":"hello","images":[],"local_images":[]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-20T08:00:00.004Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment>here</environment>"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-20T08:00:00.005Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-20T08:00:00.500Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"answer it"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-20T08:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"hi"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-20T08:00:01.001Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
        "\n",
    );

    #[test]
    fn a_longer_rollout_only_appends_nodes() {
        // Each prefix of a rollout cut at a line end holds exactly a prefix
        // of the whole one's thread, with no dead end: what is not known yet
        // is held back.
        for (rollout, expected_held) in [
            (ROLLOUT, None),
            (ITEM_ROLLOUT, None),
            (LEGACY_ROLLOUT, None),
            (PARENT_ROLLOUT, None),
            // Cut after the event, and after the context behind it: the event
            // may yet be a mirror, so it and what follows wait.
            (EVENT_FIRST_ROLLOUT, Some(vec![0, 0, 0, 1, 2, 0, 0, 0, 0])),
            (GRAND_2025, None),
            (
                &resume_2025(
                    "parent-2025",
                    "2025-12-11T01:58:00.000Z",
                    GRAND_2025,
                    "second",
                ),
                None,
            ),
        ] {
            let (_whole_directory, whole_summary, whole) = project_text(rollout, "grow.jsonl");
            assert_eq!(whole_summary.stats.held_back_records, 0);
            let thread = blockdag::thread_to(whole.facts(), leaf(&whole));
            assert_eq!(
                ids_with_tag(&whole, schema::block::KIND),
                thread.iter().copied().collect::<BTreeSet<_>>()
            );
            let whole_receipts = ids_with_tag(&whole, schema::source_projection::KIND);
            let lines: Vec<&str> = rollout.split_inclusive('\n').collect();
            let mut held = Vec::new();
            for cut in 1..=lines.len() {
                let prefix: String = lines[..cut].concat();
                let (_directory, summary, part) = project_text(&prefix, "grow.jsonl");
                let nodes = ids_with_tag(&part, schema::block::KIND);
                assert_eq!(
                    nodes,
                    thread[..nodes.len()]
                        .iter()
                        .copied()
                        .collect::<BTreeSet<_>>(),
                    "cut after line {cut}: a prefix of the whole thread"
                );
                assert!(
                    ids_with_tag(&part, schema::source_projection::KIND).is_subset(&whole_receipts),
                    "cut after line {cut}"
                );
                held.push(summary.stats.held_back_records);
            }
            if let Some(expected) = expected_held {
                assert_eq!(held, expected);
            }
        }
        // The event is the response item's mirror once both are written.
        let (_directory, summary, _) = project_text(EVENT_FIRST_ROLLOUT, "whole.jsonl");
        assert_eq!(summary.stats.mirrored_prompts, 1);
    }

    #[test]
    fn a_finished_rollout_emits_its_trailing_event() {
        let lines: Vec<&str> = EVENT_FIRST_ROLLOUT.split_inclusive('\n').collect();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stopped.jsonl");
        fs::write(&path, lines[..4].concat()).unwrap();
        let project = |finished_before| {
            let mut fragment = Fragment::empty();
            let summary = project_path_finished(&path, finished_before, |projected| {
                fragment += projected.fragment;
                Ok(())
            })
            .unwrap();
            (summary, fragment)
        };
        let (held, held_fragment) = project(None);
        assert_eq!(held.stats.held_back_records, 1);
        let cutoff = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        let (flushed, flushed_fragment) = project(Some(cutoff));
        assert_eq!(flushed.stats.held_back_records, 0);
        assert_eq!(
            ids_with_tag(&flushed_fragment, schema::block::KIND).len(),
            ids_with_tag(&held_fragment, schema::block::KIND).len() + 1,
            "the event is emitted after the context before it"
        );
        use schema::content_fact::{direction, modality};
        // The instruction before it, and now the prompt.
        assert_eq!(
            content_facts(&held_fragment, modality::TEXT, direction::IN).len(),
            1
        );
        assert_eq!(
            content_facts(&flushed_fragment, modality::TEXT, direction::IN).len(),
            2
        );
    }

    #[test]
    fn a_compaction_is_perceived() {
        let rollout = concat!(
            r#"{"timestamp":"2026-09-10T08:00:00.000Z","type":"session_meta","payload":{"id":"thread-compact"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-10T08:00:00.100Z","type":"event_msg","payload":{"type":"agent_message","message":"working"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-10T08:00:00.200Z","type":"compacted","payload":{"message":"summary of the work so far","replacement_history":[{"type":"compaction","encrypted_content":"sealed-history"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-09-10T08:00:00.300Z","type":"event_msg","payload":{"type":"agent_message","message":"continuing"}}"#,
            "\n",
        );
        let (_directory, summary, fragment) = project_text(rollout, "compact.jsonl");
        assert_eq!(
            summary.stats.source_projections, 4,
            "the message; the compaction's summary and sealed history; the message"
        );
        use schema::content_fact::{direction, modality};
        assert_eq!(
            content_facts(&fragment, modality::TEXT, direction::IN).len(),
            1
        );
        assert_eq!(
            content_facts(&fragment, modality::THINKING, direction::IN).len(),
            1
        );
        assert_eq!(
            content_facts(&fragment, modality::TEXT, direction::OUT).len(),
            2
        );
    }

    #[test]
    fn call_ids_and_envelope_times_stay_out_of_every_hashed_text() {
        let rollout = |call: &str, created: &str| {
            format!(
                concat!(
                    r#"{{"timestamp":"2026-09-10T08:00:00.000Z","type":"session_meta","payload":{{"id":"thread-ids"}}}}"#,
                    "\n",
                    r#"{{"timestamp":"2026-09-10T08:00:00.100Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"run it"}}]}}}}"#,
                    "\n",
                    r#"{{"timestamp":"2026-09-10T08:00:00.200Z","type":"response_item","payload":{{"type":"function_call","id":"fc_{call}","call_id":"{call}","name":"shell","arguments":"{{}}","internal_chat_message_metadata_passthrough":{{"create_time":{created}}}}}}}"#,
                    "\n",
                    r#"{{"timestamp":"2026-09-10T08:00:00.300Z","type":"response_item","payload":{{"type":"function_call_output","id":"out_{call}","call_id":"{call}","output":"done"}}}}"#,
                    "\n",
                    r#"{{"timestamp":"2026-09-10T08:00:00.400Z","type":"response_item","payload":{{"type":"agent_message","id":"amsg_{call}","author":"/root/peer","recipient":"/root","content":[{{"type":"input_text","text":"peer says hi"}}],"internal_chat_message_metadata_passthrough":{{"create_time":{created}}},"created_at":"{created}"}}}}"#,
                    "\n",
                ),
                call = call,
                created = created,
            )
        };
        let (_first_directory, first_summary, first) =
            project_text(&rollout("call_1", "1788691762.1"), "first.jsonl");
        let (_second_directory, _, second) =
            project_text(&rollout("call_2", "1799999999.9"), "second.jsonl");
        assert_eq!(first_summary.stats.unresolved_tool_results, 0);
        assert_eq!(
            ids_with_tag(&first, schema::block::KIND),
            ids_with_tag(&second, schema::block::KIND)
        );
        assert_eq!(
            ids_with_tag(&first, schema::content_fact::KIND),
            ids_with_tag(&second, schema::content_fact::KIND)
        );
        assert!(exists!(pattern!(first.facts(), [{
            _?output @ schema::block::responds_to: _?call
        }])));
    }
}
