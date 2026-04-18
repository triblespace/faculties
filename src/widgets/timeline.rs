//! GORBIE-embeddable branch timeline.
//!
//! A pan/zoom time axis that overlays events from one or more pile
//! branches on a single vertical timeline (newest at top, oldest at
//! bottom). v1 painted opaque "commit happened" bars for a single
//! branch; v2 adds per-event-type decoration for the most visible
//! daily activity branches:
//!
//! * Compass — goal status changes (pill + goal title)
//! * Local messages — body preview with sender/recipient pills
//! * Wiki — fragment-version commits with title
//! * Commits — generic commit-bar rendering for arbitrary branches
//!
//! The widget is `Send + Sync` (GORBIE's state storage requires it
//! across threads), so the live connection lives behind a
//! `parking_lot::Mutex`.
//!
//! ```ignore
//! // Single-branch: generic commit bars (v1 behavior).
//! let mut timeline = BranchTimeline::new("./self.pile", "wiki");
//! // Inside a GORBIE card:
//! timeline.render(ctx);
//!
//! // Multi-branch: decorated overlay.
//! let mut timeline = BranchTimeline::multi("./self.pile", vec![
//!     TimelineSource::Compass { branch: "compass".into() },
//!     TimelineSource::LocalMessages { branch: "local-messages".into() },
//!     TimelineSource::Wiki { branch: "wiki".into() },
//! ]);
//! timeline.render(ctx);
//! ```
//!
//! Input handling:
//! * scroll = pan (vertical)
//! * ctrl+scroll or horizontal scroll = zoom
//! * drag = pan
//! * double-click = jump to "now"
//!
//! The ruler is a "four-sine" design: overlapping cosines at the natural
//! time periods (minute, hour, day) that produce constructive interference
//! at nice times. Labels are placed independently at the coarsest interval
//! that gives ~6-10 labels per viewport.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hifitime::{Duration as HifiDuration, Epoch};
use parking_lot::Mutex;
use GORBIE::card_ctx::GRID_ROW_MODULE;
use GORBIE::prelude::CardCtx;
use triblespace::core::id::Id;
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{
    ancestors, BlobStore, BlobStoreGet, BranchStore, CommitSelector, CommitSet, Repository,
    Workspace,
};
use triblespace::core::trible::TribleSet;
use triblespace::core::value::schemas::hash::{Blake3, Handle};
use triblespace::core::value::Value;
use triblespace::macros::{find, pattern};
use triblespace::prelude::blobschemas::{LongString, SimpleArchive};
use triblespace::prelude::View;

use crate::schemas::compass::{
    board as compass_attrs, KIND_GOAL_ID, KIND_NOTE_ID, KIND_STATUS_ID,
};
use crate::schemas::local_messages::{local as local_attrs, KIND_MESSAGE_ID};
use crate::schemas::wiki::{attrs as wiki_attrs, KIND_VERSION_ID};

/// Handle to a long-string blob (branch names, titles, bodies, notes).
type TextHandle = Value<Handle<Blake3, LongString>>;
/// A commit blob handle (SimpleArchive of its metadata tribles).
type CommitHandleValue = Value<Handle<Blake3, SimpleArchive>>;

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

/// Current TAI time as a ns key, or 0 if the system clock is unavailable.
fn now_key() -> i128 {
    Epoch::now()
        .map(|e| e.to_tai_duration().total_nanoseconds())
        .unwrap_or(0)
}

/// First 8 hex chars of an Id — compact label for pills / hover.
fn id_prefix(id: Id) -> String {
    let s = format!("{id:x}");
    if s.len() > 8 { s[..8].to_string() } else { s }
}

/// Trim a string to `max` chars on a single line, replacing inner
/// newlines with spaces. Used for body/title previews on event rows.
fn preview(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
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

/// Branch + decoration description for the multi-branch constructor.
#[derive(Clone, Debug)]
pub enum TimelineSource {
    /// Just paint commits as plain bars (useful for arbitrary branches).
    Commits {
        branch: String,
        label: String,
        color: egui::Color32,
    },
    /// Compass branch — render goal status changes with the goal title
    /// and status-color pill.
    Compass { branch: String },
    /// Local-messages branch — render each message with sender/body
    /// preview.
    LocalMessages { branch: String },
    /// Wiki branch — render fragment-version commits with title.
    Wiki { branch: String },
}

/// Coarse kind used on the widget's `selected_event` and as a color key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Commits,
    Compass,
    LocalMessages,
    Wiki,
}

impl TimelineSource {
    fn branch(&self) -> &str {
        match self {
            TimelineSource::Commits { branch, .. } => branch,
            TimelineSource::Compass { branch } => branch,
            TimelineSource::LocalMessages { branch } => branch,
            TimelineSource::Wiki { branch } => branch,
        }
    }

    /// A short (≤6 char) source label used in the pill.
    fn label(&self) -> String {
        match self {
            TimelineSource::Commits { label, .. } => label.clone(),
            TimelineSource::Compass { .. } => "goals".to_string(),
            TimelineSource::LocalMessages { .. } => "local".to_string(),
            TimelineSource::Wiki { .. } => "wiki".to_string(),
        }
    }

    fn color(&self) -> egui::Color32 {
        match self {
            TimelineSource::Commits { color, .. } => *color,
            // RAL 1012 lemon yellow — matches playground color_goals.
            TimelineSource::Compass { .. } => egui::Color32::from_rgb(0xd9, 0xc2, 0x2e),
            // RAL 6032 signal green — matches playground color_local_msg.
            TimelineSource::LocalMessages { .. } => egui::Color32::from_rgb(0x23, 0x7f, 0x52),
            // RAL 3012 beige red — matches playground color_wiki.
            TimelineSource::Wiki { .. } => egui::Color32::from_rgb(0xc1, 0x87, 0x6b),
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

// ── Event model ──────────────────────────────────────────────────────

/// A single point on the timeline. Flat enough that we can sort and
/// paint without re-querying per frame. Per-kind fields live as
/// optional extras on the row so the painter can decorate without
/// re-reading the fact space.
#[derive(Clone, Debug)]
struct Event {
    source_idx: usize,
    kind: SourceKind,
    entity_id: Id,
    ts_ns: i128,
    /// Primary one-line preview (goal title, message body, wiki title,
    /// or short commit id for generic sources).
    summary: String,
    /// Optional kanban-status pill label (Compass).
    status: Option<String>,
    /// Optional sender pill label (LocalMessages — "<from_prefix> → <to_prefix>").
    from_to: Option<String>,
}

// ── Live connection ──────────────────────────────────────────────────

/// Opened pile + per-source workspaces and cached fact spaces.
///
/// All sources share a single `Repository`/`Pile` handle for efficiency.
/// Each requested branch is pulled once; its checked-out `TribleSet`
/// feeds the event query, and the `Workspace` stays live so blob reads
/// (bodies, titles, notes) resolve against the correct branch.
struct MultiLive {
    sources: Vec<SourceLive>,
    events: Vec<Event>,
}

struct SourceLive {
    source: TimelineSource,
    ws: Workspace<Pile<Blake3>>,
    space: TribleSet,
}

impl MultiLive {
    fn open(path: &Path, sources: &[TimelineSource]) -> Result<Self, String> {
        let mut pile = Pile::<Blake3>::open(path).map_err(|e| format!("open pile: {e:?}"))?;
        if let Err(err) = pile.restore() {
            let _ = pile.close();
            return Err(format!("restore: {err:?}"));
        }
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core06::OsRng);
        let mut repo = Repository::new(pile, signing_key, TribleSet::new())
            .map_err(|e| format!("repo: {e:?}"))?;
        repo.storage_mut()
            .refresh()
            .map_err(|e| format!("refresh: {e:?}"))?;

        let mut live = MultiLive {
            sources: Vec::with_capacity(sources.len()),
            events: Vec::new(),
        };
        for src in sources {
            let bid = find_branch(&mut repo, src.branch())
                .ok_or_else(|| format!("no '{}' branch found", src.branch()))?;
            let mut ws = repo.pull(bid).map_err(|e| format!("pull: {e:?}"))?;
            // For commit-only rendering we do NOT need a checked-out
            // fact space; skip the work for that case.
            let space = match src {
                TimelineSource::Commits { .. } => TribleSet::new(),
                _ => ws
                    .checkout(..)
                    .map_err(|e| format!("checkout: {e:?}"))?
                    .into_facts(),
            };
            live.sources.push(SourceLive {
                source: src.clone(),
                ws,
                space,
            });
        }
        live.refresh_events();
        Ok(live)
    }

    /// Re-enumerate events from every source. Eager — good enough for
    /// v1; progressive loading is future work.
    fn refresh_events(&mut self) {
        let mut out: Vec<Event> = Vec::new();
        for (idx, s) in self.sources.iter_mut().enumerate() {
            match s.source.clone() {
                TimelineSource::Commits { .. } => {
                    collect_commit_events(idx, s, &mut out);
                }
                TimelineSource::Compass { .. } => {
                    collect_compass_events(idx, s, &mut out);
                }
                TimelineSource::LocalMessages { .. } => {
                    collect_local_events(idx, s, &mut out);
                }
                TimelineSource::Wiki { .. } => {
                    collect_wiki_events(idx, s, &mut out);
                }
            }
        }
        out.sort_by_key(|e| e.ts_ns);
        self.events = out;
    }
}

/// Walk every commit reachable from HEAD and emit one Event per commit.
/// Commits without `created_at` are skipped — they're merge commits by
/// design and carry no author-time bits.
fn collect_commit_events(idx: usize, s: &mut SourceLive, out: &mut Vec<Event>) {
    let Some(head) = s.ws.head() else {
        return;
    };
    let Ok(set): Result<CommitSet, _> = ancestors(head).select(&mut s.ws) else {
        return;
    };
    for raw in set.iter() {
        let handle: CommitHandleValue = Value::new(*raw);
        let Ok(meta) = s.ws.get::<TribleSet, SimpleArchive>(handle) else {
            continue;
        };
        if let Some((cid, ts)) = find!(
            (cid: Id, ts: (i128, i128)),
            pattern!(&meta, [{ ?cid @ metadata::created_at: ?ts }])
        )
        .next()
        {
            out.push(Event {
                source_idx: idx,
                kind: SourceKind::Commits,
                entity_id: cid,
                ts_ns: ts.0,
                summary: id_prefix(cid),
                status: None,
                from_to: None,
            });
        }
    }
}

/// Emit a Compass event per status-change entity. Status events carry
/// `compass::task` → goal, `compass::status` → text, plus `created_at`.
/// We also record a plain "goal created" event for each goal so an
/// otherwise-quiet board still paints something.
fn collect_compass_events(idx: usize, s: &mut SourceLive, out: &mut Vec<Event>) {
    // Cache titles per goal id so repeated status changes don't re-read
    // the same blob N times.
    let mut title_by_goal: HashMap<Id, String> = HashMap::new();

    // Goal creations — useful even when no status transition exists yet.
    let goal_rows: Vec<(Id, TextHandle, (i128, i128))> = find!(
        (gid: Id, title: TextHandle, ts: (i128, i128)),
        pattern!(&s.space, [{
            ?gid @
            metadata::tag: &KIND_GOAL_ID,
            compass_attrs::title: ?title,
            metadata::created_at: ?ts,
        }])
    )
    .collect();

    for (gid, title_h, ts) in goal_rows {
        let title = s
            .ws
            .get::<View<str>, LongString>(title_h)
            .map(|v| {
                let s: &str = v.as_ref();
                s.to_string()
            })
            .unwrap_or_default();
        title_by_goal.insert(gid, title.clone());
        out.push(Event {
            source_idx: idx,
            kind: SourceKind::Compass,
            entity_id: gid,
            ts_ns: ts.0,
            summary: preview(&title, 80),
            status: Some("created".to_string()),
            from_to: None,
        });
    }

    // Status transitions.
    let status_rows: Vec<(Id, Id, String, (i128, i128))> = find!(
        (event_id: Id, gid: Id, status: String, ts: (i128, i128)),
        pattern!(&s.space, [{
            ?event_id @
            metadata::tag: &KIND_STATUS_ID,
            compass_attrs::task: ?gid,
            compass_attrs::status: ?status,
            metadata::created_at: ?ts,
        }])
    )
    .collect();

    for (event_id, gid, status, ts) in status_rows {
        let title = title_by_goal
            .get(&gid)
            .cloned()
            .unwrap_or_else(|| id_prefix(gid));
        out.push(Event {
            source_idx: idx,
            kind: SourceKind::Compass,
            entity_id: event_id,
            ts_ns: ts.0,
            summary: preview(&title, 80),
            status: Some(status),
            from_to: None,
        });
    }

    // Notes — painted as a small "note" pill so activity on goals is
    // visible on the timeline even when the status hasn't moved.
    let note_rows: Vec<(Id, Id, TextHandle, (i128, i128))> = find!(
        (event_id: Id, gid: Id, note: TextHandle, ts: (i128, i128)),
        pattern!(&s.space, [{
            ?event_id @
            metadata::tag: &KIND_NOTE_ID,
            compass_attrs::task: ?gid,
            compass_attrs::note: ?note,
            metadata::created_at: ?ts,
        }])
    )
    .collect();

    for (event_id, gid, note_h, ts) in note_rows {
        let body = s
            .ws
            .get::<View<str>, LongString>(note_h)
            .map(|v| {
                let s: &str = v.as_ref();
                s.to_string()
            })
            .unwrap_or_default();
        let title = title_by_goal
            .get(&gid)
            .cloned()
            .unwrap_or_else(|| id_prefix(gid));
        let summary = if body.is_empty() {
            preview(&title, 80)
        } else {
            preview(&format!("{title} — {body}"), 80)
        };
        out.push(Event {
            source_idx: idx,
            kind: SourceKind::Compass,
            entity_id: event_id,
            ts_ns: ts.0,
            summary,
            status: Some("note".to_string()),
            from_to: None,
        });
    }
}

/// Emit a LocalMessages event per message. The "sender → recipient"
/// prefix is shown as a pill; the body preview is the summary.
fn collect_local_events(idx: usize, s: &mut SourceLive, out: &mut Vec<Event>) {
    let rows: Vec<(Id, Id, Id, TextHandle, (i128, i128))> = find!(
        (mid: Id, from: Id, to: Id, body: TextHandle, ts: (i128, i128)),
        pattern!(&s.space, [{
            ?mid @
            metadata::tag: &KIND_MESSAGE_ID,
            local_attrs::from: ?from,
            local_attrs::to: ?to,
            local_attrs::body: ?body,
            metadata::created_at: ?ts,
        }])
    )
    .collect();

    for (mid, from, to, body_h, ts) in rows {
        let body = s
            .ws
            .get::<View<str>, LongString>(body_h)
            .map(|v| {
                let s: &str = v.as_ref();
                s.to_string()
            })
            .unwrap_or_default();
        out.push(Event {
            source_idx: idx,
            kind: SourceKind::LocalMessages,
            entity_id: mid,
            ts_ns: ts.0,
            summary: preview(&body, 80),
            status: None,
            from_to: Some(format!("{} → {}", id_prefix(from), id_prefix(to))),
        });
    }
}

/// Emit a Wiki event per fragment-version. The title blob is pulled
/// eagerly so we don't need per-frame blob I/O during paint.
fn collect_wiki_events(idx: usize, s: &mut SourceLive, out: &mut Vec<Event>) {
    let rows: Vec<(Id, TextHandle, (i128, i128))> = find!(
        (vid: Id, title: TextHandle, ts: (i128, i128)),
        pattern!(&s.space, [{
            ?vid @
            metadata::tag: &KIND_VERSION_ID,
            wiki_attrs::title: ?title,
            metadata::created_at: ?ts,
        }])
    )
    .collect();

    for (vid, title_h, ts) in rows {
        let title = s
            .ws
            .get::<View<str>, LongString>(title_h)
            .map(|v| {
                let s: &str = v.as_ref();
                s.to_string()
            })
            .unwrap_or_default();
        out.push(Event {
            source_idx: idx,
            kind: SourceKind::Wiki,
            entity_id: vid,
            ts_ns: ts.0,
            summary: preview(&title, 80),
            status: None,
            from_to: None,
        });
    }
}

/// Find a branch by name in a pile-backed repository.
fn find_branch(repo: &mut Repository<Pile<Blake3>>, name: &str) -> Option<Id> {
    let reader = repo.storage_mut().reader().ok()?;
    for item in repo.storage_mut().branches().ok()? {
        let bid = item.ok()?;
        let head = repo.storage_mut().head(bid).ok()??;
        let meta: TribleSet = reader.get(head).ok()?;
        let branch_name = find!(
            (h: TextHandle),
            pattern!(&meta, [{ metadata::name: ?h }])
        )
        .into_iter()
        .next()
        .and_then(|(h,)| reader.get::<View<str>, LongString>(h).ok())
        .map(|v: View<str>| {
            let s: &str = v.as_ref();
            s.to_string()
        });
        if branch_name.as_deref() == Some(name) {
            return Some(bid);
        }
    }
    None
}

// ── Widget ───────────────────────────────────────────────────────────

/// GORBIE-embeddable pan/zoom timeline for one or more pile branches.
///
/// Paints a full-width vertical time axis (newest at top, oldest at
/// bottom) with:
///
/// * a four-sine ruler (constructive interference at minute / hour /
///   day boundaries)
/// * time labels at the coarsest interval that fits
/// * per-source event chips with source-specific decoration
///
/// The pile + all requested source branches are opened lazily on the
/// first render, in the same pattern as [`WikiViewer`](crate::widgets::WikiViewer).
pub struct BranchTimeline {
    pile_path: PathBuf,
    /// One entry per requested source. Single-branch constructor builds
    /// a single `TimelineSource::Commits` entry with a default label.
    sources: Vec<TimelineSource>,
    viewport_height: f32,
    // Wrapped in a Mutex so the widget is `Send + Sync` — GORBIE's state
    // storage requires that across threads, and `Workspace<Pile<Blake3>>`
    // uses interior-mutability types (Cell/RefCell) that aren't Sync.
    live: Option<Mutex<MultiLive>>,
    error: Option<String>,
    /// Top edge of viewport, in TAI ns. Newest visible time.
    timeline_start: i128,
    /// Pixels per minute of wall time.
    timeline_scale: f32,
    /// Tracks the first render so we can initialize `timeline_start` to
    /// "now" before painting.
    first_render: bool,
    /// Manual drag tracking — avoids registering `Sense::drag` on the
    /// viewport, which would trigger an egui 0.33/0.34 hit_test panic
    /// when the section header above us has `Sense::click`. Holds the
    /// pointer y from the previous frame while a drag is in progress.
    drag_last_y: Option<f32>,
    /// The most-recently-clicked event, if any. Hosts can read this to
    /// drive floating detail cards.
    pub selected_event: Option<(SourceKind, Id)>,
}

impl BranchTimeline {
    /// Single-branch timeline — renders commits on `branch_name` as
    /// generic commit bars. Matches v1 behavior.
    pub fn new(pile_path: impl Into<PathBuf>, branch_name: impl Into<String>) -> Self {
        let branch = branch_name.into();
        // Default commit-bar color: muted amber (matches v1 "commit_color").
        let color = egui::Color32::from_rgb(0xff, 0xc8, 0x3a);
        Self {
            pile_path: pile_path.into(),
            sources: vec![TimelineSource::Commits {
                label: branch.clone(),
                branch,
                color,
            }],
            viewport_height: DEFAULT_VIEWPORT_HEIGHT,
            live: None,
            error: None,
            timeline_start: 0,
            timeline_scale: TIMELINE_DEFAULT_SCALE,
            first_render: true,
            drag_last_y: None,
            selected_event: None,
        }
    }

    /// Multi-branch overlay — each source paints its own events on the
    /// shared axis.
    pub fn multi(pile_path: impl Into<PathBuf>, sources: Vec<TimelineSource>) -> Self {
        Self {
            pile_path: pile_path.into(),
            sources,
            viewport_height: DEFAULT_VIEWPORT_HEIGHT,
            live: None,
            error: None,
            timeline_start: 0,
            timeline_scale: TIMELINE_DEFAULT_SCALE,
            first_render: true,
            drag_last_y: None,
            selected_event: None,
        }
    }

    /// Override the viewport height (pixels). Defaults to 800.
    pub fn with_height(mut self, height: f32) -> Self {
        self.viewport_height = height.max(48.0);
        self
    }

    /// Retarget at a different pile. No-op if the path is unchanged.
    pub fn set_pile_path(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        if path == self.pile_path {
            return;
        }
        self.pile_path = path;
        self.live = None;
        self.error = None;
        self.first_render = true;
        self.drag_last_y = None;
        self.selected_event = None;
    }

    /// Render the timeline into a GORBIE card context.
    pub fn render(&mut self, ctx: &mut CardCtx<'_>) {
        // Lazy pile open on first render.
        if self.live.is_none() && self.error.is_none() {
            match MultiLive::open(&self.pile_path, &self.sources) {
                Ok(live) => self.live = Some(Mutex::new(live)),
                Err(e) => self.error = Some(e),
            }
        }

        if let Some(err) = &self.error {
            ctx.label(format!("branch timeline error: {err}"));
            return;
        }

        let Some(live_lock) = self.live.as_ref() else {
            ctx.label("branch timeline not initialized");
            return;
        };

        let now = now_key();
        if self.first_render {
            self.timeline_start = now;
            self.first_render = false;
            // Opportunistically refresh on first render in case events
            // were added between construction and first paint.
            live_lock.lock().refresh_events();
        }

        // Snapshot events + source descriptors so we don't hold the
        // mutex across egui calls.
        let (events, sources): (Vec<Event>, Vec<TimelineSource>) = {
            let guard = live_lock.lock();
            (guard.events.clone(), guard.sources.iter().map(|s| s.source.clone()).collect())
        };

        let viewport_height = self.viewport_height;

        ctx.section("Activity", |ctx| {
            ctx.grid(|g| {
                // Source legend — one chip per source with count.
                g.full(|ctx| {
                    let ui = ctx.ui_mut();
                    ui.horizontal_wrapped(|ui| {
                        for (i, s) in sources.iter().enumerate() {
                            let count = events.iter().filter(|e| e.source_idx == i).count();
                            render_pill(ui, &format!("{} · {count}", s.label()), s.color());
                        }
                    });
                });
                // Viewport spans the whole grid row.
                g.full(|ctx| {
                    self.paint_viewport(ctx, viewport_height, now, &events, &sources);
                });
            });
        });
    }

    /// Paint the timeline viewport. All pan/zoom/scroll logic lives here.
    fn paint_viewport(
        &mut self,
        ctx: &mut CardCtx<'_>,
        viewport_height: f32,
        now: i128,
        events: &[Event],
        sources: &[TimelineSource],
    ) {
        let ui = ctx.ui_mut();
        let scroll_speed = 3.0;
        let viewport_width = ui.available_width();
        // Click-only sense. We implement drag-to-pan manually below via
        // raw pointer state to sidestep egui's hit_test unwrap panic that
        // fires when a click-sensing widget (like the section header) and
        // a drag-sensing widget (what this viewport would be) both land
        // close to the cursor.
        let (viewport_rect, viewport_response) = ui.allocate_exact_size(
            egui::vec2(viewport_width, viewport_height),
            egui::Sense::click(),
        );

        // Input handling — compute ns_per_px from CURRENT scale.
        {
            let ns_per_px = 60_000_000_000.0 / self.timeline_scale as f64;

            if viewport_response.hovered() {
                let (scroll_y, scroll_x, ctrl, pointer_pos) = ui.input(|i| {
                    (
                        i.smooth_scroll_delta.y,
                        i.smooth_scroll_delta.x,
                        i.modifiers.command || i.modifiers.ctrl,
                        i.pointer.hover_pos(),
                    )
                });

                let cursor_rel_y = pointer_pos
                    .map(|p| (p.y - viewport_rect.top()).max(0.0))
                    .unwrap_or(viewport_height * 0.5);

                let cursor_time =
                    self.timeline_start - (cursor_rel_y as f64 * ns_per_px) as i128;

                if !ctrl && scroll_y != 0.0 {
                    let pan_ns = (scroll_y as f64 * scroll_speed * ns_per_px) as i128;
                    self.timeline_start += pan_ns;
                }

                let zoom_factor = if ctrl && scroll_y != 0.0 {
                    if scroll_y > 0.0 {
                        1.15
                    } else {
                        1.0 / 1.15
                    }
                } else if scroll_x != 0.0 {
                    if scroll_x > 0.0 {
                        1.08
                    } else {
                        1.0 / 1.08
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
                }

                ui.ctx().input_mut(|i| {
                    i.smooth_scroll_delta = egui::Vec2::ZERO;
                });
            }

            // Manual drag-to-pan (see comment on the allocate_exact_size
            // sense choice above for why we don't use `Sense::drag`).
            let (primary_down, pointer_pos) = ui
                .input(|i| (i.pointer.primary_down(), i.pointer.hover_pos()));
            let in_viewport =
                pointer_pos.map(|p| viewport_rect.contains(p)).unwrap_or(false);
            if primary_down && in_viewport {
                if let Some(p) = pointer_pos {
                    if let Some(last_y) = self.drag_last_y {
                        let drag_delta = p.y - last_y;
                        let pan_ns = (drag_delta as f64 * ns_per_px) as i128;
                        self.timeline_start += pan_ns;
                    }
                    self.drag_last_y = Some(p.y);
                }
            } else {
                self.drag_last_y = None;
            }

            if viewport_response.double_clicked() {
                self.timeline_start = now;
            }
        }

        // Recompute bounds AFTER input with final scale.
        let ns_per_px = 60_000_000_000.0 / self.timeline_scale as f64;
        let viewport_ns = (viewport_height as f64 * ns_per_px) as i128;
        let view_start = self.timeline_start;
        let view_end = view_start - viewport_ns;

        let painter = ui.painter_at(viewport_rect);

        // Background. Neutral dark grey — palette matching is the
        // caller's concern.
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
                let y = viewport_rect.top()
                    + ((view_start - tick) as f64 / ns_per_px) as f32;
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

        // Per-source event rendering.
        //
        // Two layouts depending on source count:
        //   1 source, all Commits → v1 ribbon-on-right style (short
        //      horizontal tick + dot on a thin vertical axis line).
        //   Otherwise → chip-style rows: wide chip with source pill +
        //      per-kind decoration + summary text.
        let only_commits = sources.len() == 1
            && matches!(sources[0], TimelineSource::Commits { .. });

        let pointer_pos = ui.input(|i| i.pointer.hover_pos());
        let mut hover_label: Option<(egui::Pos2, String)> = None;
        let mut clicked_event: Option<(SourceKind, Id)> = None;
        let mut hover_rect: Option<(egui::Rect, egui::Color32)> = None;

        if only_commits {
            // v1 compact ribbon.
            let commit_color = sources[0].color();
            let axis_color = egui::Color32::from_rgb(0xbd, 0xbd, 0xbd);
            let axis_x = viewport_rect.right() - 40.0;
            painter.line_segment(
                [
                    egui::pos2(axis_x, viewport_rect.top()),
                    egui::pos2(axis_x, viewport_rect.bottom()),
                ],
                egui::Stroke::new(0.5, axis_color),
            );

            for ev in events {
                if ev.ts_ns < view_end || ev.ts_ns > view_start {
                    continue;
                }
                let y =
                    viewport_rect.top() + ((view_start - ev.ts_ns) as f64 / ns_per_px) as f32;
                let x1 = axis_x - 8.0;
                let x2 = axis_x + 8.0;
                painter.line_segment(
                    [egui::pos2(x1, y), egui::pos2(x2, y)],
                    egui::Stroke::new(1.5, commit_color),
                );
                painter.circle_filled(egui::pos2(axis_x, y), 2.5, commit_color);

                if let Some(p) = pointer_pos {
                    if viewport_rect.contains(p)
                        && (p.y - y).abs() <= 4.0
                        && (p.x - axis_x).abs() <= 40.0
                    {
                        let label =
                            format!("{}  {}", ev.summary, format_time_marker(ev.ts_ns));
                        hover_label = Some((egui::pos2(axis_x - 12.0, y), label));
                        if viewport_response.clicked() {
                            clicked_event = Some((ev.kind, ev.entity_id));
                        }
                    }
                }
            }

            if let Some((pos, label)) = hover_label {
                painter.text(
                    pos,
                    egui::Align2::RIGHT_CENTER,
                    label,
                    egui::FontId::monospace(10.0),
                    axis_color,
                );
            }
        } else {
            // Multi-source chip layout.
            let event_left = viewport_rect.left() + max_len + 110.0;
            let event_right_margin = 8.0;
            let event_width = (viewport_rect.right() - event_left - event_right_margin).max(80.0);
            let chip_h = 16.0;
            let text_color = egui::Color32::from_rgb(0xe6, 0xe6, 0xe6);

            for ev in events {
                if ev.ts_ns < view_end || ev.ts_ns > view_start {
                    continue;
                }
                let y =
                    viewport_rect.top() + ((view_start - ev.ts_ns) as f64 / ns_per_px) as f32;
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
                let src_pill_w = 42.0;
                let src_pill = egui::Rect::from_min_size(
                    egui::pos2(event_left + 2.0, y - chip_h * 0.5 + 1.0),
                    egui::vec2(src_pill_w, chip_h - 2.0),
                );
                painter.rect_filled(src_pill, 3.0, src_color);
                painter.text(
                    src_pill.center(),
                    egui::Align2::CENTER_CENTER,
                    &src_label,
                    egui::FontId::proportional(9.0),
                    text_on(src_color),
                );

                // Optional secondary pill (status / from→to).
                let mut text_x = event_left + src_pill_w + 6.0;
                if let Some(status) = &ev.status {
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
                        status,
                        egui::FontId::proportional(9.0),
                        text_on(pill_color),
                    );
                    text_x = pill.right() + 6.0;
                }
                if let Some(fromto) = &ev.from_to {
                    painter.text(
                        egui::pos2(text_x, y),
                        egui::Align2::LEFT_CENTER,
                        fromto,
                        egui::FontId::monospace(9.0),
                        muted,
                    );
                    text_x += (fromto.len() as f32 * 6.5).min(140.0);
                }

                // Summary text — clipped against the chip's right edge.
                let text_rect = egui::Rect::from_min_max(
                    egui::pos2(text_x, chip_rect.top()),
                    egui::pos2(chip_rect.right() - 4.0, chip_rect.bottom()),
                );
                let text_painter = painter.with_clip_rect(text_rect);
                text_painter.text(
                    egui::pos2(text_x, y),
                    egui::Align2::LEFT_CENTER,
                    &ev.summary,
                    egui::FontId::monospace(10.0),
                    text_color,
                );

                // Interaction: hover highlight + click.
                if let Some(p) = pointer_pos {
                    if chip_rect.contains(p) {
                        hover_rect = Some((chip_rect, src_color));
                        if viewport_response.clicked() {
                            clicked_event = Some((ev.kind, ev.entity_id));
                        }
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
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
        }

        if let Some(sel) = clicked_event {
            self.selected_event = Some(sel);
        }
    }
}

/// Draw a small rounded pill label. Used by the source legend above the
/// viewport.
fn render_pill(ui: &mut egui::Ui, label: &str, fill: egui::Color32) {
    let text = text_on(fill);
    egui::Frame::NONE
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(label).small().monospace().color(text));
        });
}
