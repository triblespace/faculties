//! Streaming projection of one Codex app-server rollout onto Archive's
//! canonical block DAG.
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
//! project to the same content facts; the new one only gathers a message's
//! parts, or a reasoning item's summary parts, into one record and so into one
//! block. `Reasoning.raw_content` mirrors the old `agent_reasoning_raw_content`
//! event, which was never projected either.
//!
//! A Codex fork or resume replays its parent's records into a new file under
//! new timestamps. A block is its predecessors and its content, and a receipt
//! is the source namespace and the block, so a replayed message lands on the
//! parent's block and receipt and adds at most its replay time.
//!
//! Dialogue (the event stream above, or, in a file without events, the
//! `response_item` messages and reasoning summaries) threads the DAG. Tool
//! calls and their outputs, harness context (developer and user-role
//! `response_item` messages), inter-agent messages, compaction summaries and
//! `world_state` project as side branches: they follow the latest dialogue
//! block (and the side record before them), but the next dialogue block does
//! not follow them. A fork whose replay carries only the event stream therefore
//! threads the same dialogue blocks as its parent's original file.
//!
//! No source bytes are stored. Skipped, with the reason:
//! `token_count`, `token_usage_record`, `task_started`, `task_complete`,
//! `turn_aborted`, `thread_settings_applied`, `inter_agent_communication_metadata`
//! and legacy `record_type/state` are bookkeeping; `response_item/reasoning`
//! in a file with events mirrors the `agent_reasoning`/`Reasoning` events, and
//! its `encrypted_content` is a sealed token only the provider can decrypt;
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
//! `ArchiveImportWriter` can publish its COMMIT.

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

use crate::schemas::blockdag as schema;
use crate::{archive_source, blockdag, files};

/// Observable projection accounting. Non-dialogue records are contracted, not
/// silently mistaken for additional messages.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectionStats {
    /// Non-empty, newline-terminated JSONL records in the frozen prefix.
    pub records_seen: usize,
    /// Projections emitted for dialogue records.
    pub source_projections: usize,
    /// Side-branch projections (tools, harness context, compactions).
    pub side_projections: usize,
    /// Ordered content parts emitted before set-level deduplication.
    pub content_parts: usize,
    /// Bookkeeping, mirrors and other rows that project to nothing.
    pub skipped_records: usize,
    /// Source timestamp strings that were present but not decodable.
    pub invalid_timestamps: usize,
    /// Data-URI assets whose bytes could not be decoded.
    pub undecodable_assets: usize,
}

impl ProjectionStats {
    fn since(self, earlier: Self) -> Self {
        Self {
            records_seen: self.records_seen.saturating_sub(earlier.records_seen),
            source_projections: self
                .source_projections
                .saturating_sub(earlier.source_projections),
            side_projections: self
                .side_projections
                .saturating_sub(earlier.side_projections),
            content_parts: self.content_parts.saturating_sub(earlier.content_parts),
            skipped_records: self.skipped_records.saturating_sub(earlier.skipped_records),
            invalid_timestamps: self
                .invalid_timestamps
                .saturating_sub(earlier.invalid_timestamps),
            undecodable_assets: self
                .undecodable_assets
                .saturating_sub(earlier.undecodable_assets),
        }
    }
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
    /// Head of the dialogue chain.
    previous_block: Option<Id>,
    /// Head of the side chain hanging off the current dialogue block.
    side_head: Option<Id>,
    /// Receipt of the last dialogue block, the file's leaf.
    leaf: Option<Id>,
    /// Content part of each tool call, by call id, for `responds_to`.
    call_parts: std::collections::HashMap<String, Id>,
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

/// The two fragments one projected record yields.
struct ProjectedRecord {
    /// The receipt's intrinsic closure (content fact, part, block, receipt)
    /// with the annotations a replay of the record reproduces exactly.
    receipt: Fragment,
    /// This occurrence of it: its (replay-varying) timestamp.
    occurrence: Fragment,
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
    if path.is_dir() {
        bail!(
            "Codex Archive import requires one explicit rollout JSONL file; recursive session-directory ingestion is intentionally unsupported"
        );
    }

    let plan = plan_prefix(path)?;
    let frozen = archive_source::freeze_prefix(path, plan.observed_bytes)?;
    if frozen.digest != plan.digest {
        bail!(
            "Codex rollout {} changed inside its planned prefix before projection",
            path.display()
        );
    }
    project_planned(path, plan, frozen, emit)
}

fn project_planned<F>(
    path: &Path,
    plan: PrefixPlan,
    frozen: archive_source::FrozenSource,
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
        previous_block: None,
        side_head: None,
        leaf: None,
        call_parts: std::collections::HashMap::new(),
        stats: ProjectionStats::default(),
    };
    let mut fragments_emitted = 0usize;
    let projected = scan_frozen_prefix(&frozen.bytes, |line, raw| {
        let before = projector.stats;
        if let Some(record) = projector.project_record(line, raw)? {
            let mut record_stats = projector.stats.since(before);
            record_stats.records_seen = 1;
            // Two fragments, so a writer that already holds the receipt's
            // closure (a fork replaying its parent) skips that closure whole
            // and stages only the occurrence.
            emit(ProjectedFile {
                source_path: path.to_path_buf(),
                fragment: record.receipt,
                stats: record_stats,
            })?;
            emit(ProjectedFile {
                source_path: path.to_path_buf(),
                fragment: record.occurrence,
                stats: ProjectionStats::default(),
            })?;
            fragments_emitted += 2;
        }
        Ok(())
    })?;
    if projected.complete_bytes != plan.complete_bytes {
        bail!(
            "Codex rollout {} frozen-prefix length changed during projection",
            path.display()
        );
    }
    projector.stats.records_seen = projected.records_seen;

    // The session id and path annotate the file's leaf only.
    if let Some(leaf) = projector.leaf {
        emit(ProjectedFile {
            source_path: path.to_path_buf(),
            fragment: blockdag::source_occurrence(
                leaf,
                blockdag::ProjectionAnnotations {
                    source_path: Some(path.to_string_lossy().into_owned()),
                    source_locator: Some(format!("session/{}", plan.session_id)),
                    ..blockdag::ProjectionAnnotations::default()
                },
            )?,
            stats: ProjectionStats::default(),
        })?;
        fragments_emitted += 1;
    }

    let stats = projector.stats;
    let summary = ProjectionSummary {
        files_scanned: 1,
        fragments_emitted,
        frozen_bytes: plan.observed_bytes,
        trailing_bytes_ignored: plan.trailing_bytes,
        stats,
        ..ProjectionSummary::default()
    };
    Ok(summary)
}

/// What the planning pass learns from the records of one rollout.
#[derive(Default)]
struct PlanScan {
    session_id: Option<String>,
    legacy_id: Option<String>,
    has_events: bool,
    first: bool,
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
        let payload = record
            .payload
            .as_ref()
            .ok_or_else(|| anyhow!("Codex session_meta has no object field \"payload\""))?;
        let id = consistent_session_id(payload)?;
        match &self.session_id {
            Some(previous) if previous != &id => bail!(
                "Codex rollout {} contains conflicting session ids {:?} and {:?}",
                path.display(),
                previous,
                id
            ),
            Some(_) => {}
            None => self.session_id = Some(id),
        }
        Ok(())
    }

    /// The session id, and whether the file is in the legacy bare format.
    fn finish(self, path: &Path) -> Result<(String, bool, bool)> {
        match (self.session_id, self.legacy_id) {
            (Some(id), _) => Ok((id, false, self.has_events)),
            (None, Some(id)) => Ok((id, true, self.has_events)),
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
    let (session_id, legacy, has_events) = planned.finish(path)?;
    let observed_bytes = u64::try_from(bytes.len()).context("resident Codex length exceeds u64")?;
    let digest = *blake3::hash(bytes.as_ref()).as_bytes();
    let plan = PrefixPlan {
        session_id,
        has_events,
        legacy,
        observed_bytes,
        complete_bytes: scan.complete_bytes,
        trailing_bytes: observed_bytes.saturating_sub(scan.complete_bytes),
        digest,
    };
    project_planned(
        path,
        plan,
        archive_source::FrozenSource { bytes, digest },
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
    let (session_id, legacy, has_events) = planned.finish(path)?;
    Ok(PrefixPlan {
        session_id,
        has_events,
        legacy,
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
    fn project_record(&mut self, _line: u64, raw: Bytes) -> Result<Option<ProjectedRecord>> {
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
                return Ok(None);
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
                )?,
                "user_message",
            ),
            Some("agent_message") => (
                project_message_parts(
                    payload,
                    schema::content_fact::direction::OUT,
                    &mut self.stats,
                )?,
                "agent_message",
            ),
            Some("agent_reasoning") => (
                project_reasoning_parts(payload, &mut self.stats)?,
                "agent_reasoning",
            ),
            Some("item_completed") => {
                let Some(item) = payload.item.as_ref() else {
                    self.stats.skipped_records += 1;
                    return Ok(None);
                };
                match item.item_type.as_deref() {
                    Some("UserMessage") => (
                        project_item_message_parts(
                            item,
                            schema::content_fact::direction::IN,
                            &mut self.stats,
                        )?,
                        "UserMessage",
                    ),
                    Some("AgentMessage") => (
                        project_item_message_parts(
                            item,
                            schema::content_fact::direction::OUT,
                            &mut self.stats,
                        )?,
                        "AgentMessage",
                    ),
                    Some("Reasoning") => (
                        project_item_reasoning_parts(item, &mut self.stats)?,
                        "Reasoning",
                    ),
                    _ => {
                        self.stats.skipped_records += 1;
                        return Ok(None);
                    }
                }
            }
            _ => {
                self.stats.skipped_records += 1;
                return Ok(None);
            }
        };
        let timestamp = self.timestamp(record.timestamp.as_deref());
        self.dialogue(parts, raw_role, timestamp)
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

    /// Emit one dialogue block: it follows the previous dialogue block.
    fn dialogue(
        &mut self,
        parts: Fragment,
        raw_role: &str,
        timestamp: Option<Inline<NsTAIInterval>>,
    ) -> Result<Option<ProjectedRecord>> {
        if parts.exports().next().is_none() {
            self.stats.skipped_records += 1;
            return Ok(None);
        }
        // Codex child rollouts replay inherited records while rewriting their
        // observation time. Time therefore annotates the receipt, never the
        // shared block.
        let block = blockdag::block(self.previous_block, None, parts)?;
        let block_id = block
            .root()
            .expect("canonical block constructor returns one root");
        let record = self.receipt(block, raw_role, timestamp)?;
        self.previous_block = Some(block_id);
        self.side_head = None;
        self.leaf = Some(record.receipt.root().expect("receipt is rooted"));
        self.stats.source_projections += 1;
        Ok(Some(record))
    }

    /// Emit one side-branch block: it follows the side record before it, or
    /// the latest dialogue block, and no dialogue block follows it.
    fn side(
        &mut self,
        parts: Fragment,
        raw_role: &str,
        timestamp: Option<Inline<NsTAIInterval>>,
    ) -> Result<Option<ProjectedRecord>> {
        if parts.exports().next().is_none() {
            self.stats.skipped_records += 1;
            return Ok(None);
        }
        let block = blockdag::block(self.side_head.or(self.previous_block), None, parts)?;
        let block_id = block
            .root()
            .expect("canonical block constructor returns one root");
        let record = self.receipt(block, raw_role, timestamp)?;
        self.side_head = Some(block_id);
        self.stats.side_projections += 1;
        Ok(Some(record))
    }

    /// The receipt is the namespace and the block; role and model ride with
    /// it, and the replay-varying time is a fragment of its own.
    fn receipt(
        &mut self,
        block: Fragment,
        raw_role: &str,
        timestamp: Option<Inline<NsTAIInterval>>,
    ) -> Result<ProjectedRecord> {
        let receipt =
            blockdag::block_source_projection(schema::source_projection::SOURCE_CODEX, block)?;
        let receipt = blockdag::annotate_source_projection(
            receipt,
            blockdag::ProjectionAnnotations {
                raw_role: Some(raw_role.to_owned()),
                raw_model: self.current_model.clone(),
                ..blockdag::ProjectionAnnotations::default()
            },
        )?;
        let receipt_id = receipt
            .root()
            .expect("canonical source-projection constructor returns one root");
        let occurrence = blockdag::source_occurrence(
            receipt_id,
            blockdag::ProjectionAnnotations {
                source_timestamp: timestamp,
                ..blockdag::ProjectionAnnotations::default()
            },
        )?;
        Ok(ProjectedRecord {
            receipt,
            occurrence,
        })
    }

    /// Project one non-event record: a response item, a compaction, a world
    /// state, or (legacy format) a bare response item.
    fn project_item_line(&mut self, raw: Bytes) -> Result<Option<ProjectedRecord>> {
        let cheap = raw.as_ref();
        if !self.legacy
            && !contains(cheap, b"\"response_item\"")
            && !contains(cheap, b"\"compacted\"")
            && !contains(cheap, b"\"world_state\"")
        {
            self.stats.skipped_records += 1;
            return Ok(None);
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
                let parts = match payload.get("message").and_then(Value::as_str) {
                    Some(message) if !message.trim().is_empty() => {
                        let mut parts = Fragment::empty();
                        let mut ordinal = 0;
                        push_part(
                            &mut parts,
                            &mut ordinal,
                            blockdag::text_fact(
                                schema::content_fact::modality::TEXT,
                                schema::content_fact::direction::AMBIENT,
                                message,
                            )?,
                            &mut self.stats,
                        )?;
                        parts
                    }
                    _ => Fragment::empty(),
                };
                self.side(parts, "compacted", timestamp)
            }
            "world_state" => {
                let parts = event_parts(
                    payload,
                    &[],
                    schema::content_fact::direction::IN,
                    &mut self.stats,
                )?;
                self.side(parts, "world_state", timestamp)
            }
            _ => {
                self.stats.skipped_records += 1;
                Ok(None)
            }
        }
    }

    fn response_item(
        &mut self,
        payload: &Value,
        timestamp: Option<Inline<NsTAIInterval>>,
    ) -> Result<Option<ProjectedRecord>> {
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
                    return Ok(None);
                }
                let parts = response_content_parts(
                    payload.get("content"),
                    schema::content_fact::modality::TEXT,
                    direction,
                    &mut self.stats,
                )?;
                match role {
                    "user" | "assistant" if !self.has_events => {
                        self.dialogue(parts, role, timestamp)
                    }
                    // User-role items are the typed prompt and the harness
                    // context around it; developer and system items are
                    // instructions. All of it is what the model saw.
                    _ => self.side(parts, role, timestamp),
                }
            }
            "reasoning" => {
                if self.has_events {
                    self.stats.skipped_records += 1;
                    return Ok(None);
                }
                let mut parts = Fragment::empty();
                let mut ordinal = 0;
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
                        push_part(
                            &mut parts,
                            &mut ordinal,
                            blockdag::text_fact(
                                schema::content_fact::modality::THINKING,
                                schema::content_fact::direction::OUT,
                                text,
                            )?,
                            &mut self.stats,
                        )?;
                    }
                }
                self.dialogue(parts, "reasoning", timestamp)
            }
            // `ghost_snapshot` holds git object references; a record without
            // a kind is the legacy header or a legacy `record_type/state`.
            "ghost_snapshot" | "" => {
                self.stats.skipped_records += 1;
                Ok(None)
            }
            kind if kind.ends_with("_output") => {
                let call_id = payload.get("call_id").and_then(Value::as_str);
                let responds_to = call_id.and_then(|id| self.call_parts.get(id).copied());
                let parts = tool_output_parts(payload.get("output"), responds_to, &mut self.stats)?;
                self.side(parts, kind, timestamp)
            }
            kind if kind.ends_with("_call") => {
                let body = canonical_value(&without(payload, CALL_ENVELOPE))?;
                let fact = blockdag::text_fact(
                    schema::content_fact::modality::TOOL_CALL,
                    schema::content_fact::direction::OUT,
                    body,
                )?;
                let part = blockdag::content_part(0, fact, None)?;
                self.stats.content_parts += 1;
                if let Some(call_id) = payload.get("call_id").and_then(Value::as_str) {
                    self.call_parts
                        .insert(call_id.to_owned(), part.root().expect("part is rooted"));
                }
                self.side(part, kind, timestamp)
            }
            // Inter-agent messages and any item kind not named above.
            _ => {
                let parts = event_parts(
                    payload,
                    CALL_ENVELOPE,
                    schema::content_fact::direction::IN,
                    &mut self.stats,
                )?;
                self.side(parts, kind, timestamp)
            }
        }
    }
}

/// Fields of a response item that are envelope, not content.
const CALL_ENVELOPE: &[&str] = &[
    "id",
    "call_id",
    "status",
    "internal_chat_message_metadata_passthrough",
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

/// One EVENT part holding the canonical JSON of `value` without `keys`.
fn event_parts(
    value: &Value,
    keys: &[&str],
    direction: Id,
    stats: &mut ProjectionStats,
) -> Result<Fragment> {
    let body = canonical_value(&without(value, keys))?;
    let mut parts = Fragment::empty();
    if body != "{}" && body != "null" {
        let mut ordinal = 0;
        push_part(
            &mut parts,
            &mut ordinal,
            blockdag::text_fact(schema::content_fact::modality::EVENT, direction, body)?,
            stats,
        )?;
    }
    Ok(parts)
}

/// Text and image items of a response item's `content`, in order.
fn response_content_facts(
    content: Option<&Value>,
    modality: Id,
    direction: Id,
    stats: &mut ProjectionStats,
) -> Result<Vec<Fragment>> {
    let items: Vec<&Value> = match content {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    };
    let mut facts = Vec::new();
    for item in items {
        let fact = match item {
            Value::String(text) if !text.trim().is_empty() => {
                Some(blockdag::text_fact(modality, direction, text.as_str())?)
            }
            Value::Object(_) => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    (!text.trim().is_empty())
                        .then(|| blockdag::text_fact(modality, direction, text))
                        .transpose()?
                } else if let Some(url) = item
                    .get("image_url")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("url").and_then(Value::as_str))
                {
                    project_asset(url, schema::content_fact::modality::IMAGE, direction, stats)?
                } else {
                    Some(blockdag::text_fact(
                        schema::content_fact::modality::EVENT,
                        direction,
                        canonical_value(item)?,
                    )?)
                }
            }
            _ => None,
        };
        facts.extend(fact);
    }
    Ok(facts)
}

fn canonical_value(value: &Value) -> Result<String> {
    archive_source::canonical_json(Bytes::from_source(serde_json::to_vec(value)?))
        .map_err(|error| anyhow!("canonicalize Codex JSON: {error}"))
}

/// Wrap facts into ordered parts, each optionally answering one call part.
fn wrap_parts(
    facts: Vec<Fragment>,
    responds_to: Option<Id>,
    stats: &mut ProjectionStats,
) -> Result<Fragment> {
    let mut parts = Fragment::empty();
    for (ordinal, fact) in facts.into_iter().enumerate() {
        let ordinal = u64::try_from(ordinal).context("content-part ordinal exceeds u64")?;
        parts += blockdag::content_part(ordinal, fact, responds_to)?;
        stats.content_parts += 1;
    }
    Ok(parts)
}

fn response_content_parts(
    content: Option<&Value>,
    modality: Id,
    direction: Id,
    stats: &mut ProjectionStats,
) -> Result<Fragment> {
    let facts = response_content_facts(content, modality, direction, stats)?;
    wrap_parts(facts, None, stats)
}

/// The parts of a tool output: its text and images, each answering the call.
fn tool_output_parts(
    output: Option<&Value>,
    responds_to: Option<Id>,
    stats: &mut ProjectionStats,
) -> Result<Fragment> {
    let direction = schema::content_fact::direction::IN;
    let modality = schema::content_fact::modality::TOOL_RESULT;
    let facts = match output {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![blockdag::text_fact(modality, direction, text.as_str())?]
        }
        Some(Value::String(_)) | Some(Value::Null) | None => Vec::new(),
        Some(Value::Array(_)) => response_content_facts(output, modality, direction, stats)?,
        Some(other) => vec![blockdag::text_fact(
            modality,
            direction,
            canonical_value(other)?,
        )?],
    };
    wrap_parts(facts, responds_to, stats)
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
) -> Result<Fragment> {
    let mut parts = Fragment::empty();
    let mut ordinal = 0u64;
    for content in &item.content {
        let fact = match content.content_type.as_deref() {
            Some("text" | "Text") => match content
                .text
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                Some(text) => Some(blockdag::text_fact(
                    schema::content_fact::modality::TEXT,
                    direction,
                    text,
                )?),
                None => None,
            },
            Some("image") => match content.image_url.as_ref().or(content.url.as_ref()) {
                Some(pointer) => project_asset(
                    pointer,
                    schema::content_fact::modality::IMAGE,
                    direction,
                    stats,
                )?,
                None => None,
            },
            Some("localImage" | "local_image") => match content.path.as_ref() {
                Some(pointer) => project_asset(
                    pointer,
                    schema::content_fact::modality::IMAGE,
                    direction,
                    stats,
                )?,
                None => None,
            },
            _ => None,
        };
        if let Some(fact) = fact {
            push_part(&mut parts, &mut ordinal, fact, stats)?;
        }
    }
    Ok(parts)
}

/// Project a 2026-09 `Reasoning` item's summary parts, in order: each is what
/// one old-format `agent_reasoning` event carried.
fn project_item_reasoning_parts(item: &CodexItem, stats: &mut ProjectionStats) -> Result<Fragment> {
    let mut parts = Fragment::empty();
    let mut ordinal = 0u64;
    for text in item
        .summary_text
        .iter()
        .filter(|text| !text.trim().is_empty())
    {
        push_part(
            &mut parts,
            &mut ordinal,
            blockdag::text_fact(
                schema::content_fact::modality::THINKING,
                schema::content_fact::direction::OUT,
                text.as_ref(),
            )?,
            stats,
        )?;
    }
    Ok(parts)
}

fn project_reasoning_parts(
    payload: &CodexPayload,
    stats: &mut ProjectionStats,
) -> Result<Fragment> {
    let mut parts = Fragment::empty();
    let Some(text) = payload
        .text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    else {
        return Ok(parts);
    };
    let mut ordinal = 0;
    push_part(
        &mut parts,
        &mut ordinal,
        blockdag::text_fact(
            schema::content_fact::modality::THINKING,
            schema::content_fact::direction::OUT,
            text,
        )?,
        stats,
    )?;
    Ok(parts)
}

fn project_message_parts(
    payload: &CodexPayload,
    direction: Id,
    stats: &mut ProjectionStats,
) -> Result<Fragment> {
    let mut parts = Fragment::empty();
    let mut ordinal = 0u64;
    if let Some(message) = payload
        .message
        .as_deref()
        .filter(|message| !message.trim().is_empty())
    {
        push_part(
            &mut parts,
            &mut ordinal,
            blockdag::text_fact(schema::content_fact::modality::TEXT, direction, message)?,
            stats,
        )?;
    }

    for pointer in payload.images.iter().chain(&payload.local_images) {
        if let Some(fact) = project_asset(
            pointer,
            schema::content_fact::modality::IMAGE,
            direction,
            stats,
        )? {
            push_part(&mut parts, &mut ordinal, fact, stats)?;
        }
    }
    for pointer in payload.audio.iter().chain(&payload.local_audio) {
        if let Some(fact) = project_asset(
            pointer,
            schema::content_fact::modality::AUDIO,
            direction,
            stats,
        )? {
            push_part(&mut parts, &mut ordinal, fact, stats)?;
        }
    }
    Ok(parts)
}

fn push_part(
    parts: &mut Fragment,
    ordinal: &mut u64,
    fact: Fragment,
    stats: &mut ProjectionStats,
) -> Result<()> {
    *parts += blockdag::content_part(*ordinal, fact, None)?;
    *ordinal = ordinal
        .checked_add(1)
        .ok_or_else(|| anyhow!("Codex message has more than u64::MAX content parts"))?;
    stats.content_parts += 1;
    Ok(())
}

fn project_asset(
    pointer: &str,
    modality: Id,
    direction: Id,
    stats: &mut ProjectionStats,
) -> Result<Option<Fragment>> {
    if let Some(data) = pointer.strip_prefix("data:") {
        let Some((header, payload)) = data.split_once(',') else {
            stats.undecodable_assets += 1;
            return Ok(None);
        };
        if !header
            .split(';')
            .any(|component| component.eq_ignore_ascii_case("base64"))
        {
            stats.undecodable_assets += 1;
            return Ok(None);
        }
        let bytes = match BASE64_STANDARD.decode(payload.as_bytes()) {
            Ok(bytes) => bytes,
            Err(_) => {
                stats.undecodable_assets += 1;
                return Ok(None);
            }
        };
        let media_type = header
            .split(';')
            .next()
            .filter(|value| !value.trim().is_empty())
            .map(files::normalize_media_type_or_default)
            .unwrap_or_else(|| files::DEFAULT_MEDIA_TYPE.to_owned());
        return Ok(Some(blockdag::blob_fact(
            modality,
            direction,
            bytes,
            &media_type,
        )?));
    }

    Ok(Some(blockdag::asset_pointer_fact(
        modality,
        direction,
        schema::source_projection::SOURCE_CODEX,
        pointer.to_owned(),
        None,
        None,
    )?))
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
    use triblespace::prelude::blobencodings::UTF8String;
    use triblespace::prelude::inlineencodings::Handle;

    use super::*;
    use crate::archive_collection::ArchiveImportWriter;
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
    fn visible_events_thread_the_dialogue_and_harness_items_branch_off_it() {
        let (_directory, summary, fragment) = project_text(ROLLOUT, "rollout.jsonl");
        assert_eq!(summary.stats.source_projections, 3);
        assert_eq!(
            summary.stats.side_projections, 1,
            "the user-role response item"
        );
        assert_eq!(summary.stats.content_parts, 5);
        assert_eq!(summary.stats.records_seen, 8);
        // A receipt and an occurrence per projected record, then the leaf.
        assert_eq!(summary.fragments_emitted, 9);
        assert_eq!(summary.stats.skipped_records, 4);
        assert_eq!(
            ids_with_tag(&fragment, schema::source_projection::KIND).len(),
            4
        );
        assert_eq!(ids_with_tag(&fragment, schema::block::KIND).len(), 4);
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
            4
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
    fn sealed_only_reasoning_and_bookkeeping_project_nothing() {
        let source = chunked_telemetry_rollout(256 * 1024);
        let (_directory, summary, fragment) = project_text(&source, "sealed.jsonl");
        assert_eq!(summary.stats.records_seen, 1_026);
        assert_eq!(summary.stats.skipped_records, 1_026);
        assert_eq!(summary.fragments_emitted, 0);
        assert!(fragment.facts().is_empty());
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
            summary.stats.source_projections, 3,
            "user, reasoning, assistant"
        );
        assert_eq!(summary.stats.side_projections, 2, "the call and its output");
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
            1
        );
        // The output answers the call through a semantic edge, not the id.
        let call_part = find!(
            part: Id,
            pattern!(fragment.facts(), [
                { ?part @ schema::content_part::fact: _?fact },
                { _?fact @ schema::content_fact::modality: &modality::TOOL_CALL },
            ])
        )
        .next()
        .unwrap();
        assert!(exists!(pattern!(fragment.facts(), [{
            _?answer @ schema::content_part::responds_to: call_part
        }])));
        // The tool exhaust hangs off the reasoning; the answer follows the
        // reasoning, not the output.
        let (_lean_directory, _, lean) = project_text(
            &LEGACY_ROLLOUT
                .lines()
                .filter(|line| !line.contains("function_call"))
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
            "lean.jsonl",
        );
        assert!(ids_with_tag(&lean, schema::block::KIND)
            .is_subset(&ids_with_tag(&fragment, schema::block::KIND)));
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
            4
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
        assert_eq!(summary.stats.source_projections, 3);
        assert_eq!(summary.stats.side_projections, 1);
        // text + image, two summary parts, one text, and the harness text.
        assert_eq!(summary.stats.content_parts, 6);
        assert_eq!(
            summary.stats.skipped_records, 10,
            "bookkeeping, tool items, the empty reasoning item and response mirrors"
        );
        assert_eq!(summary.fragments_emitted, 9);
        assert_eq!(
            ids_with_tag(&fragment, schema::source_projection::KIND).len(),
            4
        );
        assert_eq!(ids_with_tag(&fragment, schema::block::KIND).len(), 4);
        assert_eq!(
            root_blocks(&fragment).len(),
            2,
            "one chain of three blocks, and the harness item before it"
        );

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
            content_facts(&fragment, modality::IMAGE, direction::IN).len(),
            1
        );
        assert_eq!(
            content_facts(&fragment, modality::THINKING, direction::OUT).len(),
            2
        );
        assert_eq!(
            content_facts(&fragment, modality::TEXT, direction::OUT).len(),
            1
        );

        // Both summary parts land in one block, in their source order.
        let mut blobs = fragment.blobs().clone();
        let blob_reader = blobs.snapshot().unwrap();
        let mut thinking: Vec<(u64, Inline<Handle<UTF8String>>)> = find!(
            (ordinal: u64, payload: Inline<Handle<UTF8String>>),
            pattern!(fragment.facts(), [
                { _?block @ schema::block::contains: _?part },
                { _?part @
                    schema::content_part::ordinal: ?ordinal,
                    schema::content_part::fact: _?fact,
                },
                { _?fact @
                    schema::content_fact::modality: &modality::THINKING,
                    schema::content_fact::payload: ?payload,
                },
            ])
        )
        .collect();
        thinking.sort();
        let thinking: Vec<String> = thinking
            .into_iter()
            .map(|(_, payload)| {
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
            (modality::IMAGE, direction::IN),
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

        // Every fork record but its own two projects to a receipt closure the
        // parent already holds, block, parts, facts and receipt alike.
        let closures: Vec<&Fragment> = fork
            .iter()
            .filter(|fragment| has_tag(fragment, schema::block::KIND))
            .collect();
        assert_eq!(closures.len(), 5);
        assert_eq!(
            closures.iter().filter(|fragment| novel(fragment)).count(),
            2
        );

        // What is new for the replayed three is their occurrence: facts about
        // a receipt and nothing else.
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
        assert_eq!(occurrences.len(), 6, "five times and the leaf's session");
        for occurrence in occurrences {
            assert!(occurrence
                .facts()
                .iter()
                .all(|fact| receipts.contains(fact.e())));
        }

        // The shared prefix is projected once: one receipt per block, and each
        // replayed receipt carries both of its occurrences.
        assert_eq!(receipts.len(), 5);
        assert_eq!(ids_with_tag(&union, schema::block::KIND).len(), 5);
        let observed_twice = receipts
            .iter()
            .filter(|receipt| {
                let receipt = **receipt;
                find!(
                    timestamp: Inline<NsTAIInterval>,
                    pattern!(union.facts(), [{
                        receipt @ schema::source_projection::source_timestamp: ?timestamp
                    }])
                )
                .count()
                    == 2
            })
            .count();
        assert_eq!(observed_twice, 3);
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
}
