//! Minimal GORBIE-embeddable compass (kanban) board viewer.
//!
//! Renders goals from a triblespace pile's `compass` branch grouped into
//! kanban columns by their latest status (default columns: todo / doing /
//! blocked / done). Each goal card shows its title, a status chip, a short
//! id, and tag chips. Clicking a card expands it to show its notes.
//!
//! Scope is intentionally tight: v1 is read-only. It does not support
//! drag-to-move-status, inline editing, priority (`higher`/`lower`) edge
//! rendering, parent/child tree collapsing, or goal composition.
//!
//! ```ignore
//! let mut board = CompassBoard::new_default("./self.pile");
//! // Inside a GORBIE card:
//! board.render(ctx);
//! ```

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use GORBIE::prelude::CardCtx;
use triblespace::core::id::Id;
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{BlobStore, BlobStoreGet, BranchStore, Repository, Workspace};
use triblespace::core::trible::TribleSet;
use triblespace::core::value::schemas::hash::{Blake3, Handle};
use triblespace::core::value::Value;
use triblespace::macros::{find, pattern};
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::View;

use crate::schemas::compass::{
    board as compass, DEFAULT_STATUSES, KIND_GOAL_ID, KIND_NOTE_ID, KIND_STATUS_ID,
};

/// Default branch name the compass faculty writes to.
pub const COMPASS_BRANCH_NAME: &str = "compass";

/// Handle to a long-string blob (titles, notes).
type TextHandle = Value<Handle<Blake3, LongString>>;

/// Format an Id as a lowercase hex string.
fn fmt_id_full(id: Id) -> String {
    format!("{id:x}")
}

/// First 8 hex chars of an Id — compact label for cards.
fn id_prefix(id: Id) -> String {
    let s = fmt_id_full(id);
    if s.len() > 8 {
        s[..8].to_string()
    } else {
        s
    }
}

// ── Color palette (RAL-inspired, matches playground diagnostics) ────

fn color_todo() -> egui::Color32 {
    // RAL 6018 yellow green
    egui::Color32::from_rgb(0x57, 0xa6, 0x39)
}
fn color_doing() -> egui::Color32 {
    // RAL 1003 signal yellow
    egui::Color32::from_rgb(0xf7, 0xba, 0x0b)
}
fn color_blocked() -> egui::Color32 {
    // RAL 3020 traffic red
    egui::Color32::from_rgb(0xcc, 0x0a, 0x17)
}
fn color_done() -> egui::Color32 {
    // RAL 5005 signal blue
    egui::Color32::from_rgb(0x15, 0x4e, 0xa1)
}
fn color_muted() -> egui::Color32 {
    // RAL 7012 basalt grey
    egui::Color32::from_rgb(0x4d, 0x55, 0x59)
}
fn color_frame() -> egui::Color32 {
    // RAL 7016 anthracite grey
    egui::Color32::from_rgb(0x29, 0x32, 0x36)
}
fn color_tag() -> egui::Color32 {
    // Neutral tag chip fill — caller can't introspect content hash, so
    // a single muted fill keeps v1 simple.
    egui::Color32::from_rgb(0x4a, 0x56, 0x5c)
}

fn status_color(status: &str) -> egui::Color32 {
    match status {
        "todo" => color_todo(),
        "doing" => color_doing(),
        "blocked" => color_blocked(),
        "done" => color_done(),
        _ => color_muted(),
    }
}

/// Pick black or white text color for good contrast on `fill`.
fn text_on(fill: egui::Color32) -> egui::Color32 {
    // WCAG relative luminance (approx).
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

// ── Row structs ────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct GoalRow {
    id: Id,
    id_prefix: String,
    title: String,
    tags: Vec<String>,
    status: String,
    /// TAI ns of the latest status assignment (sort key within a column).
    status_at: Option<i128>,
    /// TAI ns of the goal's own creation (fallback sort key).
    created_at: Option<i128>,
    note_count: usize,
}

impl GoalRow {
    fn sort_key(&self) -> i128 {
        self.status_at
            .or(self.created_at)
            .unwrap_or(i128::MIN)
    }
}

#[derive(Clone, Debug)]
struct NoteRow {
    at: Option<i128>,
    body: String,
}

// ── Live compass connection ──────────────────────────────────────────

/// Opened pile + cached compass fact space + workspace for blob reads.
struct CompassLive {
    space: TribleSet,
    ws: Workspace<Pile<Blake3>>,
}

impl CompassLive {
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
        let mut ws = repo
            .pull(bid)
            .map_err(|e| format!("pull {branch_name}: {e:?}"))?;
        let space = ws
            .checkout(..)
            .map_err(|e| format!("checkout {branch_name}: {e:?}"))?
            .into_facts();

        Ok(CompassLive { space, ws })
    }

    fn text(&mut self, h: TextHandle) -> String {
        self.ws
            .get::<View<str>, LongString>(h)
            .map(|v| {
                let s: &str = v.as_ref();
                s.to_string()
            })
            .unwrap_or_default()
    }

    /// Collect every goal with derived current status, tags, note count.
    fn goals(&mut self) -> Vec<GoalRow> {
        // Title + created_at per goal entity.
        let mut by_id: HashMap<Id, GoalRow> = HashMap::new();

        let title_rows: Vec<(Id, TextHandle, (i128, i128))> = find!(
            (gid: Id, title: TextHandle, ts: (i128, i128)),
            pattern!(&self.space, [{
                ?gid @
                metadata::tag: &KIND_GOAL_ID,
                compass::title: ?title,
                metadata::created_at: ?ts,
            }])
        )
        .collect();

        for (gid, title_handle, ts) in title_rows {
            if by_id.contains_key(&gid) {
                continue;
            }
            let title = self.text(title_handle);
            by_id.insert(
                gid,
                GoalRow {
                    id: gid,
                    id_prefix: id_prefix(gid),
                    title,
                    tags: Vec::new(),
                    status: "todo".to_string(),
                    status_at: None,
                    created_at: Some(ts.0),
                    note_count: 0,
                },
            );
        }

        // Tags — compass::tag is a ShortString on the goal entity. Goals
        // without tags simply don't match.
        for (gid, tag) in find!(
            (gid: Id, tag: String),
            pattern!(&self.space, [{
                ?gid @
                metadata::tag: &KIND_GOAL_ID,
                compass::tag: ?tag,
            }])
        ) {
            if let Some(row) = by_id.get_mut(&gid) {
                row.tags.push(tag);
            }
        }

        // Latest status per goal. Status events are separate entities
        // tagged KIND_STATUS_ID, linked to the goal via compass::task, and
        // carry a metadata::created_at timestamp. We pick the event with
        // the largest timestamp per task.
        for (gid, status, ts) in find!(
            (gid: Id, status: String, ts: (i128, i128)),
            pattern!(&self.space, [{
                _?event @
                metadata::tag: &KIND_STATUS_ID,
                compass::task: ?gid,
                compass::status: ?status,
                metadata::created_at: ?ts,
            }])
        ) {
            if let Some(row) = by_id.get_mut(&gid) {
                let replace = match row.status_at {
                    None => true,
                    Some(prev) => ts.0 > prev,
                };
                if replace {
                    row.status = status;
                    row.status_at = Some(ts.0);
                }
            }
        }

        // Note counts — every note event carries compass::task pointing
        // at its goal.
        for gid in find!(
            gid: Id,
            pattern!(&self.space, [{
                _?event @
                metadata::tag: &KIND_NOTE_ID,
                compass::task: ?gid,
            }])
        ) {
            if let Some(row) = by_id.get_mut(&gid) {
                row.note_count += 1;
            }
        }

        for row in by_id.values_mut() {
            row.tags.sort();
            row.tags.dedup();
        }

        by_id.into_values().collect()
    }

    /// Notes on a specific goal, sorted newest-first.
    fn notes_for(&mut self, goal_id: Id) -> Vec<NoteRow> {
        let raw: Vec<(TextHandle, (i128, i128))> = find!(
            (note_handle: TextHandle, ts: (i128, i128)),
            pattern!(&self.space, [{
                _?event @
                metadata::tag: &KIND_NOTE_ID,
                compass::task: &goal_id,
                compass::note: ?note_handle,
                metadata::created_at: ?ts,
            }])
        )
        .collect();

        let mut notes: Vec<NoteRow> = raw
            .into_iter()
            .map(|(h, ts)| NoteRow {
                at: Some(ts.0),
                body: self.text(h),
            })
            .collect();
        notes.sort_by(|a, b| b.at.cmp(&a.at));
        notes
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

/// GORBIE-embeddable kanban-style compass board viewer.
///
/// Groups goals from a pile's compass branch into columns by their latest
/// status. Default columns are `todo / doing / blocked / done`, plus one
/// extra column per custom status value encountered.
///
/// ```ignore
/// let mut board = CompassBoard::new_default("./self.pile");
/// // Inside a GORBIE card:
/// board.render(ctx);
/// ```
pub struct CompassBoard {
    pile_path: PathBuf,
    branch_name: String,
    // Wrapped in a Mutex so the widget is `Send + Sync` — GORBIE's state
    // storage requires that across threads, and `Workspace<Pile<Blake3>>`
    // uses interior-mutability types (Cell/RefCell) that aren't Sync.
    live: Option<Mutex<CompassLive>>,
    error: Option<String>,
    expanded_goal: Option<Id>,
    column_height: f32,
}

impl CompassBoard {
    /// Build a board pointing at a pile on disk and a named branch.
    /// The pile is not opened until the first [`render`](Self::render)
    /// call.
    pub fn new(pile_path: impl Into<PathBuf>, branch_name: impl Into<String>) -> Self {
        Self {
            pile_path: pile_path.into(),
            branch_name: branch_name.into(),
            live: None,
            error: None,
            expanded_goal: None,
            column_height: 500.0,
        }
    }

    /// Build a board pointing at the conventional `compass` branch.
    pub fn new_default(pile_path: impl Into<PathBuf>) -> Self {
        Self::new(pile_path, COMPASS_BRANCH_NAME)
    }

    /// Override the per-column scroll-area height (pixels). Default 500.
    pub fn with_column_height(mut self, height: f32) -> Self {
        self.column_height = height.max(120.0);
        self
    }

    /// Render the board into a GORBIE card context.
    pub fn render(&mut self, ctx: &mut CardCtx<'_>) {
        // Lazy pile open on first render.
        if self.live.is_none() && self.error.is_none() {
            match CompassLive::open(&self.pile_path, &self.branch_name) {
                Ok(live) => self.live = Some(Mutex::new(live)),
                Err(e) => self.error = Some(e),
            }
        }

        if let Some(err) = &self.error {
            ctx.label(format!("compass board error: {err}"));
            return;
        }

        let Some(live_lock) = self.live.as_ref() else {
            ctx.label("compass board not initialized");
            return;
        };
        let mut live = live_lock.lock();

        // Pre-materialize everything the UI closures need so we don't
        // have to juggle dual mutable borrows of `self` / the live conn.
        let mut goals = live.goals();
        // Newest first within each column.
        goals.sort_by(|a, b| {
            b.sort_key()
                .cmp(&a.sort_key())
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                .then_with(|| a.id.cmp(&b.id))
        });

        // Bucket goals per status — preserving the global sort above.
        let mut by_status: BTreeMap<String, Vec<GoalRow>> = BTreeMap::new();
        for g in goals {
            by_status
                .entry(g.status.clone())
                .or_default()
                .push(g);
        }

        // Column order: defaults first, then any custom statuses sorted.
        let mut columns: Vec<String> = DEFAULT_STATUSES.iter().map(|s| s.to_string()).collect();
        let mut extras: Vec<String> = by_status
            .keys()
            .filter(|s| !DEFAULT_STATUSES.contains(&s.as_str()))
            .cloned()
            .collect();
        extras.sort();
        columns.extend(extras);

        // Per-column counts (including empty columns).
        let column_data: Vec<(String, Vec<GoalRow>)> = columns
            .into_iter()
            .map(|s| {
                let rows = by_status.remove(&s).unwrap_or_default();
                (s, rows)
            })
            .collect();

        // Resolve expanded goal's notes (if any) while we still hold
        // `live`.
        let expanded = self.expanded_goal;
        let expanded_notes: Option<(Id, Vec<NoteRow>)> = expanded.map(|gid| {
            let notes = live.notes_for(gid);
            (gid, notes)
        });

        drop(live);

        let expanded_goal = &mut self.expanded_goal;
        let column_height = self.column_height;
        let branch_name = self.branch_name.clone();

        ctx.section(&format!("Board: {branch_name}"), |ctx| {
            let total_goals: usize = column_data.iter().map(|(_, r)| r.len()).sum();
            ctx.label(format!("{total_goals} goals"));

            let ui = ctx.ui_mut();
            if total_goals == 0 {
                ui.label("No goals yet.");
                return;
            }

            // Horizontal row of columns. Each column is a vertical scroll
            // area of goal cards.
            ui.horizontal_top(|ui| {
                let available = ui.available_width();
                let col_count = column_data.len().max(1);
                let col_spacing = 8.0;
                let col_width = ((available - col_spacing * (col_count as f32 - 1.0))
                    / col_count as f32)
                    .max(180.0);

                for (status, rows) in &column_data {
                    render_column(
                        ui,
                        status,
                        rows,
                        col_width,
                        column_height,
                        expanded_goal,
                        expanded_notes.as_ref(),
                    );
                }
            });
        });
    }
}

// ── Column / card rendering ───────────────────────────────────────────

fn render_column(
    ui: &mut egui::Ui,
    status: &str,
    rows: &[GoalRow],
    width: f32,
    height: f32,
    expanded_goal: &mut Option<Id>,
    expanded_notes: Option<&(Id, Vec<NoteRow>)>,
) {
    let status_col = status_color(status);
    egui::Frame::NONE
        .fill(color_frame())
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(width);
            ui.set_min_height(height);

            // Column header.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("{} ({})", status.to_uppercase(), rows.len()))
                        .monospace()
                        .strong()
                        .color(status_col),
                );
            });
            ui.add_space(6.0);

            egui::ScrollArea::vertical()
                .id_salt(("compass_column", status))
                .max_height(height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if rows.is_empty() {
                        ui.small("(empty)");
                        return;
                    }
                    for row in rows {
                        render_goal_card(ui, row, expanded_goal, expanded_notes);
                        ui.add_space(6.0);
                    }
                });
        });
}

fn render_goal_card(
    ui: &mut egui::Ui,
    row: &GoalRow,
    expanded_goal: &mut Option<Id>,
    expanded_notes: Option<&(Id, Vec<NoteRow>)>,
) {
    let card_bg = egui::Color32::from_rgb(0x33, 0x3b, 0x40);
    let is_expanded = *expanded_goal == Some(row.id);

    let card_response = egui::Frame::NONE
        .fill(card_bg)
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // Title + short id.
            ui.horizontal(|ui| {
                render_chip(ui, &row.status, status_color(&row.status));
                ui.add(
                    egui::Label::new(egui::RichText::new(&row.title).monospace())
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&row.id_prefix)
                        .monospace()
                        .small()
                        .color(color_muted()),
                );
                if row.note_count > 0 {
                    render_chip(ui, &format!("{}n", row.note_count), color_muted());
                }
            });

            if !row.tags.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    for tag in &row.tags {
                        render_chip(ui, &format!("#{tag}"), color_tag());
                    }
                });
            }
        })
        .response;

    // Whole card is clickable to toggle expansion.
    let click_id = ui.make_persistent_id(("compass_goal", row.id));
    let response = ui.interact(card_response.rect, click_id, egui::Sense::click());
    if response.clicked() {
        if *expanded_goal == Some(row.id) {
            *expanded_goal = None;
        } else {
            *expanded_goal = Some(row.id);
        }
    }

    if is_expanded {
        let notes: &[NoteRow] = expanded_notes
            .filter(|(gid, _)| *gid == row.id)
            .map(|(_, n)| n.as_slice())
            .unwrap_or(&[]);
        egui::Frame::NONE
            .stroke(egui::Stroke::new(1.0, color_muted()))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                if notes.is_empty() {
                    ui.small("(no notes)");
                    return;
                }
                let now = now_tai_ns();
                for note in notes {
                    ui.label(
                        egui::RichText::new(format_age(now, note.at))
                            .small()
                            .color(color_muted()),
                    );
                    ui.add(
                        egui::Label::new(egui::RichText::new(&note.body))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                    ui.add_space(4.0);
                }
            });
    }
}

fn render_chip(ui: &mut egui::Ui, label: &str, fill: egui::Color32) {
    let text = text_on(fill);
    egui::Frame::NONE
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(label).small().color(text));
        });
}

// ── Time helpers ─────────────────────────────────────────────────────

fn now_tai_ns() -> i128 {
    hifitime::Epoch::now()
        .map(|e| e.to_tai_duration().total_nanoseconds())
        .unwrap_or(0)
}

fn format_age(now_key: i128, maybe_key: Option<i128>) -> String {
    let Some(key) = maybe_key else {
        return "-".to_string();
    };
    let delta_ns = now_key.saturating_sub(key);
    let delta_s = (delta_ns / 1_000_000_000).max(0) as i64;
    if delta_s < 60 {
        format!("{delta_s}s")
    } else if delta_s < 60 * 60 {
        format!("{}m", delta_s / 60)
    } else if delta_s < 24 * 60 * 60 {
        format!("{}h", delta_s / 3600)
    } else {
        format!("{}d", delta_s / 86_400)
    }
}
