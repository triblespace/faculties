//! Full-featured GORBIE-embeddable compass (kanban) board widget.
//!
//! Renders goals from a triblespace pile's `compass` branch grouped into
//! kanban columns by their latest status (default: todo / doing / blocked
//! / done). Beyond read-only display, the widget supports:
//!
//! - Composing new goals (title, tags, optional parent, initial status)
//! - Moving a goal to a new status (click a goal card → pick a status)
//! - Adding notes to an expanded goal
//! - Parent/child indentation with a collapse toggle per subtree
//! - Priority arrows: `board::higher` / `board::lower` edges rendered as
//!   `> over <id_prefix>` badges on the card
//! - Tag chips colored via `GORBIE::themes::colorhash::ral_categorical`,
//!   so the same tag string always gets the same hue.
//!
//! Writes produce the same fact shapes as the `compass.rs` faculty CLI —
//! new goal/status/note/prioritize entities tagged with the appropriate
//! `KIND_*_ID`. Commits go through `Repository::pull → Workspace::commit
//! → Repository::try_push`; on CAS conflict a side toast is shown and
//! the user can retry (the next render will have re-pulled).
//!
//! ```ignore
//! let mut board = CompassBoard::new("./self.pile", "compass");
//! // Inside a GORBIE card:
//! board.render(ctx);
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use GORBIE::prelude::CardCtx;
use GORBIE::themes::colorhash;
use triblespace::core::id::{ufoid, ExclusiveId, Id};
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{BlobStore, BlobStoreGet, BranchStore, Repository, Workspace};
use triblespace::core::trible::TribleSet;
use triblespace::core::value::schemas::hash::{Blake3, Handle};
use triblespace::core::value::{TryToValue, Value};
use triblespace::macros::{entity, find, pattern};
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::valueschemas::NsTAIInterval;
use triblespace::prelude::View;

use crate::schemas::compass::{
    board as compass, DEFAULT_STATUSES, KIND_GOAL_ID, KIND_NOTE_ID, KIND_PRIORITIZE_ID,
    KIND_STATUS_ID,
};

/// Default branch name the compass faculty writes to.
pub const COMPASS_BRANCH_NAME: &str = "compass";

/// Handle to a long-string blob (titles, notes).
type TextHandle = Value<Handle<Blake3, LongString>>;
/// Interval value (TAI ns lower/upper) used for `metadata::created_at`.
type IntervalValue = Value<NsTAIInterval>;

// ── ID / time helpers ────────────────────────────────────────────────

fn fmt_id_full(id: Id) -> String {
    format!("{id:x}")
}

fn id_prefix(id: Id) -> String {
    let s = fmt_id_full(id);
    if s.len() > 8 {
        s[..8].to_string()
    } else {
        s
    }
}

fn now_tai_ns() -> i128 {
    hifitime::Epoch::now()
        .map(|e| e.to_tai_duration().total_nanoseconds())
        .unwrap_or(0)
}

fn now_epoch() -> hifitime::Epoch {
    hifitime::Epoch::now().unwrap_or_else(|_| hifitime::Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0))
}

fn epoch_interval(epoch: hifitime::Epoch) -> IntervalValue {
    (epoch, epoch).try_to_value().unwrap()
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

// ── Color palette (RAL-inspired, matches playground diagnostics) ────

fn color_todo() -> egui::Color32 {
    egui::Color32::from_rgb(0x57, 0xa6, 0x39) // RAL 6018
}
fn color_doing() -> egui::Color32 {
    egui::Color32::from_rgb(0xf7, 0xba, 0x0b) // RAL 1003
}
fn color_blocked() -> egui::Color32 {
    egui::Color32::from_rgb(0xcc, 0x0a, 0x17) // RAL 3020
}
fn color_done() -> egui::Color32 {
    egui::Color32::from_rgb(0x15, 0x4e, 0xa1) // RAL 5005
}
fn color_muted() -> egui::Color32 {
    egui::Color32::from_rgb(0x4d, 0x55, 0x59) // RAL 7012
}
fn color_frame() -> egui::Color32 {
    egui::Color32::from_rgb(0x29, 0x32, 0x36) // RAL 7016
}
fn card_bg() -> egui::Color32 {
    egui::Color32::from_rgb(0x33, 0x3b, 0x40)
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

/// Deterministic color for a tag string via GORBIE's colorhash palette.
fn tag_color(tag: &str) -> egui::Color32 {
    colorhash::ral_categorical(tag.as_bytes())
}

// ── Row structs ──────────────────────────────────────────────────────

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
    parent: Option<Id>,
    /// Goals this one is prioritized over (`board::higher=self, board::lower=x`).
    higher_over: Vec<Id>,
}

impl GoalRow {
    fn sort_key(&self) -> i128 {
        self.status_at.or(self.created_at).unwrap_or(i128::MIN)
    }
}

#[derive(Clone, Debug)]
struct NoteRow {
    at: Option<i128>,
    body: String,
}

// ── Live compass connection (read + write) ───────────────────────────

/// Owns the open pile, repository handle, and the active workspace for
/// the compass branch. Queries run against the cached `space`; writes
/// build a change tribleset, call `ws.commit(..)`, and push.
struct CompassLive {
    branch_name: String,
    branch_id: Id,
    space: TribleSet,
    repo: Repository<Pile<Blake3>>,
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

        let branch_id = find_branch(&mut repo, branch_name)
            .ok_or_else(|| format!("no '{branch_name}' branch found"))?;
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| format!("pull {branch_name}: {e:?}"))?;
        let space = ws
            .checkout(..)
            .map_err(|e| format!("checkout {branch_name}: {e:?}"))?
            .into_facts();

        Ok(CompassLive {
            branch_name: branch_name.to_string(),
            branch_id,
            space,
            repo,
            ws,
        })
    }

    /// Refresh the cached fact space after a successful commit+push. We
    /// re-pull the branch (cheap: just re-fetches head metadata and
    /// rebuilds the workspace) and re-checkout to pick up the new commit.
    fn refresh(&mut self) -> Result<(), String> {
        self.repo
            .storage_mut()
            .refresh()
            .map_err(|e| format!("refresh: {e:?}"))?;
        let mut ws = self
            .repo
            .pull(self.branch_id)
            .map_err(|e| format!("pull {}: {e:?}", self.branch_name))?;
        let space = ws
            .checkout(..)
            .map_err(|e| format!("checkout {}: {e:?}", self.branch_name))?
            .into_facts();
        self.ws = ws;
        self.space = space;
        Ok(())
    }

    /// Commit a change and push. On push CAS conflict this returns an
    /// error string — the caller is expected to surface it as a toast.
    fn commit_and_push(&mut self, change: TribleSet, message: &str) -> Result<(), String> {
        self.ws.commit(change, message);
        match self.repo.try_push(&mut self.ws) {
            Ok(None) => self.refresh(),
            Ok(Some(_conflict_ws)) => {
                // CAS conflict. Drop our local workspace state by re-pulling,
                // and surface the conflict so the user knows to retry.
                let _ = self.refresh();
                Err("branch advanced concurrently — please retry".to_string())
            }
            Err(e) => {
                let _ = self.refresh();
                Err(format!("push: {e:?}"))
            }
        }
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

    /// Collect every goal with derived current status, tags, note count,
    /// parent, and outgoing priority edges (higher_over).
    fn goals(&mut self) -> Vec<GoalRow> {
        let mut by_id: HashMap<Id, GoalRow> = HashMap::new();

        // Title + created_at.
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
                    parent: None,
                    higher_over: Vec::new(),
                },
            );
        }

        // Tags.
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

        // Parents.
        for (gid, parent) in find!(
            (gid: Id, parent: Id),
            pattern!(&self.space, [{
                ?gid @
                metadata::tag: &KIND_GOAL_ID,
                compass::parent: ?parent,
            }])
        ) {
            if let Some(row) = by_id.get_mut(&gid) {
                row.parent = Some(parent);
            }
        }

        // Latest status per goal.
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

        // Note counts.
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

        // Priority edges: higher > lower. We don't track deprioritize
        // events in the widget — the faculty CLI remains the canonical
        // way to remove relationships — so this is a best-effort view.
        for (higher, lower) in find!(
            (higher: Id, lower: Id),
            pattern!(&self.space, [{
                _?event @
                metadata::tag: &KIND_PRIORITIZE_ID,
                compass::higher: ?higher,
                compass::lower: ?lower,
            }])
        ) {
            if let Some(row) = by_id.get_mut(&higher) {
                if !row.higher_over.contains(&lower) {
                    row.higher_over.push(lower);
                }
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

    // ── Write operations (mirror faculty CLI fact shapes) ─────────────

    fn add_goal(
        &mut self,
        title: String,
        status: String,
        parent: Option<Id>,
        tags: Vec<String>,
    ) -> Result<Id, String> {
        let task_id: ExclusiveId = ufoid();
        let task_ref: Id = task_id.id;
        let now = epoch_interval(now_epoch());
        let title_handle = self.ws.put::<LongString, _>(title);

        let mut change = TribleSet::new();
        change += entity! { &task_id @
            metadata::tag: &KIND_GOAL_ID,
            compass::title: title_handle,
            metadata::created_at: now,
            compass::parent?: parent.as_ref(),
            compass::tag*: tags.iter().map(|t| t.as_str()),
        };
        let status_id: ExclusiveId = ufoid();
        change += entity! { &status_id @
            metadata::tag: &KIND_STATUS_ID,
            compass::task: &task_ref,
            compass::status: status.as_str(),
            metadata::created_at: now,
        };

        self.commit_and_push(change, "add goal")?;
        Ok(task_ref)
    }

    fn move_status(&mut self, task_id: Id, status: String) -> Result<(), String> {
        let now = epoch_interval(now_epoch());
        let status_id: ExclusiveId = ufoid();
        let mut change = TribleSet::new();
        change += entity! { &status_id @
            metadata::tag: &KIND_STATUS_ID,
            compass::task: &task_id,
            compass::status: status.as_str(),
            metadata::created_at: now,
        };
        self.commit_and_push(change, "move goal")
    }

    fn add_note(&mut self, task_id: Id, body: String) -> Result<(), String> {
        let now = epoch_interval(now_epoch());
        let note_id: ExclusiveId = ufoid();
        let body_handle = self.ws.put::<LongString, _>(body);
        let mut change = TribleSet::new();
        change += entity! { &note_id @
            metadata::tag: &KIND_NOTE_ID,
            compass::task: &task_id,
            compass::note: body_handle,
            metadata::created_at: now,
        };
        self.commit_and_push(change, "add goal note")
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

// ── Tree layout ──────────────────────────────────────────────────────

/// Depth-first walk through parent/child forest, yielding (row, depth).
/// Rows that have a parent outside this subset are treated as roots.
fn order_rows(rows: Vec<GoalRow>) -> Vec<(GoalRow, usize)> {
    let mut by_id: HashMap<Id, GoalRow> = HashMap::new();
    for row in rows {
        by_id.insert(row.id, row);
    }
    let ids: HashSet<Id> = by_id.keys().copied().collect();
    let mut children: HashMap<Id, Vec<Id>> = HashMap::new();
    let mut roots = Vec::new();

    for (id, row) in &by_id {
        if let Some(parent) = row.parent {
            if ids.contains(&parent) {
                children.entry(parent).or_default().push(*id);
                continue;
            }
        }
        roots.push(*id);
    }

    let sort_ids = |items: &mut Vec<Id>, by_id: &HashMap<Id, GoalRow>| {
        items.sort_by(|a, b| {
            let a_row = by_id.get(a);
            let b_row = by_id.get(b);
            let a_key = a_row.map(|r| r.sort_key()).unwrap_or(i128::MIN);
            let b_key = b_row.map(|r| r.sort_key()).unwrap_or(i128::MIN);
            b_key
                .cmp(&a_key)
                .then_with(|| {
                    let at = a_row.map(|r| r.title.as_str()).unwrap_or("");
                    let bt = b_row.map(|r| r.title.as_str()).unwrap_or("");
                    at.to_lowercase().cmp(&bt.to_lowercase())
                })
                .then_with(|| a.cmp(b))
        });
    };

    sort_ids(&mut roots, &by_id);
    for kids in children.values_mut() {
        sort_ids(kids, &by_id);
    }

    let mut ordered = Vec::new();
    let mut visited = HashSet::new();

    fn walk(
        id: Id,
        depth: usize,
        by_id: &HashMap<Id, GoalRow>,
        children: &HashMap<Id, Vec<Id>>,
        visited: &mut HashSet<Id>,
        out: &mut Vec<(GoalRow, usize)>,
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
    // Any unvisited (e.g. parent-cycle) nodes get a depth-0 fallback.
    let leftovers: Vec<Id> = by_id.keys().copied().filter(|id| !visited.contains(id)).collect();
    for id in leftovers {
        walk(id, 0, &by_id, &children, &mut visited, &mut ordered);
    }
    ordered
}

// ── Compose form state ───────────────────────────────────────────────

/// Inline "+ Add" form bound to a specific column (status).
#[derive(Default)]
struct ComposeForm {
    open: bool,
    title: String,
    tags: String,
    /// Hex-prefix for a parent goal; resolved against `goals` when
    /// submitting (ambiguous or unknown = none).
    parent_prefix: String,
}

// ── Widget ───────────────────────────────────────────────────────────

/// GORBIE-embeddable kanban-style compass board.
///
/// Full-featured: supports composing goals, moving status, adding notes,
/// parent/child nesting with per-subtree collapse, priority arrow badges,
/// and colorhashed tag chips. See the module docs for details.
///
/// ```ignore
/// let mut board = CompassBoard::new("./self.pile", "compass");
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
    /// Transient error toast from the last write (clears on next success).
    toast: Option<String>,
    expanded_goal: Option<Id>,
    /// Goals whose children should be hidden (parent-node collapsed).
    collapsed: HashSet<Id>,
    compose: HashMap<String, ComposeForm>,
    /// Per-goal inline note-input buffer.
    note_inputs: HashMap<Id, String>,
    /// Goal whose status-move menu is currently open.
    status_menu: Option<Id>,
    column_height: f32,
}

impl CompassBoard {
    /// Build a board pointing at a pile on disk and a named branch. The
    /// pile is not opened until the first [`render`](Self::render) call.
    pub fn new(pile_path: impl Into<PathBuf>, branch_name: impl Into<String>) -> Self {
        Self {
            pile_path: pile_path.into(),
            branch_name: branch_name.into(),
            live: None,
            error: None,
            toast: None,
            expanded_goal: None,
            collapsed: HashSet::new(),
            compose: HashMap::new(),
            note_inputs: HashMap::new(),
            status_menu: None,
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

        let mut goals = live.goals();
        // Global sort used when a goal has no parent context.
        goals.sort_by(|a, b| {
            b.sort_key()
                .cmp(&a.sort_key())
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                .then_with(|| a.id.cmp(&b.id))
        });

        // Fill tree-ordered (row, depth) vectors per-status.
        let mut by_status: BTreeMap<String, Vec<GoalRow>> = BTreeMap::new();
        for g in goals.clone() {
            by_status.entry(g.status.clone()).or_default().push(g);
        }

        let mut columns: Vec<String> = DEFAULT_STATUSES.iter().map(|s| s.to_string()).collect();
        let mut extras: Vec<String> = by_status
            .keys()
            .filter(|s| !DEFAULT_STATUSES.contains(&s.as_str()))
            .cloned()
            .collect();
        extras.sort();
        columns.extend(extras);

        // Pre-compute a global id→title lookup (used for "> over <prefix>"
        // badges when the target isn't in the same column).
        let title_by_id: HashMap<Id, String> = goals
            .iter()
            .map(|g| (g.id, g.title.clone()))
            .collect();

        // Per-column tree-ordered rows.
        let column_data: Vec<(String, Vec<(GoalRow, usize)>)> = columns
            .into_iter()
            .map(|s| {
                let rows = by_status.remove(&s).unwrap_or_default();
                let ordered = order_rows(rows);
                (s, ordered)
            })
            .collect();

        // Resolve expanded goal's notes (if any) while we still hold `live`.
        let expanded = self.expanded_goal;
        let expanded_notes: Option<(Id, Vec<NoteRow>)> = expanded.map(|gid| {
            let notes = live.notes_for(gid);
            (gid, notes)
        });

        drop(live);

        // Pull scalars out of `self` before the closure so we don't end up
        // with conflicting borrows.
        let column_height = self.column_height;
        let branch_name = self.branch_name.clone();
        let total_goals: usize = column_data.iter().map(|(_, r)| r.len()).sum();

        // Write intents collected during render (applied after the UI closure
        // so we don't re-enter `live` while holding egui state).
        let mut add_intent: Option<AddIntent> = None;
        let mut move_intent: Option<(Id, String)> = None;
        let mut note_intent: Option<(Id, String)> = None;

        // Mutable handles to self state we need inside the closure.
        let expanded_goal = &mut self.expanded_goal;
        let collapsed = &mut self.collapsed;
        let compose = &mut self.compose;
        let note_inputs = &mut self.note_inputs;
        let status_menu = &mut self.status_menu;
        let toast = &mut self.toast;

        ctx.section(&format!("Compass: {branch_name}"), |ctx| {
            ctx.label(format!("{total_goals} goals"));

            if let Some(msg) = toast.as_ref() {
                let color = ctx.ctx().style().visuals.error_fg_color;
                ctx.label(
                    egui::RichText::new(msg.as_str())
                        .color(color)
                        .monospace()
                        .small(),
                );
            }

            if total_goals == 0 && column_data.iter().all(|(s, _)| !compose.contains_key(s)) {
                ctx.label("No goals yet. Click + Add in a column below to start.");
            }

            // Lay columns out on GORBIE's 12-col grid. For the default
            // four statuses this is 4 × 3 cells; five or six statuses fall
            // back to 2 cells each; anything denser wraps automatically.
            let col_count = column_data.len().max(1);
            let span = (12u32 / col_count as u32).max(1);

            ctx.grid(|g| {
                for (status, rows) in &column_data {
                    let form = compose.entry(status.clone()).or_default();
                    g.place(span, |ctx| {
                        let ui = ctx.ui_mut();
                        render_column(
                            ui,
                            status,
                            rows,
                            column_height,
                            expanded_goal,
                            expanded_notes.as_ref(),
                            collapsed,
                            note_inputs,
                            status_menu,
                            form,
                            &title_by_id,
                            &mut add_intent,
                            &mut move_intent,
                            &mut note_intent,
                        );
                    });
                }
            });
        });

        // Apply writes after the UI closure.
        if add_intent.is_some() || move_intent.is_some() || note_intent.is_some() {
            let Some(live_lock) = self.live.as_ref() else {
                return;
            };
            let mut live = live_lock.lock();
            let mut err_msg: Option<String> = None;

            if let Some(intent) = add_intent {
                match live.add_goal(intent.title, intent.status.clone(), intent.parent, intent.tags)
                {
                    Ok(_new_id) => {
                        // Close the compose form on success.
                        if let Some(form) = self.compose.get_mut(&intent.status) {
                            form.open = false;
                            form.title.clear();
                            form.tags.clear();
                            form.parent_prefix.clear();
                        }
                        self.toast = None;
                    }
                    Err(e) => err_msg = Some(format!("add failed: {e}")),
                }
            }
            if let Some((id, status)) = move_intent {
                match live.move_status(id, status) {
                    Ok(()) => {
                        self.status_menu = None;
                        self.toast = None;
                    }
                    Err(e) => err_msg = Some(format!("move failed: {e}")),
                }
            }
            if let Some((id, body)) = note_intent {
                let body_trimmed = body.trim();
                if body_trimmed.is_empty() {
                    err_msg = Some("note body is empty".to_string());
                } else {
                    match live.add_note(id, body_trimmed.to_string()) {
                        Ok(()) => {
                            if let Some(buf) = self.note_inputs.get_mut(&id) {
                                buf.clear();
                            }
                            self.toast = None;
                        }
                        Err(e) => err_msg = Some(format!("note failed: {e}")),
                    }
                }
            }

            if let Some(msg) = err_msg {
                self.toast = Some(msg);
            }
        }
    }
}

// ── Write intents ────────────────────────────────────────────────────

struct AddIntent {
    title: String,
    status: String,
    parent: Option<Id>,
    tags: Vec<String>,
}

// ── Column rendering ─────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_column(
    ui: &mut egui::Ui,
    status: &str,
    rows: &[(GoalRow, usize)],
    height: f32,
    expanded_goal: &mut Option<Id>,
    expanded_notes: Option<&(Id, Vec<NoteRow>)>,
    collapsed: &mut HashSet<Id>,
    note_inputs: &mut HashMap<Id, String>,
    status_menu: &mut Option<Id>,
    form: &mut ComposeForm,
    title_by_id: &HashMap<Id, String>,
    add_intent: &mut Option<AddIntent>,
    move_intent: &mut Option<(Id, String)>,
    note_intent: &mut Option<(Id, String)>,
) {
    let status_col = status_color(status);
    egui::Frame::NONE
        .fill(color_frame())
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            // Fill the grid cell we were placed in; force vertical layout
            // (Frame inherits its parent's direction by default).
            ui.set_width(ui.available_width());
            ui.set_min_height(height);
            ui.vertical(|ui| {

            // Column header + "+ Add" toggle.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("{} ({})", status.to_uppercase(), rows.len()))
                        .monospace()
                        .strong()
                        .color(status_col),
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui
                            .small_button(if form.open { "×" } else { "+ Add" })
                            .clicked()
                        {
                            form.open = !form.open;
                        }
                    },
                );
            });
            ui.add_space(4.0);

            // Inline compose form.
            if form.open {
                render_compose_form(ui, status, form, add_intent);
                ui.add_space(6.0);
            }

            // Collect set of visible IDs for filtering children of collapsed parents.
            let ancestors_collapsed: HashSet<Id> = {
                // An ID is "hidden" if any of its ancestors (inside this
                // column, among the tree-ordered rows) is in `collapsed`.
                let mut hidden: HashSet<Id> = HashSet::new();
                // Walk tree-ordered list; since depth is non-decreasing when
                // walking into a subtree, we can track the active path.
                let mut path: Vec<(Id, usize)> = Vec::new();
                for (row, depth) in rows {
                    while path.last().map(|(_, d)| *d >= *depth).unwrap_or(false) {
                        path.pop();
                    }
                    let parent_hidden = path.iter().any(|(pid, _)| {
                        hidden.contains(pid) || collapsed.contains(pid)
                    });
                    if parent_hidden {
                        hidden.insert(row.id);
                    }
                    path.push((row.id, *depth));
                }
                hidden
            };

            egui::ScrollArea::vertical()
                .id_salt(("compass_column", status))
                .max_height(height)
                .auto_shrink([false, false])
                // Disable drag-to-scroll — it registers a content-wide
                // `Sense::drag()` that collides with nested click-senses
                // on cards/triangles and trips an `unwrap()` in egui's
                // hit_test under some layouts (egui 0.33.x / 0.34.x).
                .scroll_source(egui::scroll_area::ScrollSource {
                    scroll_bar: true,
                    drag: false,
                    mouse_wheel: true,
                })
                .show(ui, |ui| {
                    if rows.is_empty() && !form.open {
                        ui.small("(empty)");
                        return;
                    }
                    for (row, depth) in rows {
                        if ancestors_collapsed.contains(&row.id) {
                            continue;
                        }
                        render_goal_card(
                            ui,
                            row,
                            *depth,
                            expanded_goal,
                            expanded_notes,
                            collapsed,
                            note_inputs,
                            status_menu,
                            title_by_id,
                            move_intent,
                            note_intent,
                        );
                        ui.add_space(6.0);
                    }
                });
            });
        });
}

fn render_compose_form(
    ui: &mut egui::Ui,
    status: &str,
    form: &mut ComposeForm,
    add_intent: &mut Option<AddIntent>,
) {
    egui::Frame::NONE
        .fill(card_bg())
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(format!("New goal → {status}"))
                    .small()
                    .color(color_muted()),
            );
            ui.add(
                egui::TextEdit::singleline(&mut form.title)
                    .hint_text("title")
                    .desired_width(f32::INFINITY),
            );
            ui.add(
                egui::TextEdit::singleline(&mut form.tags)
                    .hint_text("tags (space-separated)")
                    .desired_width(f32::INFINITY),
            );
            ui.add(
                egui::TextEdit::singleline(&mut form.parent_prefix)
                    .hint_text("parent id prefix (optional)")
                    .desired_width(f32::INFINITY),
            );
            ui.horizontal(|ui| {
                let submit_enabled = !form.title.trim().is_empty() && add_intent.is_none();
                if ui
                    .add_enabled(submit_enabled, egui::Button::new("Create"))
                    .clicked()
                {
                    let parent = resolve_prefix_hack(&form.parent_prefix);
                    let tags: Vec<String> = form
                        .tags
                        .split_whitespace()
                        .map(|s| s.trim_start_matches('#').to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    *add_intent = Some(AddIntent {
                        title: form.title.trim().to_string(),
                        status: status.to_string(),
                        parent,
                        tags,
                    });
                }
                if ui.small_button("Cancel").clicked() {
                    form.open = false;
                    form.title.clear();
                    form.tags.clear();
                    form.parent_prefix.clear();
                }
            });
        });
}

/// Resolve a hex prefix to a full Id. This widget can't access the live
/// connection at form-render time (it'd re-enter the mutex), so we only
/// accept a full 32-char hex id. Shorter prefixes silently yield `None`.
/// Callers who need prefix resolution should copy the full id from the
/// board into the field — which is easy because the id_prefix is always
/// shown on cards.
fn resolve_prefix_hack(prefix: &str) -> Option<Id> {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Only accept full 32-char hex — shorter prefixes are ambiguous and
    // we'd need another mutex re-entry to resolve them.
    Id::from_hex(trimmed)
}

// ── Card rendering ───────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_goal_card(
    ui: &mut egui::Ui,
    row: &GoalRow,
    depth: usize,
    expanded_goal: &mut Option<Id>,
    expanded_notes: Option<&(Id, Vec<NoteRow>)>,
    collapsed: &mut HashSet<Id>,
    note_inputs: &mut HashMap<Id, String>,
    status_menu: &mut Option<Id>,
    title_by_id: &HashMap<Id, String>,
    move_intent: &mut Option<(Id, String)>,
    note_intent: &mut Option<(Id, String)>,
) {
    const DEP_LINE_STEP: f32 = 6.0;
    const DEP_LINE_BASE: f32 = 8.0;
    let dep_lines = depth.min(3);
    let dep_indent = if dep_lines == 0 {
        0.0
    } else {
        (dep_lines as f32 * DEP_LINE_STEP) + DEP_LINE_BASE
    };

    let is_expanded = *expanded_goal == Some(row.id);
    let is_collapsed = collapsed.contains(&row.id);

    let card_response = egui::Frame::NONE
        .fill(card_bg())
        .corner_radius(egui::CornerRadius::same(4))
        .outer_margin(egui::Margin {
            left: dep_indent as i8,
            right: 0,
            top: 0,
            bottom: 0,
        })
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // Row 1: status chip · title · collapse triangle · short id.
            ui.horizontal(|ui| {
                render_chip(ui, &row.status, status_color(&row.status));
                // Collapse-subtree triangle, only shown when there are
                // visible children (we don't know here without the tree
                // snapshot, so show it always at depth=0 or higher — the
                // click is a no-op for leaves but is harmless).
                let tri = if is_collapsed { "▸" } else { "▾" };
                if ui
                    .add(
                        egui::Label::new(
                            egui::RichText::new(tri).monospace().color(color_muted()),
                        )
                        .sense(egui::Sense::click()),
                    )
                    .clicked()
                {
                    if is_collapsed {
                        collapsed.remove(&row.id);
                    } else {
                        collapsed.insert(row.id);
                    }
                }

                ui.add(
                    egui::Label::new(egui::RichText::new(&row.title).monospace())
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });

            // Row 2: id prefix · optional parent pointer · note count chip.
            ui.horizontal(|ui| {
                let id_text = if let Some(parent) = row.parent {
                    format!("^{} {}", id_prefix(parent), row.id_prefix)
                } else {
                    row.id_prefix.clone()
                };
                ui.label(
                    egui::RichText::new(id_text)
                        .monospace()
                        .small()
                        .color(color_muted()),
                );
                if row.note_count > 0 {
                    render_chip(ui, &format!("{}n", row.note_count), color_muted());
                }
            });

            // Row 3: priority edges + tags.
            let has_prio = !row.higher_over.is_empty();
            if has_prio || !row.tags.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    for lower in &row.higher_over {
                        let target_label = title_by_id
                            .get(lower)
                            .map(|t| {
                                if t.len() > 20 {
                                    format!("{}…", &t[..20])
                                } else {
                                    t.clone()
                                }
                            })
                            .unwrap_or_else(|| id_prefix(*lower));
                        render_chip(
                            ui,
                            &format!("▲ over {target_label}"),
                            egui::Color32::from_rgb(0x55, 0x3f, 0x7f),
                        );
                    }
                    for tag in &row.tags {
                        render_chip(ui, &format!("#{tag}"), tag_color(tag));
                    }
                });
            }
        })
        .response;

    // Whole card is clickable to toggle note expansion.
    let click_id = ui.make_persistent_id(("compass_goal", row.id));
    let response = ui.interact(card_response.rect, click_id, egui::Sense::click());
    if response.clicked() {
        if *expanded_goal == Some(row.id) {
            *expanded_goal = None;
        } else {
            *expanded_goal = Some(row.id);
        }
    }
    let secondary = response.secondary_clicked();
    if secondary || response.hovered() && ui.input(|i| i.modifiers.shift && i.pointer.any_click()) {
        *status_menu = Some(row.id);
    }

    // Status-menu popup (opens next to the card).
    if *status_menu == Some(row.id) {
        egui::Window::new(format!("move_menu_{}", row.id_prefix))
            .title_bar(false)
            .resizable(false)
            .fixed_pos(card_response.rect.right_top())
            .show(ui.ctx(), |ui| {
                ui.label(
                    egui::RichText::new("Move to…")
                        .small()
                        .color(color_muted()),
                );
                for status in DEFAULT_STATUSES {
                    let fill = status_color(status);
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new(status).color(fill).monospace(),
                        ))
                        .clicked()
                    {
                        *move_intent = Some((row.id, status.to_string()));
                    }
                }
                if ui.small_button("Cancel").clicked() {
                    *status_menu = None;
                }
            });
    }

    if is_expanded {
        let notes: &[NoteRow] = expanded_notes
            .filter(|(gid, _)| *gid == row.id)
            .map(|(_, n)| n.as_slice())
            .unwrap_or(&[]);
        egui::Frame::NONE
            .stroke(egui::Stroke::new(1.0, color_muted()))
            .outer_margin(egui::Margin {
                left: dep_indent as i8,
                right: 0,
                top: 0,
                bottom: 0,
            })
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());

                // Move-status row (inline, as an alternative to the popup).
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("move to")
                            .small()
                            .color(color_muted()),
                    );
                    for status in DEFAULT_STATUSES {
                        if status == row.status {
                            continue;
                        }
                        let fill = status_color(status);
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new(status).color(fill).small(),
                            ))
                            .clicked()
                        {
                            *move_intent = Some((row.id, status.to_string()));
                        }
                    }
                });

                ui.separator();

                // Notes.
                let now = now_tai_ns();
                if notes.is_empty() {
                    ui.small("(no notes)");
                } else {
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
                }

                ui.separator();

                // + Note inline form.
                let buf = note_inputs.entry(row.id).or_default();
                ui.add(
                    egui::TextEdit::multiline(buf)
                        .hint_text("new note…")
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    let submit_enabled =
                        !buf.trim().is_empty() && note_intent.is_none();
                    if ui
                        .add_enabled(submit_enabled, egui::Button::new("+ Note"))
                        .clicked()
                    {
                        *note_intent = Some((row.id, buf.clone()));
                    }
                });
            });
        ui.add_space(4.0);
    }

    // Draw dependency gutter lines to the left of the card.
    let rect = card_response.rect;
    let painter = ui.painter();
    let stroke = egui::Stroke::new(1.2, color_muted());
    for idx in 0..dep_lines {
        let x = rect.left() - dep_indent + 4.0 + (idx as f32 * DEP_LINE_STEP);
        let y1 = rect.top() + 0.5;
        let y2 = rect.bottom() - 0.5;
        painter.line_segment([egui::pos2(x, y1), egui::pos2(x, y2)], stroke);
    }
}

fn render_chip(ui: &mut egui::Ui, label: &str, fill: egui::Color32) {
    let text = colorhash::text_color_on(fill);
    egui::Frame::NONE
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(label).small().color(text));
        });
}
