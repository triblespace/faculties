//! GORBIE-embeddable activity timeline.
//!
//! A pan/zoom time axis that overlays events from one or more pile
//! datasets on a single vertical timeline (newest at top, oldest at
//! bottom). The widget holds UI + cached-event state only; each configured
//! [`TimelineSource`] names its dataset with a stable
//! [`SourceKey`](crate::widgets::storage::SourceKey), and rendering resolves
//! those keys through a shared
//! [`WidgetContext`](crate::widgets::storage::WidgetContext).
//!
//! Per-kind decoration:
//! * Compass — goal status changes (pill + goal title)
//! * Local messages — body preview with sender/recipient pills
//! * Wiki — fragment versions with title
//!
//! ```ignore
//! let mut timeline = BranchTimeline::multi(vec![
//!     TimelineSource::Compass {
//!         key: SourceKey::Compass,
//!         label: "goals".into(),
//!     },
//!     TimelineSource::LocalMessages {
//!         key: SourceKey::Messages,
//!         label: "local".into(),
//!     },
//!     TimelineSource::Wiki {
//!         key: SourceKey::Wiki,
//!         label: "wiki".into(),
//!     },
//! ]);
//! // Inside a GORBIE card:
//! timeline.render(ctx, &storage.context());
//! ```
//!
//! Input handling:
//! * scroll = pan (vertical)
//! * pinch or cmd/ctrl + scroll = zoom (horizontal trackpad drift no longer zooms)
//! * drag = pan
//! * double-click = jump to "now"
//!
//! The ruler is a "four-sine" design: overlapping cosines at the natural
//! time periods (minute, hour, day) that produce constructive interference
//! at nice times. Labels are placed independently at the coarsest interval
//! that gives ~6-10 labels per viewport.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use hifitime::{Duration as HifiDuration, Epoch};
use triblespace::core::id::Id;
use triblespace::core::inline::encodings::hash::Handle;
use triblespace::core::inline::{Inline, RawInline};
use triblespace::core::metadata;
use triblespace::core::repo::BlobStoreGet;
use triblespace::macros::{find, pattern};
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::View;
use GORBIE::card_ctx::GRID_ROW_MODULE;
use GORBIE::prelude::CardCtx;

use crate::schemas::blockdag as archive;
use crate::schemas::compass::{board as compass_attrs, KIND_GOAL_ID, KIND_NOTE_ID, KIND_STATUS_ID};
use crate::schemas::message::{local as message_attrs, KIND_MESSAGE_ID};
use crate::schemas::reason::{reason_schema as reason_attrs, KIND_REASON_ID};
use crate::schemas::wiki::{attrs as wiki_attrs, KIND_AUTHORSHIP};
use crate::storage::FactArchive;
use crate::widgets::storage::{DatasetRevision, DatasetView, SourceKey, WidgetContext};

/// Handle to a long-string blob (titles, bodies, notes).
type TextHandle = Inline<Handle<UTF8String>>;

// ── Rendering constants ──────────────────────────────────────────────

/// Default viewport height in pixels.
const DEFAULT_VIEWPORT_HEIGHT: f32 = 800.0;
/// Default zoom: pixels per minute of wall time.
const TIMELINE_DEFAULT_SCALE: f32 = 2.0;

/// Tick intervals (in nanoseconds) used for label placement. Picks the
/// smallest interval >= `label_min_ns` so labels never overlap.
const TICK_INTERVALS: &[i128] = {
    const NS: i128 = 1_000_000_000;
    &[
        NS,             // 1 second
        5 * NS,         // 5 seconds
        10 * NS,        // 10 seconds
        30 * NS,        // 30 seconds
        60 * NS,        // 1 minute
        5 * 60 * NS,    // 5 minutes
        10 * 60 * NS,   // 10 minutes
        30 * 60 * NS,   // 30 minutes
        3600 * NS,      // 1 hour
        3 * 3600 * NS,  // 3 hours
        6 * 3600 * NS,  // 6 hours
        12 * 3600 * NS, // 12 hours
        86400 * NS,     // 1 day
        7 * 86400 * NS, // 1 week
    ]
};

/// Format a TAI nanosecond key as a human-readable time marker.
fn format_time_marker(key: i128) -> String {
    let ns = HifiDuration::from_total_nanoseconds(key);
    let epoch = Epoch::from_tai_duration(ns);
    let (y, m, d, h, min, s, _) = epoch.to_gregorian_utc();
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}")
}

/// Current TAI time as a ns key, when the system clock is available.
fn now_key() -> Option<i128> {
    crate::clock::tai_nanoseconds_now().ok()
}

/// First 8 hex chars of an Id — compact label for pills / hover.
fn id_hex(id: Id) -> String {
    format!("{id:x}")
}

/// Truncate `s` to fit in `max_px` at `char_px` per char, appending
/// "…" if truncated. Char-aware so multibyte sequences don't panic
/// on slice.
fn truncate_to_chip_width(s: &str, max_px: f32, char_px: f32) -> String {
    let max_chars = (max_px / char_px).max(3.0) as usize;
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let take: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{take}…")
}

/// Trim a string to `max` chars on a single line, replacing inner
/// newlines with spaces. Used for body/title previews on event rows.
fn preview(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        let take: String = trimmed.chars().take(max.saturating_sub(1)).collect();
        format!("{take}…")
    }
}

/// Pick black or white text color for good contrast on `fill`.
fn text_on(fill: egui::Color32) -> egui::Color32 {
    let r = fill.r() as f32 / 255.0;
    let g = fill.g() as f32 / 255.0;
    let b = fill.b() as f32 / 255.0;
    let lin = |c: f32| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let l = 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
    if l > 0.4 {
        egui::Color32::BLACK
    } else {
        egui::Color32::WHITE
    }
}

// ── Source descriptions & styling ────────────────────────────────────

/// Decoration description for a timeline source. Each variant carries the
/// stable key used to find its immutable dataset at render time.
#[derive(Clone, Debug)]
pub enum TimelineSource {
    /// Compass — render goal status changes with the goal title and
    /// status-color pill.
    Compass { key: SourceKey, label: String },
    /// Local-messages — render each message with sender/body preview.
    LocalMessages { key: SourceKey, label: String },
    /// Wiki — render fragment versions with title.
    Wiki { key: SourceKey, label: String },
    /// Reason events — explicit reasoning notes the agent records
    /// alongside the command it ran. Useful as
    /// "thought before action" markers along the activity axis.
    Reason { key: SourceKey, label: String },
    /// Archive — externally imported conversation messages (archive
    /// dataset), so e.g. ChatGPT / Codex / Copilot history appears inline
    /// with the rest of the timeline.
    Archive { key: SourceKey, label: String },
}

/// Coarse kind used on the widget's `selected_event` and as a color key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Compass,
    LocalMessages,
    Wiki,
    Reason,
    Archive,
}

impl TimelineSource {
    fn key(&self) -> SourceKey {
        match self {
            TimelineSource::Compass { key, .. }
            | TimelineSource::LocalMessages { key, .. }
            | TimelineSource::Wiki { key, .. }
            | TimelineSource::Reason { key, .. }
            | TimelineSource::Archive { key, .. } => *key,
        }
    }

    /// A short (≤6 char) source label used in the pill.
    fn label(&self) -> String {
        match self {
            TimelineSource::Compass { label, .. }
            | TimelineSource::LocalMessages { label, .. }
            | TimelineSource::Wiki { label, .. }
            | TimelineSource::Reason { label, .. }
            | TimelineSource::Archive { label, .. } => label.clone(),
        }
    }

    fn color(&self) -> egui::Color32 {
        match self {
            // RAL 1012 lemon yellow — matches playground color_goals.
            TimelineSource::Compass { .. } => egui::Color32::from_rgb(0xd9, 0xc2, 0x2e),
            // RAL 6032 signal green — matches playground color_local_msg.
            TimelineSource::LocalMessages { .. } => egui::Color32::from_rgb(0x23, 0x7f, 0x52),
            // RAL 3012 beige red — matches playground color_wiki.
            TimelineSource::Wiki { .. } => egui::Color32::from_rgb(0xc1, 0x87, 0x6b),
            // RAL 1003 signal yellow — reason events read as "agent thought".
            TimelineSource::Reason { .. } => egui::Color32::from_rgb(0xf7, 0xba, 0x0b),
            // RAL 5012 light blue — archived conversation messages.
            TimelineSource::Archive { .. } => egui::Color32::from_rgb(0x3b, 0x83, 0xbd),
        }
    }
}

/// Kanban status color — reused for the inline pill on Compass events.
fn status_color(status: &str) -> egui::Color32 {
    match status {
        // RAL 6018 yellow green
        "todo" => egui::Color32::from_rgb(0x57, 0xa6, 0x39),
        // RAL 1003 signal yellow
        "doing" => egui::Color32::from_rgb(0xf7, 0xba, 0x0b),
        // RAL 3020 traffic red
        "blocked" => egui::Color32::from_rgb(0xcc, 0x0a, 0x17),
        // RAL 5005 signal blue
        "done" => egui::Color32::from_rgb(0x15, 0x4e, 0xa1),
        // RAL 7012 basalt grey (muted)
        _ => egui::Color32::from_rgb(0x4d, 0x55, 0x59),
    }
}

// ── Marks: metadata selected by time window ─────────────────────────

/// What a chip would show, as the handles it would read. Selecting the
/// marks of a time window reads facts only; a label's text is read when the
/// label is drawn, never for a strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Label {
    /// A goal's creation: its title.
    Goal { title: TextHandle },
    /// A status change: the goal's title and the status text.
    Status { goal: Id, status: TextHandle },
    /// A note: the goal's title and the note body.
    Note { goal: Id, note: TextHandle },
    /// A local message: its body; sender and recipient are ids.
    Message { from: Id, to: Id, body: TextHandle },
    /// A wiki revision: its title.
    Wiki { title: TextHandle },
    /// A reasoning note: its text.
    Reason { text: TextHandle },
    /// A source projection: the texts of the node it projects to.
    Archive { block: Id },
}

/// A single point on the timeline, without its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mark {
    source_idx: usize,
    kind: SourceKind,
    entity_id: Id,
    ts_ns: i128,
    label: Label,
}

/// The text a drawn label shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Shown {
    /// Primary one-line preview (goal title, message body, or wiki title).
    summary: String,
    /// Optional kanban-status pill label (Compass).
    status: Option<String>,
    /// Optional sender line (LocalMessages — "<from> → <to>"), tooltip only.
    from_to: Option<String>,
}

// ── Live connection ──────────────────────────────────────────────────

/// Resolve configured sources by their stable keys while retaining their
/// display position. In particular, an absent source in the middle is simply
/// omitted; it can never cause the next available dataset to inherit the
/// missing source's semantic kind.
fn resolve_sources<'a, T>(
    sources: &'a [TimelineSource],
    mut resolve: impl FnMut(SourceKey) -> Option<T>,
) -> Vec<(usize, &'a TimelineSource, T)> {
    sources
        .iter()
        .enumerate()
        .filter_map(|(idx, source)| resolve(source.key()).map(|dataset| (idx, source, dataset)))
        .collect()
}

fn source_revisions(
    sources: &[TimelineSource],
    datasets: &WidgetContext<'_>,
) -> BTreeMap<SourceKey, DatasetRevision> {
    resolve_sources(sources, |key| datasets.dataset(key))
        .into_iter()
        .map(|(_, source, dataset)| (source.key(), dataset.revision))
        .collect()
}

fn read_text(dataset: DatasetView<'_>, h: TextHandle) -> String {
    dataset
        .reader
        .get::<View<str>, UTF8String>(h)
        .map(|v| {
            let s: &str = v.as_ref();
            s.to_string()
        })
        .unwrap_or_default()
}

fn interval_start(interval: Inline<NsTAIInterval>) -> Option<i128> {
    let (lower, _): (i128, i128) = interval.try_from_inline().ok()?;
    Some(lower)
}

/// `NsTAIInterval` stores each bound as an i128 with its sign bit flipped,
/// big-endian, lower bound first. Raw byte order is therefore start order,
/// which is what lets a window seek the attribute's ordered facts.
const INTERVAL_SIGN_BIT: u128 = 1 << 127;

fn raw_interval_start(raw: &RawInline) -> i128 {
    let mut lower = [0u8; 16];
    lower.copy_from_slice(&raw[..16]);
    (u128::from_be_bytes(lower) ^ INTERVAL_SIGN_BIT) as i128
}

/// The least raw interval starting at `start`.
fn raw_interval_from(start: i128) -> RawInline {
    let mut raw = [0u8; 32];
    raw[..16].copy_from_slice(&((start as u128) ^ INTERVAL_SIGN_BIT).to_be_bytes());
    raw
}

/// The facts of one interval attribute whose start lies in `[lo, hi]`, in
/// start order. Each shard seeks to `lo` in O(log n) and the walk stops past
/// `hi`, so facts outside the window are neither decoded nor merged. Values
/// whose bounds are inverted are returned too; each caller passes over them
/// as its full-history read did.
fn starts_in(
    facts: &FactArchive,
    attribute: Id,
    lo: i128,
    hi: i128,
) -> Vec<(Inline<NsTAIInterval>, Id)> {
    let from = raw_interval_from(lo);
    facts
        .iter_attribute_value_entities_from(&attribute, &from)
        .take_while(|(raw, _)| raw_interval_start(raw) <= hi)
        .map(|(raw, entity)| (Inline::new(raw), entity))
        .collect()
}

/// Every mark of one source whose time lies in `[lo, hi]`, appended to
/// `out`. Reads facts only. Returns how many time facts the window held:
/// the metadata work this selection did.
fn marks_in(
    idx: usize,
    source: &TimelineSource,
    facts: &FactArchive,
    lo: i128,
    hi: i128,
    out: &mut Vec<Mark>,
) -> usize {
    match source {
        TimelineSource::Compass { .. } => compass_marks(idx, facts, lo, hi, out),
        TimelineSource::LocalMessages { .. } => local_marks(idx, facts, lo, hi, out),
        TimelineSource::Wiki { .. } => wiki_marks(idx, facts, lo, hi, out),
        TimelineSource::Reason { .. } => reason_marks(idx, facts, lo, hi, out),
        TimelineSource::Archive { .. } => archive_marks(idx, facts, lo, hi, out),
    }
}

/// A Compass mark per goal creation, status change and note at its
/// `created_at`, so quiet boards still show up.
fn compass_marks(
    idx: usize,
    facts: &FactArchive,
    lo: i128,
    hi: i128,
    out: &mut Vec<Mark>,
) -> usize {
    let times = starts_in(facts, metadata::created_at.id(), lo, hi);
    for &(at, entity) in &times {
        let Some(ts_ns) = interval_start(at) else {
            continue;
        };
        let mark = |label| Mark {
            source_idx: idx,
            kind: SourceKind::Compass,
            entity_id: entity,
            ts_ns,
            label,
        };
        for title in find!(
            title: TextHandle,
            pattern!(facts, [{
                entity @
                metadata::tag: &KIND_GOAL_ID,
                compass_attrs::title: ?title,
            }])
        ) {
            out.push(mark(Label::Goal { title }));
        }
        for (goal, status) in find!(
            (goal: Id, status: TextHandle),
            pattern!(facts, [{
                entity @
                metadata::tag: &KIND_STATUS_ID,
                compass_attrs::status_of: ?goal,
                compass_attrs::status: ?status,
            }])
        ) {
            out.push(mark(Label::Status { goal, status }));
        }
        for (goal, note) in find!(
            (goal: Id, note: TextHandle),
            pattern!(facts, [{
                entity @
                metadata::tag: &KIND_NOTE_ID,
                compass_attrs::task: ?goal,
                compass_attrs::note: ?note,
            }])
        ) {
            out.push(mark(Label::Note { goal, note }));
        }
    }
    times.len()
}

/// One LocalMessages mark per envelope. The view answers with a bag, so a
/// repeated field is another witness of the same message; the mark sits at
/// the newest `created_at`, as the message listing and the triage count do,
/// and only a window holding that newest time shows it.
fn local_marks(idx: usize, facts: &FactArchive, lo: i128, hi: i128, out: &mut Vec<Mark>) -> usize {
    let times = starts_in(facts, metadata::created_at.id(), lo, hi);
    let mut placed = HashSet::new();
    for &(at, entity) in &times {
        let Some(ts_ns) = interval_start(at) else {
            continue;
        };
        if placed.contains(&entity) {
            continue;
        }
        let Some((from, to, body)) = find!(
            (from: Id, to: Id, body: TextHandle),
            pattern!(facts, [{
                entity @
                metadata::tag: &KIND_MESSAGE_ID,
                message_attrs::from: ?from,
                message_attrs::to: ?to,
                message_attrs::body: ?body,
            }])
        )
        .min() else {
            continue;
        };
        let newest = find!(
            created_at: Inline<NsTAIInterval>,
            pattern!(facts, [{ entity @ metadata::created_at: ?created_at }])
        )
        .map(interval_start)
        .max()
        .flatten();
        if newest != Some(ts_ns) {
            continue;
        }
        placed.insert(entity);
        out.push(Mark {
            source_idx: idx,
            kind: SourceKind::LocalMessages,
            entity_id: entity,
            ts_ns,
            label: Label::Message { from, to, body },
        });
    }
    times.len()
}

/// A Wiki mark per revision projection at its authored time: a native
/// revision's earliest authorship, a legacy version's greatest observation.
/// A `created_at` in the window names either the legacy version itself or
/// an authorship of a native revision; each candidate is then projected
/// exactly as the full listing projects it and kept if its time is here.
fn wiki_marks(idx: usize, facts: &FactArchive, lo: i128, hi: i128, out: &mut Vec<Mark>) -> usize {
    let times = starts_in(facts, metadata::created_at.id(), lo, hi);
    let mut candidates = BTreeSet::new();
    for &(_, entity) in &times {
        let mut authored = find!(
            revision: Id,
            pattern!(facts, [{
                entity @
                metadata::tag: &KIND_AUTHORSHIP,
                wiki_attrs::revision: ?revision,
            }])
        )
        .peekable();
        if authored.peek().is_none() {
            candidates.insert(entity);
        } else {
            candidates.extend(authored);
        }
    }
    for revision_id in candidates {
        for revision in crate::wiki::revision_records(facts, revision_id) {
            let Some(ts_ns) = revision.authored_at().and_then(interval_start) else {
                continue;
            };
            if ts_ns < lo || ts_ns > hi {
                continue;
            }
            out.push(Mark {
                source_idx: idx,
                kind: SourceKind::Wiki,
                entity_id: revision.id,
                ts_ns,
                label: Label::Wiki {
                    title: revision.title,
                },
            });
        }
    }
    times.len()
}

/// A Reason mark per reasoning entity. Each entity has a long-string
/// `text` payload (the agent's thought) and a created-at timestamp; the
/// label shows the first line of the thought as its summary.
fn reason_marks(idx: usize, facts: &FactArchive, lo: i128, hi: i128, out: &mut Vec<Mark>) -> usize {
    let times = starts_in(facts, metadata::created_at.id(), lo, hi);
    for &(at, entity) in &times {
        let Some(ts_ns) = interval_start(at) else {
            continue;
        };
        for text in find!(
            text: TextHandle,
            pattern!(facts, [{
                entity @
                metadata::tag: &KIND_REASON_ID,
                reason_attrs::text: ?text,
            }])
        ) {
            out.push(Mark {
                source_idx: idx,
                kind: SourceKind::Reason,
                entity_id: entity,
                ts_ns,
                label: Label::Reason { text },
            });
        }
    }
    times.len()
}

/// One Archive mark per source projection receipt and node it projects to.
/// A receipt's earliest genuine source timestamp wins when present;
/// otherwise the canonical node time is used. So a window looks in two
/// places: receipts whose earliest source time is here, and nodes whose
/// time is here for the receipts without one.
fn archive_marks(
    idx: usize,
    facts: &FactArchive,
    lo: i128,
    hi: i128,
    out: &mut Vec<Mark>,
) -> usize {
    let mark = |projection, ts_ns, block| Mark {
        source_idx: idx,
        kind: SourceKind::Archive,
        entity_id: projection,
        ts_ns,
        label: Label::Archive { block },
    };

    let sourced = starts_in(
        facts,
        archive::source_projection::source_timestamp.id(),
        lo,
        hi,
    );
    let mut receipts = HashSet::new();
    for &(_, projection) in &sourced {
        if !receipts.insert(projection) {
            continue;
        }
        let Some(ts_ns) = earliest_source_time(facts, projection).and_then(interval_start) else {
            continue;
        };
        if ts_ns < lo || ts_ns > hi {
            continue;
        }
        for block in find!(
            block: Id,
            pattern!(facts, [{
                projection @
                metadata::tag: &archive::source_projection::KIND,
                archive::source_projection::projects_to: ?block,
            }])
        ) {
            out.push(mark(projection, ts_ns, block));
        }
    }

    let timed = starts_in(facts, archive::block::timestamp.id(), lo, hi);
    let mut nodes = HashSet::new();
    for &(_, block) in &timed {
        if !nodes.insert(block) {
            continue;
        }
        let Some(ts_ns) = node_time(facts, block) else {
            continue;
        };
        if ts_ns < lo || ts_ns > hi {
            continue;
        }
        for projection in find!(
            projection: Id,
            pattern!(facts, [{
                ?projection @
                metadata::tag: &archive::source_projection::KIND,
                archive::source_projection::projects_to: &block,
            }])
        ) {
            if earliest_source_time(facts, projection).is_none() {
                out.push(mark(projection, ts_ns, block));
            }
        }
    }
    sourced.len() + timed.len()
}

/// A receipt's earliest source timestamp, when it has one.
///
/// A receipt carries one timestamp per observed occurrence: a Codex fork or
/// resume that replays its parent adds the replay's time to the parent's
/// receipt. The earliest is when the message was first said, and it is the
/// one `archive list` prints. A timestamp that is not a valid interval is
/// passed over, as that typed read passes it over.
fn earliest_source_time(facts: &FactArchive, projection: Id) -> Option<Inline<NsTAIInterval>> {
    find!(
        timestamp: Inline<NsTAIInterval>,
        pattern!(facts, [{
            projection @ archive::source_projection::source_timestamp: ?timestamp
        }])
    )
    .filter_map(|timestamp| interval_start(timestamp).map(|start| (start, timestamp)))
    .min_by_key(|(start, _)| *start)
    .map(|(_, timestamp)| timestamp)
}

/// A node's canonical time: its earliest valid timestamp.
fn node_time(facts: &FactArchive, block: Id) -> Option<i128> {
    find!(
        timestamp: Inline<NsTAIInterval>,
        pattern!(facts, [{ block @ archive::block::timestamp: ?timestamp }])
    )
    .filter_map(interval_start)
    .min()
}

/// Read the text one drawn label shows. `stopped` is the source's set of
/// nodes a recorded classifier stop followed, found once per revision on
/// the first archive label drawn.
fn hydrate(
    label: Label,
    facts: &FactArchive,
    stopped: &mut Option<BTreeSet<Id>>,
    read: &mut dyn FnMut(TextHandle) -> String,
) -> Shown {
    match label {
        Label::Goal { title } => Shown {
            summary: preview(&read(title), 80),
            status: Some("created".to_string()),
            from_to: None,
        },
        Label::Status { goal, status } => Shown {
            summary: preview(&goal_title(facts, goal, read), 80),
            status: Some(read(status)),
            from_to: None,
        },
        Label::Note { goal, note } => {
            let body = read(note);
            let title = goal_title(facts, goal, read);
            let summary = if body.is_empty() {
                preview(&title, 80)
            } else {
                preview(&format!("{title} — {body}"), 80)
            };
            Shown {
                summary,
                status: Some("note".to_string()),
                from_to: None,
            }
        }
        Label::Message { from, to, body } => Shown {
            summary: preview(&read(body), 80),
            status: None,
            from_to: Some(format!("{} → {}", id_hex(from), id_hex(to))),
        },
        Label::Wiki { title } | Label::Reason { text: title } => Shown {
            summary: preview(&read(title), 80),
            status: None,
            from_to: None,
        },
        Label::Archive { block } => Shown {
            summary: preview(&archive_content(facts, block, stopped, read), 80),
            status: None,
            from_to: None,
        },
    }
}

/// The title a status or note shows for its goal: a created goal's title.
fn goal_title(facts: &FactArchive, goal: Id, read: &mut dyn FnMut(TextHandle) -> String) -> String {
    find!(
        title: TextHandle,
        pattern!(facts, [{
            goal @
            metadata::tag: &KIND_GOAL_ID,
            compass_attrs::title: ?title,
            metadata::created_at: _?created_at,
        }])
    )
    .last()
    .map(read)
    .unwrap_or_else(|| "(untitled)".to_string())
}

/// A node's texts in fact order. A chip shows a node's text, and a capture
/// carries it to a model: what a recorded classifier stop followed shows
/// only that it is withheld, and is not read. A node holding no text
/// remains visible as a typed placeholder.
fn archive_content(
    facts: &FactArchive,
    block: Id,
    stopped: &mut Option<BTreeSet<Id>>,
    read: &mut dyn FnMut(TextHandle) -> String,
) -> String {
    let stopped = stopped.get_or_insert_with(|| crate::archive::classifier_stopped(facts));
    if stopped.contains(&block) {
        return "[withheld: a classifier stop followed this]".to_owned();
    }
    // A node holds one fact, or a set at one moment: its texts in fact order.
    let mut parts: Vec<(Id, TextHandle)> = find!(
        (fact: Id, payload: TextHandle),
        pattern!(facts, [
            { block @ archive::block::holds: ?fact },
            { ?fact @ archive::content_fact::payload: ?payload },
        ])
    )
    .collect();
    parts.sort_unstable();
    let text = parts
        .into_iter()
        .map(|(_, payload)| read(payload))
        .collect::<Vec<_>>()
        .join(" ");
    if !text.trim().is_empty() {
        return text;
    }
    let count = find!(
        fact: Id,
        pattern!(facts, [{ block @ archive::block::holds: ?fact }])
    )
    .count();
    format!(
        "[{count} non-text part{}]",
        if count == 1 { "" } else { "s" }
    )
}

// ── Window selection and layout ──────────────────────────────────────

/// Height of a label chip; also the spacing a mark needs to be labelled.
const CHIP_H: f32 = 16.0;
/// Labels read on demand are kept up to this many, then forgotten.
const SHOWN_CAPACITY: usize = 4096;

/// A pixel row of strips from one source.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Strip {
    /// Offset from the viewport top, in pixels.
    y: f32,
    /// How many marks of this source fall in this pixel row.
    count: usize,
    /// The row's newest mark, selected by a click.
    mark: Mark,
}

/// What one viewport draws.
#[derive(Debug, Default)]
struct Frame {
    /// Marks with room for a chip, with their text: `(offset, mark, text)`.
    labels: Vec<(f32, Mark, Shown)>,
    /// Marks too dense for a chip, drawn from metadata alone.
    strips: Vec<Strip>,
}

/// Marks for a time range around the viewport, and the labels already read.
///
/// Selection seeks each source's ordered time facts, so it costs the window,
/// not the history. The selected range reaches one viewport beyond each edge,
/// so panning within it and zooming in re-select nothing; zooming out, or a
/// range grown far wider than the view, re-selects. A keyed dataset changing
/// discards everything.
struct Activity {
    revisions: BTreeMap<SourceKey, DatasetRevision>,
    /// The selected time range `[lo, hi]`; `marks` holds every mark in it,
    /// ordered by time.
    range: Option<(i128, i128)>,
    marks: Vec<Mark>,
    /// Time facts the last selection examined: its metadata work.
    scanned: usize,
    shown: HashMap<(usize, Label), Shown>,
    stopped: HashMap<usize, Option<BTreeSet<Id>>>,
    /// Instrumentation: selections run and texts read by this widget.
    selections: usize,
    reads: usize,
}

impl Activity {
    fn new(revisions: BTreeMap<SourceKey, DatasetRevision>) -> Self {
        Self {
            revisions,
            range: None,
            marks: Vec::new(),
            scanned: 0,
            shown: HashMap::new(),
            stopped: HashMap::new(),
            selections: 0,
            reads: 0,
        }
    }

    /// Forget every selection and label when a keyed dataset appeared,
    /// disappeared, or changed revision.
    fn observe(&mut self, revisions: BTreeMap<SourceKey, DatasetRevision>) {
        if self.revisions != revisions {
            let (selections, reads) = (self.selections, self.reads);
            *self = Self::new(revisions);
            self.selections = selections;
            self.reads = reads;
        }
    }

    /// The marks in `[view_end, view_start]`, selecting a range around it
    /// when the current one does not cover it.
    fn visible(
        &mut self,
        view_end: i128,
        view_start: i128,
        select: &mut dyn FnMut(i128, i128, &mut Vec<Mark>) -> usize,
    ) -> std::ops::Range<usize> {
        let span = view_start.saturating_sub(view_end).max(1);
        let covered = self.range.is_some_and(|(lo, hi)| {
            lo <= view_end && view_start <= hi && hi.saturating_sub(lo) / span <= 16
        });
        if !covered {
            let lo = view_end.saturating_sub(span);
            let hi = view_start.saturating_add(span);
            self.marks.clear();
            self.scanned = select(lo, hi, &mut self.marks);
            self.marks.sort_by_key(|mark| mark.ts_ns);
            self.range = Some((lo, hi));
            self.selections += 1;
        }
        let first = self.marks.partition_point(|mark| mark.ts_ns < view_end);
        let last = self.marks.partition_point(|mark| mark.ts_ns <= view_start);
        first..last
    }

    /// Lay out one viewport. A mark is labelled when its neighbours are at
    /// least a chip apart; only labels read text, so a frame reads at most
    /// one label's text per chip height of viewport. Denser marks become
    /// strips, aggregated per pixel row and source.
    fn frame<'f>(
        &mut self,
        view_start: i128,
        ns_per_px: f64,
        height: f32,
        select: &mut dyn FnMut(i128, i128, &mut Vec<Mark>) -> usize,
        facts: &dyn Fn(usize) -> Option<&'f FactArchive>,
        read: &mut dyn FnMut(usize, TextHandle) -> String,
    ) -> Frame {
        let view_end = view_start - (height as f64 * ns_per_px) as i128;
        let visible = self.visible(view_end, view_start, select);
        let offset = |mark: &Mark| ((view_start - mark.ts_ns) as f64 / ns_per_px) as f32;

        let mut frame = Frame::default();
        let mut row: Vec<Strip> = Vec::new();
        let flush = |row: &mut Vec<Strip>, strips: &mut Vec<Strip>| strips.append(row);
        for i in visible.clone() {
            let mark = self.marks[i];
            let y = offset(&mark);
            let roomy = |j: usize| (offset(&self.marks[j]) - y).abs() >= CHIP_H;
            let above = i + 1 == visible.end || roomy(i + 1);
            let below = i == visible.start || roomy(i - 1);
            if above && below {
                let shown = self.shown(&mark, facts, read);
                frame.labels.push((y, mark, shown));
                continue;
            }
            let pixel = y.floor();
            if row.first().is_some_and(|strip| strip.y != pixel) {
                flush(&mut row, &mut frame.strips);
            }
            match row
                .iter_mut()
                .find(|strip| strip.mark.source_idx == mark.source_idx)
            {
                // Marks ascend in time, so the last one seen is the newest.
                Some(strip) => {
                    strip.count += 1;
                    strip.mark = mark;
                }
                None => row.push(Strip {
                    y: pixel,
                    count: 1,
                    mark,
                }),
            }
        }
        flush(&mut row, &mut frame.strips);
        frame
    }

    /// The text of one drawn label, read once per revision.
    fn shown<'f>(
        &mut self,
        mark: &Mark,
        facts: &dyn Fn(usize) -> Option<&'f FactArchive>,
        read: &mut dyn FnMut(usize, TextHandle) -> String,
    ) -> Shown {
        let key = (mark.source_idx, mark.label);
        if let Some(shown) = self.shown.get(&key) {
            return shown.clone();
        }
        let Some(facts) = facts(mark.source_idx) else {
            return Shown::default();
        };
        if self.shown.len() >= SHOWN_CAPACITY {
            self.shown.clear();
        }
        let reads = &mut self.reads;
        let stopped = self.stopped.entry(mark.source_idx).or_default();
        let source_idx = mark.source_idx;
        let mut counted = |handle| {
            *reads += 1;
            read(source_idx, handle)
        };
        let shown = hydrate(mark.label, facts, stopped, &mut counted);
        self.shown.insert(key, shown.clone());
        shown
    }
}

// ── Widget ───────────────────────────────────────────────────────────

/// GORBIE-embeddable pan/zoom timeline for one or more semantic datasets.
///
/// Paints a full-width vertical time axis (newest at top, oldest at
/// bottom) with:
///
/// * a four-sine ruler (constructive interference at minute / hour /
///   day boundaries)
/// * time labels at the coarsest interval that fits
/// * per-source event chips with source-specific decoration where there
///   is room, and thin source-colored strips where events are denser than
///   a chip; a strip is drawn from metadata and reads no text
///
/// Only the time range around the viewport is selected, through each
/// source's ordered time facts, and only drawn chips read their text.
///
/// Sources are resolved by [`SourceKey`] from the immutable context supplied by
/// the host. Missing datasets are omitted independently.
pub struct BranchTimeline {
    /// Semantic sources in display order. Dataset lookup is always keyed and
    /// never inferred from this vector's positional relationship to input.
    sources: Vec<TimelineSource>,
    viewport_height: f32,
    /// Selected marks and read labels; discarded when any source dataset
    /// changes.
    activity: Option<Activity>,
    /// Top edge of viewport, in TAI ns. Newest visible time.
    timeline_start: i128,
    /// Pixels per minute of wall time.
    timeline_scale: f32,
    /// Tracks the first render so we can initialize `timeline_start` to
    /// current time, or to the newest event when the clock is unavailable.
    first_render: bool,
    /// The most-recently-clicked event, if any. Hosts can read this to
    /// drive floating detail cards.
    pub selected_event: Option<(SourceKind, Id)>,
}

impl BranchTimeline {
    /// Multi-source overlay — each source paints its own events on the
    /// shared axis.
    pub fn multi(sources: Vec<TimelineSource>) -> Self {
        Self {
            sources,
            viewport_height: DEFAULT_VIEWPORT_HEIGHT,
            activity: None,
            timeline_start: 0,
            timeline_scale: TIMELINE_DEFAULT_SCALE,
            first_render: true,
            selected_event: None,
        }
    }

    /// Override the viewport height (pixels). Defaults to 800.
    pub fn with_height(mut self, height: f32) -> Self {
        self.viewport_height = height.max(48.0);
        self
    }

    /// Render the timeline from immutable datasets resolved by stable key.
    pub fn render(&mut self, ctx: &mut CardCtx<'_>, datasets: &WidgetContext<'_>) {
        ctx.section("Activity", |ctx| {
            let now = now_key();

            let revisions = source_revisions(&self.sources, datasets);
            self.activity
                .get_or_insert_with(|| Activity::new(revisions.clone()))
                .observe(revisions);

            if self.first_render {
                // Without a clock the view opens at the newest event, which
                // takes one metadata pass over every source's history.
                let anchor = now.or_else(|| {
                    let mut marks = Vec::new();
                    for (idx, source, view) in
                        resolve_sources(&self.sources, |key| datasets.dataset(key))
                    {
                        marks_in(idx, source, view.facts, i128::MIN, i128::MAX, &mut marks);
                    }
                    marks.iter().map(|mark| mark.ts_ns).max()
                });
                if let Some(anchor) = anchor {
                    self.timeline_start = anchor;
                    self.first_render = false;
                }
            }

            // Visible time span in the viewport — used for the right-
            // aligned scale chip in the legend row so the viewer always
            // knows what range they're looking at without manually
            // reading the tick marks.
            if self.first_render {
                ctx.ui_mut().label(
                    egui::RichText::new("CURRENT TIME UNAVAILABLE · NO TIMELINE EVENTS")
                        .monospace()
                        .small()
                        .weak(),
                );
                return;
            }
            // Paint the viewport directly on the section ctx (no
            // grid wrapper) so it runs edge-to-edge inside the
            // section, matching the wiki graph's treatment. Legend
            // + SPAN + zoom-hint all live as overlays inside the
            // viewport itself.
            self.paint_viewport(ctx, now, datasets);
        });
    }

    /// Paint the timeline viewport. All pan/zoom/scroll logic lives here.
    fn paint_viewport(
        &mut self,
        ctx: &mut CardCtx<'_>,
        now: Option<i128>,
        datasets: &WidgetContext<'_>,
    ) {
        let viewport_height = self.viewport_height;
        let ui = ctx.ui_mut();
        let scroll_speed = 3.0;
        let viewport_width = ui.available_width();
        // egui's drag sense is z-aware (only the topmost widget claims
        // the drag), so floats dragged across the viewport don't pan
        // the timeline. The hit_test.rs panic that prompted manual
        // detection in earlier 0.34 versions has been fixed upstream.
        let (viewport_rect, viewport_response) = ui.allocate_exact_size(
            egui::vec2(viewport_width, viewport_height),
            egui::Sense::click_and_drag(),
        );

        // Input handling — compute ns_per_px from CURRENT scale.
        {
            let ns_per_px = 60_000_000_000.0 / self.timeline_scale as f64;

            // Check hover via direct rect-contains-pointer test rather
            // than `viewport_response.hovered()`, because the outer
            // notebook ScrollArea claims hover priority and makes
            // the widget-level hovered() unreliable for scroll capture.
            let pointer_in_viewport = ui
                .input(|i| i.pointer.hover_pos())
                .map(|p| viewport_rect.contains(p))
                .unwrap_or(false);
            if pointer_in_viewport {
                let (scroll_y, ctrl, pointer_pos, pinch) = ui.input(|i| {
                    (
                        i.smooth_scroll_delta.y,
                        i.modifiers.command || i.modifiers.ctrl,
                        i.pointer.hover_pos(),
                        i.zoom_delta(),
                    )
                });

                let cursor_rel_y = pointer_pos
                    .map(|p| (p.y - viewport_rect.top()).max(0.0))
                    .unwrap_or(viewport_height * 0.5);

                let cursor_time = self.timeline_start - (cursor_rel_y as f64 * ns_per_px) as i128;

                // Scroll without a modifier → pan the timeline.
                // Cmd/Ctrl + scroll OR native trackpad pinch → zoom
                // around the cursor row. Horizontal scroll no longer
                // zooms — trackpad sideways drift was triggering
                // unintended zoom on every swipe.
                let mut consumed_scroll = false;
                if scroll_y != 0.0 && !ctrl {
                    let pan_ns = (scroll_y as f64 * scroll_speed * ns_per_px) as i128;
                    self.timeline_start += pan_ns;
                    consumed_scroll = true;
                }

                let zoom_factor = if pinch != 1.0 {
                    pinch
                } else if ctrl && scroll_y != 0.0 {
                    if scroll_y > 0.0 {
                        1.15
                    } else {
                        1.0 / 1.15
                    }
                } else {
                    1.0
                };

                if zoom_factor != 1.0 {
                    let new_scale = (self.timeline_scale * zoom_factor).clamp(0.01, 1000.0);
                    let new_ns_per_px = 60_000_000_000.0 / new_scale as f64;
                    self.timeline_start =
                        cursor_time + (cursor_rel_y as f64 * new_ns_per_px) as i128;
                    self.timeline_scale = new_scale;
                    consumed_scroll = true;
                }

                // Only swallow the scroll delta when we actually used
                // it for pan/zoom — otherwise let the outer notebook
                // ScrollArea consume the gesture normally.
                if consumed_scroll {
                    ui.ctx().input_mut(|i| {
                        i.smooth_scroll_delta = egui::Vec2::ZERO;
                    });
                }
            }

            // Drag-to-pan via egui's z-aware drag sense. egui handles
            // the click/drag deadzone internally — short press+release
            // returns clicked() without ever flipping dragged() — so
            // we don't need a manual threshold.
            let drag_dy = viewport_response.drag_delta().y;
            if drag_dy != 0.0 {
                let pan_ns = (drag_dy as f64 * ns_per_px) as i128;
                self.timeline_start += pan_ns;
            }

            if viewport_response.double_clicked() {
                if let Some(now) = now {
                    self.timeline_start = now;
                }
            }
        }

        // Recompute bounds AFTER input with final scale.
        let ns_per_px = 60_000_000_000.0 / self.timeline_scale as f64;
        let viewport_ns = (viewport_height as f64 * ns_per_px) as i128;
        let view_start = self.timeline_start;
        let view_end = view_start - viewport_ns;

        let painter = ui.painter_at(viewport_rect);

        // Neutral dark-grey viewport background — makes the ruler
        // ticks and event chips pop against a consistent panel
        // regardless of the notebook theme fill.
        let frame_color = egui::Color32::from_rgb(0x29, 0x2c, 0x2f);
        painter.rect_filled(viewport_rect, 0.0, frame_color);

        // Four-sine ruler: one cosine per natural time period.
        let muted = egui::Color32::from_rgb(0x8a, 0x8a, 0x8a);
        let max_len = 80.0;
        let tick_spacing_px = GRID_ROW_MODULE;
        let tau = std::f64::consts::TAU;

        let ns = 1_000_000_000.0f64;
        let periods = [60.0 * ns, 3600.0 * ns, 86400.0 * ns];

        let significance = |t: f64| -> f32 {
            let mut sig = 0.0f32;
            let mut n = 0.0f32;
            for &period in &periods {
                let px_wave = period / ns_per_px;
                let vis = ((px_wave as f32 / tick_spacing_px - 1.0) / 3.0).clamp(0.0, 1.0);
                if vis < 0.001 {
                    continue;
                }
                sig += vis * (0.5 + 0.5 * (tau * t / period).cos() as f32);
                n += vis;
            }
            if n > 0.0 {
                sig / n
            } else {
                0.0
            }
        };

        let n_samples = (viewport_height / tick_spacing_px) as usize + 1;
        for i in 0..=n_samples {
            let y = viewport_rect.top() + i as f32 * tick_spacing_px;
            if y > viewport_rect.bottom() {
                break;
            }
            let t = view_start as f64 - (i as f64 * tick_spacing_px as f64 * ns_per_px);
            let sig = significance(t);
            let tick_len = 2.0 + (max_len - 2.0) * sig;

            painter.line_segment(
                [
                    egui::pos2(viewport_rect.left(), y),
                    egui::pos2(viewport_rect.left() + tick_len, y),
                ],
                egui::Stroke::new(0.5, muted),
            );
        }

        // Labels at coarsest interval giving >= label_min_spacing_px.
        let label_min_spacing_px = 100.0;
        let label_min_ns = (label_min_spacing_px as f64 * ns_per_px) as i128;
        let label_interval = TICK_INTERVALS
            .iter()
            .copied()
            .find(|&iv| iv >= label_min_ns)
            .unwrap_or(*TICK_INTERVALS.last().unwrap());

        if label_interval > 0 {
            let first = (view_start / label_interval) * label_interval;
            let mut tick = first;
            while tick > view_end {
                let y = viewport_rect.top() + ((view_start - tick) as f64 / ns_per_px) as f32;
                if y >= viewport_rect.top() && y <= viewport_rect.bottom() {
                    let label = format_time_marker(tick);
                    painter.text(
                        egui::pos2(viewport_rect.left() + max_len + 4.0, y),
                        egui::Align2::LEFT_CENTER,
                        &label,
                        egui::FontId::monospace(9.0),
                        muted,
                    );
                }
                tick -= label_interval;
            }
        }

        // NOW marker — a dashed horizontal guideline at current time
        // so the viewer can orient immediately. Only painted when
        // `now` falls inside the visible window.
        if let Some(now) = now.filter(|now| *now >= view_end && *now <= view_start) {
            // egui only repaints on input events, which froze the
            // marker until the mouse moved. Schedule a repaint for
            // when the marker will have travelled ~1px at the current
            // zoom, clamped to [1s, 60s] so a zoomed-out idle viewer
            // doesn't busy-spin and a zoomed-in one still glides.
            let secs_per_px = (ns_per_px / 1e9).clamp(1.0, 60.0);
            painter
                .ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(secs_per_px));

            let y = viewport_rect.top() + ((view_start - now) as f64 / ns_per_px) as f32;
            let now_color = egui::Color32::from_rgb(0xf7, 0xba, 0x0b); // RAL 1003
                                                                       // Dashed line: short segments every 10px.
            let mut x = viewport_rect.left();
            let x_end = viewport_rect.right();
            while x < x_end {
                let seg_end = (x + 6.0).min(x_end);
                painter.line_segment(
                    [egui::pos2(x, y), egui::pos2(seg_end, y)],
                    egui::Stroke::new(1.0, now_color),
                );
                x += 10.0;
            }
            painter.text(
                egui::pos2(viewport_rect.right() - 4.0, y - 6.0),
                egui::Align2::RIGHT_BOTTOM,
                "NOW",
                egui::FontId::monospace(9.0),
                now_color,
            );
        }

        // Top-right overlay: visible-window span + interaction hint.
        // Painted as plain text over the ruler — no pill background,
        // relies on the muted colors to recede against both the
        // viewport fill and the event chips.
        {
            let visible_secs = viewport_height as f64 * 60.0 / self.timeline_scale as f64;
            let span_label = format!("SPAN {}", format_span(visible_secs));
            let hint_label = "PINCH/\u{2318}+SCROLL \u{2192} ZOOM · DBL-CLICK \u{2192} NOW";
            let span_font = egui::FontId::monospace(10.0);
            let hint_font = egui::FontId::monospace(9.0);
            let span_color = egui::Color32::from_rgb(0xc8, 0xc8, 0xc8);
            let hint_color = egui::Color32::from_rgb(0x7a, 0x7a, 0x7a);
            let top = viewport_rect.top() + 6.0;
            let right = viewport_rect.right() - 8.0;
            let gap = 12.0;
            let hint_galley = painter.layout_no_wrap(hint_label.to_string(), hint_font, hint_color);
            let span_galley = painter.layout_no_wrap(span_label, span_font, span_color);
            let hint_pos = egui::pos2(right - hint_galley.size().x, top);
            painter.galley(hint_pos, hint_galley, hint_color);
            let span_pos = egui::pos2(hint_pos.x - gap - span_galley.size().x, top);
            painter.galley(span_pos, span_galley, span_color);
        }

        // Select the window's marks, then read only the labels with room.
        let views: Vec<Option<DatasetView<'_>>> = self
            .sources
            .iter()
            .map(|source| datasets.dataset(source.key()))
            .collect();
        let sources = &self.sources;
        let mut select = |lo, hi, out: &mut Vec<Mark>| {
            resolve_sources(sources, |key| datasets.dataset(key))
                .into_iter()
                .map(|(idx, source, view)| marks_in(idx, source, view.facts, lo, hi, out))
                .sum::<usize>()
        };
        let facts = |idx: usize| views.get(idx).copied().flatten().map(|view| view.facts);
        let mut read = |idx: usize, handle| match views.get(idx).copied().flatten() {
            Some(view) => read_text(view, handle),
            None => String::new(),
        };
        let frame = self
            .activity
            .as_mut()
            .map_or_else(Frame::default, |activity| {
                activity.frame(
                    view_start,
                    ns_per_px,
                    viewport_height,
                    &mut select,
                    &facts,
                    &mut read,
                )
            });

        // Per-source chip rows with source-specific decoration.
        let pointer_pos = ui.input(|i| i.pointer.hover_pos());
        let mut clicked_event: Option<(SourceKind, Id)> = None;
        let mut hover_rect: Option<(egui::Rect, egui::Color32)> = None;

        let event_left = viewport_rect.left() + max_len + 110.0;
        let event_right_margin = 8.0;
        let event_width = (viewport_rect.right() - event_left - event_right_margin).max(80.0);
        let chip_h = CHIP_H;
        let src_pill_w = 42.0;
        let text_color = egui::Color32::from_rgb(0xe6, 0xe6, 0xe6);

        // Strips: one source-colored line per pixel row and source, under
        // the source pill's column so the chips' column structure holds.
        let strip_h = 2.0;
        for strip in &frame.strips {
            let y = viewport_rect.top() + strip.y;
            let color = sources[strip.mark.source_idx].color();
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(event_left + 2.0, y - strip_h * 0.5),
                    egui::vec2(src_pill_w, strip_h),
                ),
                0.0,
                color,
            );
        }

        for (offset, ev, shown) in &frame.labels {
            let y = viewport_rect.top() + offset;
            let src = &sources[ev.source_idx];
            let src_color = src.color();
            let src_label = src.label();

            let chip_rect = egui::Rect::from_min_size(
                egui::pos2(event_left, y - chip_h * 0.5),
                egui::vec2(event_width, chip_h),
            );

            // Chip background.
            painter.rect_filled(chip_rect, 3.0, frame_color);

            // Source pill (left).
            let src_pill = egui::Rect::from_min_size(
                egui::pos2(event_left + 2.0, y - chip_h * 0.5 + 1.0),
                egui::vec2(src_pill_w, chip_h - 2.0),
            );
            painter.rect_filled(src_pill, 3.0, src_color);
            painter.text(
                src_pill.center(),
                egui::Align2::CENTER_CENTER,
                &src_label.to_uppercase(),
                egui::FontId::monospace(9.0),
                text_on(src_color),
            );

            // Optional secondary pill (status / from→to).
            let mut text_x = event_left + src_pill_w + 6.0;
            if let Some(status) = &shown.status {
                let pill_color = match ev.kind {
                    SourceKind::Compass => status_color(status),
                    _ => src_color,
                };
                let pill_w = 40.0 + (status.len() as f32 * 4.0).min(40.0);
                let pill = egui::Rect::from_min_size(
                    egui::pos2(text_x, y - chip_h * 0.5 + 1.0),
                    egui::vec2(pill_w, chip_h - 2.0),
                );
                painter.rect_filled(pill, 3.0, pill_color);
                painter.text(
                    pill.center(),
                    egui::Align2::CENTER_CENTER,
                    &status.to_uppercase(),
                    egui::FontId::monospace(9.0),
                    text_on(pill_color),
                );
                text_x = pill.right() + 6.0;
            }
            // `from_to` is intentionally not painted on the chip
            // strip — those are bare IDs and would dominate the
            // visible row. The full sender/recipient line is
            // surfaced in the hover tooltip instead.

            // Summary text — char-truncated to fit the available
            // chip width with a trailing "…". Cleaner than a hard
            // clip-rect cutoff because it always ends at a char
            // boundary with a visible overflow indicator.
            let available_px = (chip_rect.right() - text_x - 4.0).max(0.0);
            let truncated = truncate_to_chip_width(&shown.summary, available_px, 6.0);
            painter.text(
                egui::pos2(text_x, y),
                egui::Align2::LEFT_CENTER,
                &truncated,
                egui::FontId::monospace(10.0),
                text_color,
            );

            // Interaction: hover highlight + click + tooltip with
            // full summary + absolute timestamp (truncated chip text
            // often hides context, so surface it on hover).
            if let Some(p) = pointer_pos {
                if chip_rect.contains(p) {
                    hover_rect = Some((chip_rect, src_color));
                    if viewport_response.clicked() {
                        clicked_event = Some((ev.kind, ev.entity_id));
                    }
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    let time_str = format_time_marker(ev.ts_ns);
                    let summary = shown.summary.clone();
                    let src_label = src_label.clone();
                    let status_label = shown.status.clone();
                    let fromto_label = shown.from_to.clone();
                    let src_color_tip = src_color;
                    egui::Tooltip::always_open(
                        ui.ctx().clone(),
                        ui.layer_id(),
                        egui::Id::new(("timeline_event_tip", ev.entity_id)),
                        egui::PopupAnchor::Pointer,
                    )
                    .gap(12.0)
                    .show(|tip| {
                        tip.set_max_width(360.0);
                        // Header: colored source dot + source
                        // label + timestamp on a single line.
                        tip.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let (dot_rect, _) =
                                ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                            ui.painter()
                                .circle_filled(dot_rect.center(), 4.0, src_color_tip);
                            ui.label(
                                egui::RichText::new(src_label.to_uppercase())
                                    .small()
                                    .monospace()
                                    .strong()
                                    .color(src_color_tip),
                            );
                            ui.label(egui::RichText::new("·").small().weak());
                            ui.label(egui::RichText::new(time_str).small().monospace().weak());
                        });
                        // Optional status + from→to meta line.
                        if status_label.is_some() || fromto_label.is_some() {
                            tip.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                if let Some(st) = status_label {
                                    ui.label(
                                        egui::RichText::new(st.to_uppercase())
                                            .small()
                                            .monospace()
                                            .strong(),
                                    );
                                }
                                if let Some(ft) = fromto_label {
                                    ui.label(egui::RichText::new(ft).small().monospace().weak());
                                }
                            });
                        }
                        tip.separator();
                        tip.add(egui::Label::new(summary).wrap());
                        // Full canonical id at the bottom — the
                        // chip strip itself omits ids to keep the
                        // top-level view readable, so the hover
                        // surface is where they live.
                        tip.add(
                            egui::Label::new(
                                egui::RichText::new(id_hex(ev.entity_id))
                                    .monospace()
                                    .small()
                                    .weak(),
                            )
                            .wrap(),
                        );
                    });
                }
            }
        }

        // A strip under the pointer, when no chip is: metadata only. The
        // nearest pixel row within a few pixels answers, and a click
        // selects its newest mark.
        if hover_rect.is_none() {
            let near = pointer_pos
                .filter(|p| {
                    viewport_rect.contains(*p)
                        && p.x >= event_left
                        && p.x <= event_left + event_width
                })
                .and_then(|p| {
                    let at = p.y - viewport_rect.top();
                    frame
                        .strips
                        .iter()
                        .filter(|strip| (strip.y - at).abs() <= 3.0)
                        .min_by(|a, b| (a.y - at).abs().total_cmp(&(b.y - at).abs()))
                });
            if let Some(strip) = near {
                let row: Vec<&Strip> = frame
                    .strips
                    .iter()
                    .filter(|other| other.y == strip.y)
                    .collect();
                let src = &sources[strip.mark.source_idx];
                let src_color = src.color();
                let row_rect = egui::Rect::from_min_size(
                    egui::pos2(event_left + 2.0, viewport_rect.top() + strip.y - 3.0),
                    egui::vec2(src_pill_w, 6.0),
                );
                hover_rect = Some((row_rect, src_color));
                if viewport_response.clicked() {
                    clicked_event = Some((strip.mark.kind, strip.mark.entity_id));
                }
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                let time_str = format_time_marker(strip.mark.ts_ns);
                let lines: Vec<(String, egui::Color32, usize)> = row
                    .iter()
                    .map(|strip| {
                        let src = &sources[strip.mark.source_idx];
                        (src.label().to_uppercase(), src.color(), strip.count)
                    })
                    .collect();
                let newest = id_hex(strip.mark.entity_id);
                egui::Tooltip::always_open(
                    ui.ctx().clone(),
                    ui.layer_id(),
                    egui::Id::new(("timeline_strip_tip", strip.mark.entity_id)),
                    egui::PopupAnchor::Pointer,
                )
                .gap(12.0)
                .show(|tip| {
                    tip.set_max_width(360.0);
                    tip.label(egui::RichText::new(time_str).small().monospace().weak());
                    for (label, color, count) in lines {
                        tip.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let (dot_rect, _) =
                                ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                            ui.painter().circle_filled(dot_rect.center(), 4.0, color);
                            ui.label(
                                egui::RichText::new(format!("{label} × {count}"))
                                    .small()
                                    .monospace()
                                    .strong()
                                    .color(color),
                            );
                        });
                    }
                    tip.label(
                        egui::RichText::new("ZOOM IN TO READ")
                            .small()
                            .monospace()
                            .weak(),
                    );
                    tip.add(
                        egui::Label::new(egui::RichText::new(newest).monospace().small().weak())
                            .wrap(),
                    );
                });
            }
        }

        if let Some((rect, color)) = hover_rect {
            painter.rect_stroke(
                rect,
                3.0,
                egui::Stroke::new(1.0, color),
                egui::StrokeKind::Outside,
            );
        }

        if let Some(sel) = clicked_event {
            self.selected_event = Some(sel);
        }
    }
}

/// Format a visible-window duration as a short human-readable span
/// label ("2h", "30m", "3d") for the right-aligned scale chip. Only
/// two significant units — enough precision for a header chip.
fn format_span(secs: f64) -> String {
    let s = secs.max(1.0);
    if s >= 86_400.0 {
        let d = s / 86_400.0;
        if d >= 10.0 {
            format!("{d:.0}D")
        } else {
            format!("{d:.1}D")
        }
    } else if s >= 3_600.0 {
        let h = s / 3_600.0;
        if h >= 10.0 {
            format!("{h:.0}H")
        } else {
            format!("{h:.1}H")
        }
    } else if s >= 60.0 {
        format!("{:.0}M", s / 60.0)
    } else {
        format!("{s:.0}S")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use triblespace::core::blob::encodings::succinctarchive::SuccinctArchive;
    use triblespace::core::trible::TribleSet;
    use triblespace::prelude::{entity, Blob, ExclusiveId, IntoBlob, TryToInline};

    fn kind(source: &TimelineSource) -> SourceKind {
        match source {
            TimelineSource::Compass { .. } => SourceKind::Compass,
            TimelineSource::LocalMessages { .. } => SourceKind::LocalMessages,
            TimelineSource::Wiki { .. } => SourceKind::Wiki,
            TimelineSource::Reason { .. } => SourceKind::Reason,
            TimelineSource::Archive { .. } => SourceKind::Archive,
        }
    }

    fn configured_sources() -> Vec<TimelineSource> {
        vec![
            TimelineSource::Compass {
                key: SourceKey::Compass,
                label: "goals".into(),
            },
            TimelineSource::Wiki {
                key: SourceKey::Wiki,
                label: "wiki".into(),
            },
            TimelineSource::Archive {
                key: SourceKey::Archive,
                label: "chat".into(),
            },
        ]
    }

    #[test]
    fn missing_middle_source_does_not_shift_kinds() {
        let sources = configured_sources();
        let resolved = resolve_sources(&sources, |key| match key {
            SourceKey::Compass => Some("compass dataset"),
            SourceKey::Archive => Some("archive dataset"),
            _ => None,
        });
        let routes: Vec<_> = resolved
            .into_iter()
            .map(|(idx, source, dataset)| (idx, source.key(), kind(source), dataset))
            .collect();

        assert_eq!(
            routes,
            vec![
                (
                    0,
                    SourceKey::Compass,
                    SourceKind::Compass,
                    "compass dataset"
                ),
                (
                    2,
                    SourceKey::Archive,
                    SourceKind::Archive,
                    "archive dataset"
                ),
            ]
        );
    }

    #[test]
    fn input_map_order_does_not_change_source_resolution() {
        let sources = configured_sources();
        let forward = [
            (SourceKey::Compass, "compass dataset"),
            (SourceKey::Wiki, "wiki dataset"),
            (SourceKey::Archive, "archive dataset"),
        ];
        let reverse = [
            (SourceKey::Archive, "archive dataset"),
            (SourceKey::Wiki, "wiki dataset"),
            (SourceKey::Compass, "compass dataset"),
        ];
        let route = |available: &[(SourceKey, &'static str)]| {
            resolve_sources(&sources, |key| {
                available
                    .iter()
                    .find_map(|(candidate, dataset)| (*candidate == key).then_some(*dataset))
            })
            .into_iter()
            .map(|(idx, source, dataset)| (idx, source.key(), kind(source), dataset))
            .collect::<Vec<_>>()
        };

        assert_eq!(route(&forward), route(&reverse));
    }

    const SECOND: i128 = 1_000_000_000;
    const MINUTE: i128 = 60 * SECOND;
    /// An arbitrary origin, so no test relies on times near zero.
    const T0: i128 = 1_000_000 * MINUTE;
    /// The default zoom: two pixels per minute.
    const DEFAULT_NS_PER_PX: f64 = 30.0 * SECOND as f64;
    const HEIGHT: f32 = 800.0;

    fn instant(ns: i128) -> Inline<NsTAIInterval> {
        let epoch = Epoch::from_tai_duration(HifiDuration::from_total_nanoseconds(ns));
        (epoch, epoch).try_to_inline().expect("valid interval")
    }

    fn text(value: &str) -> TextHandle {
        let blob: Blob<UTF8String> = value.to_owned().to_blob();
        blob.get_handle()
    }

    fn id(n: u64) -> Id {
        let mut raw = [0u8; 16];
        raw[0] = 0xa5;
        raw[8..].copy_from_slice(&n.to_be_bytes());
        Id::new(raw).expect("non-nil")
    }

    fn archive_of(facts: &TribleSet) -> FactArchive {
        FactArchive::new(vec![SuccinctArchive::from(facts)])
    }

    fn reason_source() -> Vec<TimelineSource> {
        vec![TimelineSource::Reason {
            key: SourceKey::Reason,
            label: "reason".into(),
        }]
    }

    /// One reasoning note at each time.
    fn reasons(times: impl IntoIterator<Item = i128>) -> FactArchive {
        let mut facts = TribleSet::new();
        for (n, at) in times.into_iter().enumerate() {
            let rid = id(n as u64 + 1);
            facts += entity! { ExclusiveId::force_ref(&rid) @
                metadata::tag: &KIND_REASON_ID,
                reason_attrs::text: text(&format!("thought {n}")),
                metadata::created_at: instant(at),
            }
            .into_facts();
        }
        archive_of(&facts)
    }

    /// Draw one viewport the way `paint_viewport` does, with a reader that
    /// counts on `Activity::reads` and answers every handle with "text".
    fn draw(
        activity: &mut Activity,
        sources: &[TimelineSource],
        facts: &FactArchive,
        view_start: i128,
        ns_per_px: f64,
    ) -> Frame {
        let mut select = |lo, hi, out: &mut Vec<Mark>| {
            sources
                .iter()
                .enumerate()
                .map(|(idx, source)| marks_in(idx, source, facts, lo, hi, out))
                .sum::<usize>()
        };
        let lookup = |_: usize| Some(facts);
        let mut read = |_: usize, _: TextHandle| "text".to_owned();
        activity.frame(
            view_start,
            ns_per_px,
            HEIGHT,
            &mut select,
            &lookup,
            &mut read,
        )
    }

    fn drawn_marks(frame: &Frame) -> usize {
        frame.labels.len() + frame.strips.iter().map(|strip| strip.count).sum::<usize>()
    }

    /// History outside the window is neither decoded nor read: the seek
    /// lands past it, so the selection examined no time fact at all.
    #[test]
    fn offscreen_history_is_neither_selected_nor_read() {
        let facts = reasons((0..2_000).map(|n| T0 + n * MINUTE));
        let mut activity = Activity::new(BTreeMap::new());
        let far_later = T0 + 30 * 24 * 60 * MINUTE;

        let frame = draw(
            &mut activity,
            &reason_source(),
            &facts,
            far_later,
            DEFAULT_NS_PER_PX,
        );

        assert!(frame.labels.is_empty() && frame.strips.is_empty());
        assert_eq!(activity.scanned, 0);
        assert_eq!(activity.reads, 0);
        assert_eq!(activity.selections, 1);
    }

    /// Marks denser than a chip are drawn from metadata alone: every
    /// visible mark is counted in a strip, and no text is read, although
    /// all of them are inside the viewport.
    #[test]
    fn dense_marks_draw_as_strips_without_reading() {
        let facts = reasons((0..5_000).map(|n| T0 + n * SECOND));
        let mut activity = Activity::new(BTreeMap::new());
        // The default zoom shows 400 minutes; 5,000 seconds fit in 167 px.
        let view_start = T0 + 5_000 * SECOND + 10 * MINUTE;

        let frame = draw(
            &mut activity,
            &reason_source(),
            &facts,
            view_start,
            DEFAULT_NS_PER_PX,
        );

        assert!(frame.labels.is_empty());
        assert_eq!(drawn_marks(&frame), 5_000);
        assert!(frame.strips.len() <= 170, "{} strips", frame.strips.len());
        assert_eq!(activity.reads, 0);
    }

    /// A lone mark beside a dense cluster gets its chip; only it is read.
    #[test]
    fn a_sparse_mark_beside_a_dense_cluster_alone_is_read() {
        let lone = T0 + 60 * MINUTE;
        let facts = reasons((0..1_000).map(|n| T0 + n * SECOND).chain([lone]));
        let mut activity = Activity::new(BTreeMap::new());

        let frame = draw(
            &mut activity,
            &reason_source(),
            &facts,
            lone + MINUTE,
            DEFAULT_NS_PER_PX,
        );

        assert_eq!(frame.labels.len(), 1);
        assert_eq!(frame.labels[0].1.ts_ns, lone);
        assert_eq!(drawn_marks(&frame), 1_001);
        assert_eq!(activity.reads, 1);
    }

    /// Zooming in re-selects a small window and reads exactly the labels
    /// that now have room, never more than one per chip height.
    #[test]
    fn zooming_in_reads_only_the_labels_drawn() {
        let facts = reasons((0..5_000).map(|n| T0 + n * SECOND));
        let sources = reason_source();
        let mut activity = Activity::new(BTreeMap::new());
        let view_start = T0 + 2_500 * SECOND;

        draw(
            &mut activity,
            &sources,
            &facts,
            view_start,
            DEFAULT_NS_PER_PX,
        );
        assert_eq!(activity.reads, 0);

        // Twenty pixels a second: 40 s in view, each mark a chip apart.
        let frame = draw(
            &mut activity,
            &sources,
            &facts,
            view_start,
            SECOND as f64 / 20.0,
        );

        assert_eq!(activity.selections, 2);
        assert!(
            activity.scanned <= 3 * 40 + 3,
            "scanned {}",
            activity.scanned
        );
        assert!(frame.strips.is_empty());
        assert!(
            (40..=41).contains(&frame.labels.len()),
            "{} labels",
            frame.labels.len()
        );
        assert!(frame.labels.len() <= (HEIGHT / CHIP_H) as usize + 1);
        assert_eq!(activity.reads, frame.labels.len());
        assert!(frame
            .labels
            .iter()
            .all(|(_, _, shown)| shown.summary == "text"));
    }

    /// Panning inside the selected range re-selects nothing and reads only
    /// the labels it newly draws; panning past it selects once more.
    #[test]
    fn panning_reads_only_newly_drawn_labels() {
        let facts = reasons((0..5_000).map(|n| T0 + n * SECOND));
        let sources = reason_source();
        let mut activity = Activity::new(BTreeMap::new());
        let ns_per_px = SECOND as f64 / 20.0;
        let view_start = T0 + 2_500 * SECOND;

        let first = draw(&mut activity, &sources, &facts, view_start, ns_per_px);
        let before: HashSet<Id> = first
            .labels
            .iter()
            .map(|(_, mark, _)| mark.entity_id)
            .collect();
        let reads = activity.reads;

        let panned = draw(
            &mut activity,
            &sources,
            &facts,
            view_start - 10 * SECOND,
            ns_per_px,
        );
        let new = panned
            .labels
            .iter()
            .filter(|(_, mark, _)| !before.contains(&mark.entity_id))
            .count();

        assert_eq!(activity.selections, 1);
        assert!((10..=11).contains(&new), "{new} new labels");
        assert_eq!(activity.reads - reads, new);

        // Back where it started: everything drawn has been read already.
        let reads = activity.reads;
        draw(&mut activity, &sources, &facts, view_start, ns_per_px);
        assert_eq!(activity.reads, reads);

        // Far past the selected range: one more selection.
        draw(
            &mut activity,
            &sources,
            &facts,
            view_start - 600 * SECOND,
            ns_per_px,
        );
        assert_eq!(activity.selections, 2);
    }

    /// One chip per envelope, at its newest time: a window holding only an
    /// older witness shows nothing.
    #[test]
    fn a_message_shows_only_where_its_newest_time_is() {
        let message = id(1);
        let mut facts = TribleSet::new();
        for at in [T0, T0 + 600 * MINUTE] {
            facts += entity! { ExclusiveId::force_ref(&message) @
                metadata::tag: &KIND_MESSAGE_ID,
                message_attrs::from: id(2),
                message_attrs::to: id(3),
                message_attrs::body: text("hello"),
                metadata::created_at: instant(at),
            }
            .into_facts();
        }
        let facts = archive_of(&facts);
        let source = TimelineSource::LocalMessages {
            key: SourceKey::Messages,
            label: "local".into(),
        };

        let mut old = Vec::new();
        local_marks(0, &facts, T0 - MINUTE, T0 + MINUTE, &mut old);
        let mut new = Vec::new();
        marks_in(
            0,
            &source,
            &facts,
            T0 + 599 * MINUTE,
            T0 + 601 * MINUTE,
            &mut new,
        );

        assert!(old.is_empty());
        assert_eq!(new.len(), 1);
        assert_eq!(
            new[0].ts_ns,
            interval_start(instant(T0 + 600 * MINUTE)).unwrap()
        );
    }

    /// A Codex fork replays its parent's records, and the replay's time
    /// annotates the parent's receipt beside the original's. The timeline
    /// shows that receipt where `archive list` does: at its earliest time.
    #[test]
    fn a_receipt_observed_twice_is_shown_at_its_earliest_time() {
        let receipt = Id::new([7; 16]).expect("non-nil");
        let original = instant(1_000 * SECOND);
        let replay = instant(2_000 * SECOND);
        let mut facts = TribleSet::new();
        for timestamp in [original, replay] {
            facts += entity! { ExclusiveId::force_ref(&receipt) @
                archive::source_projection::source_timestamp: timestamp,
            }
            .into_facts();
        }
        let facts = archive_of(&facts);

        assert_eq!(earliest_source_time(&facts, receipt), Some(original));
    }

    /// The first stop text in `archive::operations::CLASSIFIER_STOPS`.
    const STOP: &str = "Your response above was stopped by a safety classifier \u{2014} this is not a tool or API error. The rest of it was withheld, and tool calls in it that had not finished did not run. Do not produce that content again, even reworded.";

    /// Archive windows find a receipt by its earliest source time and a
    /// receipt without one by its node's time; a node a classifier stop
    /// followed is drawn as withheld and its text is not read.
    #[test]
    fn archive_marks_keep_source_time_fallback_and_withholding() {
        let (sourced, fallback, stop_node) = (id(10), id(11), id(12));
        let (sourced_block, fallback_block) = (id(20), id(21));
        let (sourced_fact, fallback_fact, stop_fact) = (id(30), id(31), id(32));
        let mut facts = TribleSet::new();
        for (block, fact, at) in [
            (sourced_block, sourced_fact, T0),
            (fallback_block, fallback_fact, T0 + 100 * MINUTE),
        ] {
            facts += entity! { ExclusiveId::force_ref(&block) @
                archive::block::holds: fact,
                archive::block::timestamp: instant(at),
            }
            .into_facts();
            facts += entity! { ExclusiveId::force_ref(&fact) @
                archive::content_fact::payload: text("said"),
            }
            .into_facts();
        }
        // The source time moves the first receipt away from its node's time.
        facts += entity! { ExclusiveId::force_ref(&sourced) @
            metadata::tag: &archive::source_projection::KIND,
            archive::source_projection::projects_to: sourced_block,
            archive::source_projection::source_timestamp: instant(T0 + 50 * MINUTE),
        }
        .into_facts();
        facts += entity! { ExclusiveId::force_ref(&fallback) @
            metadata::tag: &archive::source_projection::KIND,
            archive::source_projection::projects_to: fallback_block,
        }
        .into_facts();
        // A stop follows the fallback receipt's node.
        facts += entity! { ExclusiveId::force_ref(&stop_node) @
            archive::block::previous: fallback_block,
            archive::block::holds: stop_fact,
        }
        .into_facts();
        facts += entity! { ExclusiveId::force_ref(&stop_fact) @
            archive::content_fact::payload: text(STOP),
        }
        .into_facts();
        let facts = archive_of(&facts);

        let window = |lo, hi| {
            let mut out = Vec::new();
            archive_marks(0, &facts, lo, hi, &mut out);
            out.into_iter()
                .map(|mark| mark.entity_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(window(T0 - MINUTE, T0 + MINUTE), Vec::<Id>::new());
        assert_eq!(window(T0 + 49 * MINUTE, T0 + 51 * MINUTE), vec![sourced]);
        assert_eq!(window(T0 + 99 * MINUTE, T0 + 101 * MINUTE), vec![fallback]);

        let mut reads = 0;
        let mut read = |_: TextHandle| {
            reads += 1;
            "said".to_owned()
        };
        let mut stopped = None;
        let withheld = hydrate(
            Label::Archive {
                block: fallback_block,
            },
            &facts,
            &mut stopped,
            &mut read,
        );
        assert_eq!(
            withheld.summary,
            "[withheld: a classifier stop followed this]"
        );
        let shown = hydrate(
            Label::Archive {
                block: sourced_block,
            },
            &facts,
            &mut stopped,
            &mut read,
        );
        assert_eq!(shown.summary, "said");
        assert_eq!(reads, 1);
    }
}
