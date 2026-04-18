//! Minimal GORBIE-embeddable branch timeline.
//!
//! Shows a pan/zoom time axis for a single branch of a triblespace pile.
//! Commits on the branch are enumerated (following ancestors from HEAD)
//! and painted as small horizontal ticks at their `metadata::created_at`
//! position on the time axis.
//!
//! Scope is intentionally tight: v1 renders commit dots only. It does
//! not decorate with event-type-specific overlays, filter, or support
//! click-to-select. Progressive history loading is not yet implemented —
//! the full branch is walked on open.
//!
//! Input handling mirrors the playground dashboard timeline:
//! * scroll = pan (vertical)
//! * ctrl+scroll or horizontal scroll = zoom
//! * drag = pan
//! * double-click = jump to "now"
//!
//! ```ignore
//! let mut timeline = BranchTimeline::new("./self.pile", "wiki");
//! // Inside a GORBIE card:
//! timeline.render(ctx);
//! ```
//!
//! The ruler is a "four-sine" design: overlapping cosines at the natural
//! time periods (minute, hour, day) that produce constructive interference
//! at nice times. Labels are placed independently at the coarsest interval
//! that gives ~6-10 labels per viewport.

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

/// Handle to a long-string blob (for branch names).
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

// ── Live branch connection ───────────────────────────────────────────

/// Opened pile + workspace for the target branch, plus the enumerated
/// list of commits on that branch.
struct TimelineLive {
    ws: Workspace<Pile<Blake3>>,
    /// Cached commits. Each entry is `(commit_entity_id, created_at_ns)`.
    /// Sorted by timestamp ascending. Lifted eagerly on open — good
    /// enough for small branches; progressive loading is future work.
    commits: Vec<CommitEntry>,
}

#[derive(Clone, Debug)]
struct CommitEntry {
    /// The commit entity id (derived from its signed metadata).
    commit_id: Id,
    /// TAI nanoseconds (lower bound of the interval).
    ts_ns: i128,
}

impl TimelineLive {
    fn open(path: &Path, branch_name: &str) -> Result<Self, String> {
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

        let bid = find_branch(&mut repo, branch_name)
            .ok_or_else(|| format!("no '{branch_name}' branch found"))?;
        let mut ws = repo.pull(bid).map_err(|e| format!("pull: {e:?}"))?;

        let commits = enumerate_commits(&mut ws)?;

        Ok(TimelineLive { ws, commits })
    }

    /// Re-enumerate commits from HEAD. Cheap enough for v1; a future
    /// version should do incremental delta via `checkout(prev..)`.
    fn refresh(&mut self) {
        if let Ok(cs) = enumerate_commits(&mut self.ws) {
            self.commits = cs;
        }
    }
}

/// Follow ancestors from `ws.head()` and read each commit blob to extract
/// `(entity_id, created_at_ns)`. Commits without `created_at` (e.g. merge
/// commits) are skipped — they carry no author-time bits by design.
fn enumerate_commits(
    ws: &mut Workspace<Pile<Blake3>>,
) -> Result<Vec<CommitEntry>, String> {
    let Some(head) = ws.head() else {
        return Ok(Vec::new());
    };

    // `ancestors(head)` walks every commit reachable from HEAD through
    // parent links. Selecting it gives us the full CommitSet.
    let set: CommitSet = ancestors(head)
        .select(ws)
        .map_err(|e| format!("ancestors select: {e:?}"))?;

    let mut out: Vec<CommitEntry> = Vec::new();
    for raw in set.iter() {
        let handle: CommitHandleValue = Value::new(*raw);
        let Ok(meta) = ws.get::<TribleSet, SimpleArchive>(handle) else {
            continue;
        };
        // Each commit blob is a TribleSet containing the commit entity.
        // The entity id is derived intrinsically from the metadata, and
        // `metadata::created_at` is present on authored commits only.
        if let Some((cid, ts)) = find!(
            (cid: Id, ts: (i128, i128)),
            pattern!(&meta, [{ ?cid @ metadata::created_at: ?ts }])
        )
        .next()
        {
            out.push(CommitEntry {
                commit_id: cid,
                ts_ns: ts.0,
            });
        }
    }
    out.sort_by_key(|c| c.ts_ns);
    Ok(out)
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

/// GORBIE-embeddable pan/zoom timeline for a single pile branch.
///
/// Paints a full-width vertical time axis (newest at top, oldest at
/// bottom) with:
///
/// * a four-sine ruler (constructive interference at minute / hour /
///   day boundaries)
/// * time labels at the coarsest interval that fits
/// * one horizontal tick per commit at its `metadata::created_at`
///   position
///
/// The pile + target branch are opened lazily on first render, in the
/// same pattern as [`WikiViewer`](crate::widgets::WikiViewer).
pub struct BranchTimeline {
    pile_path: PathBuf,
    branch_name: String,
    viewport_height: f32,
    // Wrapped in a Mutex so the widget is `Send + Sync` — GORBIE's state
    // storage requires that across threads, and `Workspace<Pile<Blake3>>`
    // uses interior-mutability types (Cell/RefCell) that aren't Sync.
    live: Option<Mutex<TimelineLive>>,
    error: Option<String>,
    /// Top edge of viewport, in TAI ns. Newest visible time.
    timeline_start: i128,
    /// Pixels per minute of wall time.
    timeline_scale: f32,
    /// Tracks the first render so we can initialize `timeline_start` to
    /// "now" before painting.
    first_render: bool,
}

impl BranchTimeline {
    /// Build a timeline pointing at a pile on disk and a named branch.
    /// The pile is not opened until the first [`render`](Self::render)
    /// call.
    pub fn new(pile_path: impl Into<PathBuf>, branch_name: impl Into<String>) -> Self {
        Self {
            pile_path: pile_path.into(),
            branch_name: branch_name.into(),
            viewport_height: DEFAULT_VIEWPORT_HEIGHT,
            live: None,
            error: None,
            timeline_start: 0,
            timeline_scale: TIMELINE_DEFAULT_SCALE,
            first_render: true,
        }
    }

    /// Override the viewport height (pixels). Defaults to 800.
    pub fn with_height(mut self, height: f32) -> Self {
        self.viewport_height = height.max(48.0);
        self
    }

    /// Render the timeline into a GORBIE card context.
    pub fn render(&mut self, ctx: &mut CardCtx<'_>) {
        // Lazy pile open on first render.
        if self.live.is_none() && self.error.is_none() {
            match TimelineLive::open(&self.pile_path, &self.branch_name) {
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
            // Opportunistically refresh on first render in case commits
            // were added between construction and first paint.
            live_lock.lock().refresh();
        }

        // Snapshot commits so we don't hold the mutex across egui calls.
        let commits: Vec<CommitEntry> = live_lock.lock().commits.clone();

        let branch_name = self.branch_name.clone();
        let viewport_height = self.viewport_height;

        ctx.section(&format!("Branch: {branch_name}"), |ctx| {
            ctx.label(format!("{} commits", commits.len()));
            self.paint_viewport(ctx, viewport_height, now, &commits);
        });
    }

    /// Paint the timeline viewport. All pan/zoom/scroll logic lives here.
    fn paint_viewport(
        &mut self,
        ctx: &mut CardCtx<'_>,
        viewport_height: f32,
        now: i128,
        commits: &[CommitEntry],
    ) {
        let ui = ctx.ui_mut();
        let scroll_speed = 3.0;
        let viewport_width = ui.available_width();
        let (viewport_rect, viewport_response) = ui.allocate_exact_size(
            egui::vec2(viewport_width, viewport_height),
            egui::Sense::click_and_drag(),
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

            if viewport_response.dragged() {
                let drag_delta = viewport_response.drag_delta().y;
                let pan_ns = (drag_delta as f64 * ns_per_px) as i128;
                self.timeline_start += pan_ns;
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

        // Background. Use a neutral dark grey — palette matching is the
        // caller's concern.
        painter.rect_filled(
            viewport_rect,
            0.0,
            egui::Color32::from_rgb(0x29, 0x2c, 0x2f),
        );

        // Four-sine ruler: one cosine per natural time period. Periods
        // whose wavelength is < 2× tick spacing fade out smoothly.
        let muted = egui::Color32::from_rgb(0x8a, 0x8a, 0x8a);
        let axis_color = egui::Color32::from_rgb(0xbd, 0xbd, 0xbd);
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

        // Commit markers: short horizontal tick per commit, plus a small
        // filled circle on the axis. Tooltip on hover shows id + time.
        let commit_color = egui::Color32::from_rgb(0xff, 0xc8, 0x3a);
        let axis_x = viewport_rect.right() - 40.0;
        // A faint vertical axis line on the right side anchors the commits.
        painter.line_segment(
            [
                egui::pos2(axis_x, viewport_rect.top()),
                egui::pos2(axis_x, viewport_rect.bottom()),
            ],
            egui::Stroke::new(0.5, axis_color),
        );

        let pointer_pos = ui.input(|i| i.pointer.hover_pos());
        let mut hover_label: Option<(egui::Pos2, String)> = None;

        for c in commits {
            if c.ts_ns < view_end || c.ts_ns > view_start {
                continue;
            }
            let y = viewport_rect.top() + ((view_start - c.ts_ns) as f64 / ns_per_px) as f32;
            let x1 = axis_x - 8.0;
            let x2 = axis_x + 8.0;
            painter.line_segment(
                [egui::pos2(x1, y), egui::pos2(x2, y)],
                egui::Stroke::new(1.5, commit_color),
            );
            painter.circle_filled(egui::pos2(axis_x, y), 2.5, commit_color);

            if let Some(p) = pointer_pos {
                if viewport_rect.contains(p) && (p.y - y).abs() <= 4.0 && (p.x - axis_x).abs() <= 40.0
                {
                    let short = format!("{:x}", c.commit_id);
                    let short = if short.len() > 8 {
                        short[..8].to_string()
                    } else {
                        short
                    };
                    let label = format!("{}  {}", short, format_time_marker(c.ts_ns));
                    hover_label = Some((egui::pos2(axis_x - 12.0, y), label));
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
    }
}
