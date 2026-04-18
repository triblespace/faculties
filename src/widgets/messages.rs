//! Full-featured GORBIE-embeddable local-messages panel.
//!
//! Renders the append-only direct messages kept on a pile's
//! `local-messages` branch as a chronological chat log: oldest at the
//! top, newest at the bottom. When constructed via
//! [`MessagesPanel::with_user`], the panel also supports composing new
//! messages and automatically committing read-receipts for the current
//! user's inbound messages.
//!
//! Identity display is resolved against the pile's `relations` branch.
//! A person's display name falls back through `alias → first_name
//! last_name → display_name → 8-char hex prefix`. If the relations
//! branch is absent the widget quietly degrades to the hex-prefix view.
//!
//! Sender and recipient color chips use
//! `GORBIE::themes::colorhash::ral_categorical` keyed on the user id
//! bytes, so the same person always gets the same hue.
//!
//! ```ignore
//! // Read-only (anonymous):
//! let mut panel = MessagesPanel::new("./self.pile", "local-messages");
//!
//! // Interactive (composes + marks read as `me`):
//! let mut panel = MessagesPanel::with_user("./self.pile", "local-messages", me)
//!     .with_default_recipient(peer);
//! // Inside a GORBIE card:
//! panel.render(ctx);
//! ```

use std::collections::{HashMap, HashSet};
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

use crate::schemas::local_messages::{local, KIND_MESSAGE_ID, KIND_READ_ID};
use crate::schemas::relations::{relations as rel, KIND_PERSON_ID};

/// Default branch name the local-messages faculty writes to.
pub const LOCAL_MESSAGES_BRANCH_NAME: &str = "local-messages";
/// Default branch name the relations faculty writes to.
pub const RELATIONS_BRANCH_NAME: &str = "relations";

/// Handle to a long-string blob (message bodies).
type TextHandle = Value<Handle<Blake3, LongString>>;
/// Interval value (TAI ns lower/upper) used for `metadata::created_at`.
type IntervalValue = Value<NsTAIInterval>;

// ── ID / time helpers ────────────────────────────────────────────────

fn fmt_id_full(id: Id) -> String {
    format!("{id:x}")
}

/// First 8 hex chars of an Id — fallback label when no friendly name is
/// resolvable from the relations branch.
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
    hifitime::Epoch::now()
        .unwrap_or_else(|_| hifitime::Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0))
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

fn format_age_key(now_key: i128, past_key: i128) -> String {
    format_age(now_key, Some(past_key))
}

// ── Color palette (reuses compass.rs conventions) ────────────────────

fn color_frame() -> egui::Color32 {
    // RAL 7016 anthracite grey — matches compass column frame.
    egui::Color32::from_rgb(0x29, 0x32, 0x36)
}

fn color_bubble() -> egui::Color32 {
    // Slightly lighter than the frame so message bubbles stand out.
    egui::Color32::from_rgb(0x33, 0x3b, 0x40)
}

fn color_muted() -> egui::Color32 {
    // RAL 7012 basalt grey.
    egui::Color32::from_rgb(0x4d, 0x55, 0x59)
}

fn color_accent() -> egui::Color32 {
    // RAL 6032 signal green — matches playground `color_local_msg`.
    egui::Color32::from_rgb(0x23, 0x7f, 0x52)
}

fn color_read() -> egui::Color32 {
    // RAL 6017 may green — "read" accent, matches playground diagnostics.
    egui::Color32::from_rgb(0x4a, 0x77, 0x29)
}

/// Deterministic per-person color chip via GORBIE's colorhash palette.
fn person_color(id: Id) -> egui::Color32 {
    colorhash::ral_categorical(id.as_ref())
}

// ── Row structs ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct MessageRow {
    id: Id,
    from: Id,
    to: Id,
    body: String,
    /// TAI ns of the message's `metadata::created_at` (sort key).
    created_at: Option<i128>,
    /// Read receipts for this message. Each entry is `(reader, ts_ns)`.
    reads: Vec<(Id, i128)>,
}

impl MessageRow {
    fn sort_key(&self) -> i128 {
        self.created_at.unwrap_or(i128::MIN)
    }
}

/// Everything we know about a person for UI purposes.
#[derive(Clone, Debug, Default)]
struct Person {
    alias: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    display_name: Option<String>,
}

impl Person {
    /// Display name: alias > first+last > display_name > hex prefix.
    fn display(&self, fallback_id: Id) -> String {
        if let Some(a) = self.alias.as_ref() {
            if !a.trim().is_empty() {
                return a.clone();
            }
        }
        match (self.first_name.as_ref(), self.last_name.as_ref()) {
            (Some(f), Some(l)) if !f.trim().is_empty() && !l.trim().is_empty() => {
                return format!("{f} {l}");
            }
            (Some(f), _) if !f.trim().is_empty() => return f.clone(),
            (_, Some(l)) if !l.trim().is_empty() => return l.clone(),
            _ => {}
        }
        if let Some(d) = self.display_name.as_ref() {
            if !d.trim().is_empty() {
                return d.clone();
            }
        }
        id_prefix(fallback_id)
    }
}

// ── Live messages connection (read + write) ──────────────────────────

/// Owns the open pile, repository handle, and active workspaces for the
/// local-messages + (optional) relations branches. Queries run against
/// the cached `space`s; writes build a change tribleset, call
/// `ws.commit(..)`, push via `try_push`, then `refresh`.
struct MessagesLive {
    branch_name: String,
    branch_id: Id,
    space: TribleSet,
    ws: Workspace<Pile<Blake3>>,
    repo: Repository<Pile<Blake3>>,

    /// Cached people lookup from the relations branch. `None` if the
    /// branch doesn't exist on this pile.
    relations_branch_id: Option<Id>,
    relations_space: TribleSet,
    relations_ws: Option<Workspace<Pile<Blake3>>>,
    people: HashMap<Id, Person>,
}

impl MessagesLive {
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

        // Relations branch is best-effort: absence just means we show
        // hex prefixes instead of friendly names.
        let (relations_branch_id, relations_space, relations_ws) =
            match find_branch(&mut repo, RELATIONS_BRANCH_NAME) {
                Some(rid) => match repo.pull(rid) {
                    Ok(mut rws) => match rws.checkout(..) {
                        Ok(co) => (Some(rid), co.into_facts(), Some(rws)),
                        Err(e) => {
                            eprintln!("[messages] relations checkout: {e:?}");
                            (None, TribleSet::new(), None)
                        }
                    },
                    Err(e) => {
                        eprintln!("[messages] relations pull: {e:?}");
                        (None, TribleSet::new(), None)
                    }
                },
                None => (None, TribleSet::new(), None),
            };

        let mut live = MessagesLive {
            branch_name: branch_name.to_string(),
            branch_id,
            space,
            ws,
            repo,
            relations_branch_id,
            relations_space,
            relations_ws,
            people: HashMap::new(),
        };
        live.rebuild_people();
        Ok(live)
    }

    /// Re-pull both the local-messages and relations branches and
    /// rebuild the people map. Called after each successful commit.
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

        if let Some(rid) = self.relations_branch_id {
            match self.repo.pull(rid) {
                Ok(mut rws) => match rws.checkout(..) {
                    Ok(co) => {
                        self.relations_space = co.into_facts();
                        self.relations_ws = Some(rws);
                    }
                    Err(e) => eprintln!("[messages] relations checkout on refresh: {e:?}"),
                },
                Err(e) => eprintln!("[messages] relations pull on refresh: {e:?}"),
            }
        }
        self.rebuild_people();
        Ok(())
    }

    /// Commit a change and push. On CAS conflict: refresh and return an
    /// error string so the caller can surface it as a toast.
    fn commit_and_push(&mut self, change: TribleSet, message: &str) -> Result<(), String> {
        self.ws.commit(change, message);
        match self.repo.try_push(&mut self.ws) {
            Ok(None) => self.refresh(),
            Ok(Some(_conflict_ws)) => {
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

    fn relations_text(&mut self, h: TextHandle) -> Option<String> {
        self.relations_ws
            .as_mut()
            .and_then(|ws| ws.get::<View<str>, LongString>(h).ok())
            .map(|v| {
                let s: &str = v.as_ref();
                s.to_string()
            })
    }

    /// Rebuild `people` from the cached `relations_space`. Done once on
    /// open and again after every successful commit.
    fn rebuild_people(&mut self) {
        self.people.clear();
        if self.relations_ws.is_none() {
            return;
        }

        let person_ids: Vec<Id> = find!(
            pid: Id,
            pattern!(&self.relations_space, [{ ?pid @ metadata::tag: &KIND_PERSON_ID }])
        )
        .collect();

        for pid in &person_ids {
            self.people.insert(*pid, Person::default());
        }

        let alias_rows: Vec<(Id, String)> = find!(
            (pid: Id, alias: String),
            pattern!(&self.relations_space, [{ ?pid @ rel::alias: ?alias }])
        )
        .collect();
        for (pid, alias) in alias_rows {
            if let Some(p) = self.people.get_mut(&pid) {
                // Keep the lexicographically-first alias so the rendered
                // name is stable across runs.
                match p.alias.as_ref() {
                    Some(existing) if existing.as_str() <= alias.as_str() => {}
                    _ => p.alias = Some(alias),
                }
            }
        }

        let first_rows: Vec<(Id, TextHandle)> = find!(
            (pid: Id, h: TextHandle),
            pattern!(&self.relations_space, [{ ?pid @ rel::first_name: ?h }])
        )
        .collect();
        for (pid, h) in first_rows {
            if self.people.contains_key(&pid) {
                if let Some(v) = self.relations_text(h) {
                    if let Some(p) = self.people.get_mut(&pid) {
                        p.first_name.get_or_insert(v);
                    }
                }
            }
        }

        let last_rows: Vec<(Id, TextHandle)> = find!(
            (pid: Id, h: TextHandle),
            pattern!(&self.relations_space, [{ ?pid @ rel::last_name: ?h }])
        )
        .collect();
        for (pid, h) in last_rows {
            if self.people.contains_key(&pid) {
                if let Some(v) = self.relations_text(h) {
                    if let Some(p) = self.people.get_mut(&pid) {
                        p.last_name.get_or_insert(v);
                    }
                }
            }
        }

        let display_rows: Vec<(Id, TextHandle)> = find!(
            (pid: Id, h: TextHandle),
            pattern!(&self.relations_space, [{ ?pid @ rel::display_name: ?h }])
        )
        .collect();
        for (pid, h) in display_rows {
            if self.people.contains_key(&pid) {
                if let Some(v) = self.relations_text(h) {
                    if let Some(p) = self.people.get_mut(&pid) {
                        p.display_name.get_or_insert(v);
                    }
                }
            }
        }
    }

    /// Friendly display name for an Id, falling back to hex prefix.
    fn display_name(&self, id: Id) -> String {
        match self.people.get(&id) {
            Some(p) => p.display(id),
            None => id_prefix(id),
        }
    }

    /// Known people, sorted by display name, for the recipient picker.
    fn people_sorted(&self) -> Vec<(Id, String)> {
        let mut out: Vec<(Id, String)> = self
            .people
            .iter()
            .map(|(id, p)| (*id, p.display(*id)))
            .collect();
        out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        out
    }

    /// Collect every message with its from/to/body/created_at and fold
    /// in the read-receipt events that target it.
    fn messages(&mut self) -> Vec<MessageRow> {
        let mut by_id: HashMap<Id, MessageRow> = HashMap::new();

        let rows: Vec<(Id, Id, Id, TextHandle, (i128, i128))> = find!(
            (
                mid: Id,
                from: Id,
                to: Id,
                body: TextHandle,
                ts: (i128, i128)
            ),
            pattern!(&self.space, [{
                ?mid @
                metadata::tag: &KIND_MESSAGE_ID,
                local::from: ?from,
                local::to: ?to,
                local::body: ?body,
                metadata::created_at: ?ts,
            }])
        )
        .collect();

        for (mid, from, to, body_handle, ts) in rows {
            if by_id.contains_key(&mid) {
                continue;
            }
            let body = self.text(body_handle);
            by_id.insert(
                mid,
                MessageRow {
                    id: mid,
                    from,
                    to,
                    body,
                    created_at: Some(ts.0),
                    reads: Vec::new(),
                },
            );
        }

        // Read-receipt pairing.
        let mut latest: HashMap<(Id, Id), i128> = HashMap::new();
        for (mid, reader, ts) in find!(
            (mid: Id, reader: Id, ts: (i128, i128)),
            pattern!(&self.space, [{
                _?event @
                metadata::tag: &KIND_READ_ID,
                local::about_message: ?mid,
                local::reader: ?reader,
                local::read_at: ?ts,
            }])
        ) {
            let key = (mid, reader);
            let entry = latest.entry(key).or_insert(i128::MIN);
            if ts.0 > *entry {
                *entry = ts.0;
            }
        }
        for ((mid, reader), ts) in latest {
            if let Some(row) = by_id.get_mut(&mid) {
                row.reads.push((reader, ts));
            }
        }

        for row in by_id.values_mut() {
            row.reads.sort_by(|a, b| b.1.cmp(&a.1));
        }

        by_id.into_values().collect()
    }

    // ── Write operations (mirror faculty CLI fact shapes) ─────────────

    fn send_message(&mut self, from: Id, to: Id, body: String) -> Result<Id, String> {
        let msg_id: ExclusiveId = ufoid();
        let msg_ref: Id = msg_id.id;
        let now = epoch_interval(now_epoch());
        let body_handle = self.ws.put::<LongString, _>(body);

        let mut change = TribleSet::new();
        change += entity! { &msg_id @
            metadata::tag: &KIND_MESSAGE_ID,
            local::from: &from,
            local::to: &to,
            local::body: body_handle,
            metadata::created_at: now,
        };

        self.commit_and_push(change, "local message")?;
        Ok(msg_ref)
    }

    fn mark_read(&mut self, message_id: Id, reader: Id) -> Result<(), String> {
        let now = epoch_interval(now_epoch());
        let read_id: ExclusiveId = ufoid();
        let mut change = TribleSet::new();
        change += entity! { &read_id @
            metadata::tag: &KIND_READ_ID,
            local::about_message: &message_id,
            local::reader: &reader,
            local::read_at: now,
        };
        self.commit_and_push(change, "local message read")
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

/// GORBIE-embeddable local-messages panel with compose, relations
/// identity lookup, scroll-to-bottom on new messages, and automatic
/// read-receipts for inbound messages.
///
/// See the module docs for construction examples.
pub struct MessagesPanel {
    pile_path: PathBuf,
    branch_name: String,
    /// Current user (sender of composed messages, reader for receipts).
    /// `None` = read-only panel; compose UI hidden, no receipts.
    me: Option<Id>,
    /// Preset recipient for composed messages. If `None`, the compose
    /// UI shows a picker populated from the relations branch.
    default_recipient: Option<Id>,
    // Wrapped in a Mutex so the widget is `Send + Sync`. GORBIE's state
    // storage requires that across threads, and `Workspace<Pile<Blake3>>`
    // uses interior-mutability types (Cell/RefCell) that aren't Sync.
    live: Option<Mutex<MessagesLive>>,
    error: Option<String>,
    /// Transient error toast from the last write (clears on next success).
    toast: Option<String>,

    viewport_height: f32,
    /// Composer text buffer.
    compose_draft: String,
    /// User-selected recipient (overrides `default_recipient` when set).
    compose_recipient: Option<Id>,
    /// Message count observed during the last render, used to detect
    /// arrivals and auto-scroll.
    last_message_count: usize,
    /// True when we want to snap the scroll to the bottom on this render.
    scroll_to_bottom: bool,
    /// True when the user has scrolled up from the bottom; suppresses
    /// auto-scroll and shows a "new messages below" indicator instead.
    user_scrolled_up: bool,
    /// Number of messages received since the user scrolled up (cleared
    /// when they return to the bottom).
    pending_new: usize,
    /// Read-receipts we've already committed this session, keyed by
    /// message id. Avoids flooding the pile with duplicate receipts.
    read_sent: HashSet<Id>,
    /// Tracks the first render so we can scroll to the bottom (newest)
    /// on initial paint.
    first_render: bool,
}

impl MessagesPanel {
    /// Read-only panel (anonymous — shows names but no compose UI,
    /// no read-receipts).
    pub fn new(pile_path: impl Into<PathBuf>, branch_name: impl Into<String>) -> Self {
        Self {
            pile_path: pile_path.into(),
            branch_name: branch_name.into(),
            me: None,
            default_recipient: None,
            live: None,
            error: None,
            toast: None,
            viewport_height: 500.0,
            compose_draft: String::new(),
            compose_recipient: None,
            last_message_count: 0,
            scroll_to_bottom: false,
            user_scrolled_up: false,
            pending_new: 0,
            read_sent: HashSet::new(),
            first_render: true,
        }
    }

    /// Interactive panel — composer is active, and inbound messages
    /// addressed to `me` get auto-acknowledged with a read-receipt
    /// (once per message per session).
    pub fn with_user(
        pile_path: impl Into<PathBuf>,
        branch_name: impl Into<String>,
        me: Id,
    ) -> Self {
        let mut s = Self::new(pile_path, branch_name);
        s.me = Some(me);
        s
    }

    /// Address the next composed message to a specific recipient. If
    /// unset, the compose UI shows a recipient picker.
    pub fn with_default_recipient(mut self, to: Id) -> Self {
        self.default_recipient = Some(to);
        self
    }

    /// Override the scroll-area height (pixels). Default 500.
    pub fn with_height(mut self, height: f32) -> Self {
        self.viewport_height = height.max(120.0);
        self
    }

    /// Render the panel into a GORBIE card context.
    pub fn render(&mut self, ctx: &mut CardCtx<'_>) {
        // Lazy pile open on first render.
        if self.live.is_none() && self.error.is_none() {
            match MessagesLive::open(&self.pile_path, &self.branch_name) {
                Ok(live) => self.live = Some(Mutex::new(live)),
                Err(e) => self.error = Some(e),
            }
        }

        ctx.section("Messages", |ctx| {
            if let Some(err) = &self.error {
                ctx.label(format!("messages panel error: {err}"));
                return;
            }

            let Some(live_lock) = self.live.as_ref() else {
                ctx.label("messages panel not initialized");
                return;
            };
            let mut live = live_lock.lock();

            // Pre-materialize everything the UI closure needs.
            let mut messages = live.messages();
            messages.sort_by(|a, b| {
                a.sort_key()
                    .cmp(&b.sort_key())
                    .then_with(|| a.id.cmp(&b.id))
            });

            // Build a name lookup for every id we'll paint (senders,
            // recipients, readers) so we never hold the mutex across
            // the UI closure.
            let mut names: HashMap<Id, String> = HashMap::new();
            for m in &messages {
                names
                    .entry(m.from)
                    .or_insert_with(|| live.display_name(m.from));
                names.entry(m.to).or_insert_with(|| live.display_name(m.to));
                for (r, _) in &m.reads {
                    names.entry(*r).or_insert_with(|| live.display_name(*r));
                }
            }
            if let Some(me) = self.me {
                names.entry(me).or_insert_with(|| live.display_name(me));
            }
            if let Some(def) = self.default_recipient {
                names
                    .entry(def)
                    .or_insert_with(|| live.display_name(def));
            }
            if let Some(sel) = self.compose_recipient {
                names
                    .entry(sel)
                    .or_insert_with(|| live.display_name(sel));
            }

            let people_for_picker: Vec<(Id, String)> =
                if self.me.is_some() && self.default_recipient.is_none() {
                    live.people_sorted()
                } else {
                    Vec::new()
                };

            drop(live);

            // Detect arrivals (fires on first paint too, but we'll
            // overwrite user_scrolled_up below).
            let total = messages.len();
            let grew = total > self.last_message_count;
            let arrivals = total.saturating_sub(self.last_message_count);
            self.last_message_count = total;

            if self.first_render {
                self.scroll_to_bottom = true;
                self.first_render = false;
            } else if grew {
                if self.user_scrolled_up {
                    self.pending_new += arrivals;
                } else {
                    self.scroll_to_bottom = true;
                }
            }

            let now = now_tai_ns();
            let viewport_height = self.viewport_height;

            // Mark-read scan: any inbound message not yet read by `me`
            // gets a receipt (throttled via `read_sent`).
            let mut to_mark_read: Vec<Id> = Vec::new();
            if let Some(me) = self.me {
                for m in &messages {
                    if m.to != me {
                        continue;
                    }
                    if self.read_sent.contains(&m.id) {
                        continue;
                    }
                    if m.reads.iter().any(|(r, _)| *r == me) {
                        // Already acked by us in a previous session.
                        self.read_sent.insert(m.id);
                        continue;
                    }
                    to_mark_read.push(m.id);
                }
            }

            let count_label = format!("{} messages", messages.len());
            ctx.label(count_label);

            if let Some(msg) = self.toast.as_ref() {
                let color = ctx.ctx().global_style().visuals.error_fg_color;
                ctx.label(
                    egui::RichText::new(msg.as_str())
                        .color(color)
                        .monospace()
                        .small(),
                );
            }

            let mut send_intent: Option<(Id, String)> = None;
            ctx.grid(|g| g.full(|ctx| {
            let ui = ctx.ui_mut();
            if messages.is_empty() && self.me.is_none() {
                ui.label("No messages yet.");
                return;
            }

            let scroll_to_bottom = std::mem::take(&mut self.scroll_to_bottom);
            let pending_new = self.pending_new;
            let user_scrolled_up = &mut self.user_scrolled_up;
            let pending_new_slot = &mut self.pending_new;

            let mut scroll = egui::ScrollArea::vertical()
                .id_salt(("messages_panel", self.branch_name.as_str()))
                .max_height(viewport_height)
                .auto_shrink([false, false])
                // Disable drag-to-scroll — see note on compass.rs; prevents
                // an egui hit_test unwrap panic when clickable message cards
                // overlap the drag-sense the scroll area would otherwise
                // register.
                .scroll_source(egui::scroll_area::ScrollSource {
                    scroll_bar: true,
                    drag: false,
                    mouse_wheel: true,
                });
            if scroll_to_bottom {
                scroll = scroll.vertical_scroll_offset(f32::MAX);
            }
            let out = scroll.show(ui, |ui| {
                ui.set_width(ui.available_width());
                if messages.is_empty() {
                    ui.label("No messages yet.");
                }
                for msg in &messages {
                    render_message(ui, msg, now, &names, self.me);
                    ui.add_space(6.0);
                }
            });

            // Stickiness detection: if content is taller than viewport
            // and the scroll offset is within a small epsilon of the
            // bottom, we consider the user "at the bottom"; any scroll
            // above that = `user_scrolled_up`.
            let state = out.state;
            let content_h = out.content_size.y;
            let viewport_h = out.inner_rect.height();
            if content_h > viewport_h + 1.0 {
                let max_offset = content_h - viewport_h;
                let at_bottom = state.offset.y >= max_offset - 4.0;
                if at_bottom {
                    *user_scrolled_up = false;
                    *pending_new_slot = 0;
                } else if !scroll_to_bottom {
                    *user_scrolled_up = true;
                }
            } else {
                *user_scrolled_up = false;
                *pending_new_slot = 0;
            }

            // "N new messages below" indicator. Clicking jumps to bottom.
            if *user_scrolled_up && pending_new > 0 {
                let resp = ui.add(
                    egui::Button::new(
                        egui::RichText::new(format!("▼ {pending_new} new"))
                            .small()
                            .color(colorhash::text_color_on(color_accent())),
                    )
                    .fill(color_accent()),
                );
                if resp.clicked() {
                    self.scroll_to_bottom = true;
                    *user_scrolled_up = false;
                    *pending_new_slot = 0;
                }
            }

            // Compose UI.
            if let Some(me) = self.me {
                ui.separator();
                render_composer(
                    ui,
                    me,
                    self.default_recipient,
                    &mut self.compose_recipient,
                    &people_for_picker,
                    &names,
                    &mut self.compose_draft,
                    &mut send_intent,
                );
            }
            }));

            // Apply writes after UI closure.
            if !to_mark_read.is_empty() || send_intent.is_some() {
                let Some(live_lock) = self.live.as_ref() else {
                    return;
                };
                let mut live = live_lock.lock();
                let mut err: Option<String> = None;

                for mid in to_mark_read {
                    if let Some(me) = self.me {
                        match live.mark_read(mid, me) {
                            Ok(()) => {
                                self.read_sent.insert(mid);
                            }
                            Err(e) => {
                                err = Some(format!("mark-read failed: {e}"));
                                break;
                            }
                        }
                    }
                }

                if let Some((to, body)) = send_intent {
                    let trimmed = body.trim();
                    if trimmed.is_empty() {
                        err = Some("message is empty".to_string());
                    } else if let Some(me) = self.me {
                        match live.send_message(me, to, trimmed.to_string()) {
                            Ok(_new_id) => {
                                self.compose_draft.clear();
                                // Auto-scroll on our own send regardless
                                // of whether we were scrolled up.
                                self.scroll_to_bottom = true;
                                self.user_scrolled_up = false;
                                self.pending_new = 0;
                            }
                            Err(e) => err = Some(format!("send failed: {e}")),
                        }
                    }
                }

                if let Some(msg) = err {
                    self.toast = Some(msg);
                } else {
                    self.toast = None;
                }
            }
        });
    }
}

// ── Composer ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_composer(
    ui: &mut egui::Ui,
    me: Id,
    default_recipient: Option<Id>,
    compose_recipient: &mut Option<Id>,
    people: &[(Id, String)],
    names: &HashMap<Id, String>,
    draft: &mut String,
    send_intent: &mut Option<(Id, String)>,
) {
    let recipient = default_recipient.or(*compose_recipient);

    // Header row: me → recipient chips (or picker).
    ui.horizontal(|ui| {
        let me_name = names.get(&me).cloned().unwrap_or_else(|| id_prefix(me));
        render_chip(ui, &me_name, person_color(me));
        ui.label(
            egui::RichText::new("\u{2192}")
                .monospace()
                .small()
                .color(color_muted()),
        );
        if let Some(to) = default_recipient {
            let to_name = names.get(&to).cloned().unwrap_or_else(|| id_prefix(to));
            render_chip(ui, &to_name, person_color(to));
        } else {
            // Recipient picker.
            let selected_text = match *compose_recipient {
                Some(id) => names
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| id_prefix(id)),
                None => "choose recipient…".to_string(),
            };
            egui::ComboBox::from_id_salt(("messages_recipient_picker",))
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for (pid, name) in people {
                        if *pid == me {
                            continue;
                        }
                        let is_sel = *compose_recipient == Some(*pid);
                        if ui
                            .selectable_label(is_sel, format!("{name} ({})", id_prefix(*pid)))
                            .clicked()
                        {
                            *compose_recipient = Some(*pid);
                        }
                    }
                    if people.is_empty() {
                        ui.small("(no people in relations branch)");
                    }
                });
        }
    });

    ui.add_space(2.0);

    let accent = color_accent();
    egui::Frame::NONE
        .stroke(egui::Stroke::new(1.0, color_muted()))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::same(4))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(draft)
                    .hint_text("Type a message…")
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
        });

    ui.horizontal(|ui| {
        let can_send = recipient.is_some() && !draft.trim().is_empty();
        let send_label = match recipient {
            Some(to) => {
                let name = names.get(&to).cloned().unwrap_or_else(|| id_prefix(to));
                format!("Send → {name}")
            }
            None => "Send".to_string(),
        };
        if ui
            .add_enabled(
                can_send,
                egui::Button::new(
                    egui::RichText::new(send_label).color(colorhash::text_color_on(accent)),
                )
                .fill(accent),
            )
            .clicked()
        {
            if let Some(to) = recipient {
                *send_intent = Some((to, draft.clone()));
            }
        }
        if ui.small_button("Clear").clicked() {
            draft.clear();
        }
    });
}

// ── Row rendering ────────────────────────────────────────────────────

fn render_message(
    ui: &mut egui::Ui,
    msg: &MessageRow,
    now: i128,
    names: &HashMap<Id, String>,
    me: Option<Id>,
) {
    let from_is_me = me == Some(msg.from);
    let bubble_fill = if from_is_me {
        // Tint our own messages toward the accent.
        egui::Color32::from_rgb(0x2b, 0x44, 0x3b)
    } else {
        color_bubble()
    };

    egui::Frame::NONE
        .fill(bubble_fill)
        .stroke(egui::Stroke::new(1.0, color_frame()))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // Header row: from → to, plus age.
            ui.horizontal(|ui| {
                let from_name = names
                    .get(&msg.from)
                    .cloned()
                    .unwrap_or_else(|| id_prefix(msg.from));
                let to_name = names
                    .get(&msg.to)
                    .cloned()
                    .unwrap_or_else(|| id_prefix(msg.to));
                render_chip(ui, &from_name, person_color(msg.from));
                ui.label(
                    egui::RichText::new("\u{2192}")
                        .monospace()
                        .small()
                        .color(color_muted()),
                );
                render_chip(ui, &to_name, person_color(msg.to));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let age = match msg.created_at {
                        Some(k) => format_age_key(now, k),
                        None => "-".to_string(),
                    };
                    ui.label(
                        egui::RichText::new(age)
                            .monospace()
                            .small()
                            .color(color_muted()),
                    );
                });
            });

            ui.add_space(2.0);

            // Body.
            ui.add(
                egui::Label::new(egui::RichText::new(&msg.body))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );

            // Read receipts (if any).
            if !msg.reads.is_empty() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    for (reader, ts) in &msg.reads {
                        let name = names
                            .get(reader)
                            .cloned()
                            .unwrap_or_else(|| id_prefix(*reader));
                        let label = format!("read by {} ({})", name, format_age_key(now, *ts));
                        render_chip(ui, &label, color_read());
                    }
                });
            }

            // Short id footer.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(id_prefix(msg.id))
                        .monospace()
                        .small()
                        .color(color_muted()),
                );
            });
        });
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
