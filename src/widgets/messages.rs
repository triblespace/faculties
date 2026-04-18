//! Minimal GORBIE-embeddable local-messages panel.
//!
//! Renders the append-only direct messages kept on a pile's
//! `local-messages` branch as a chronological chat log: oldest at the
//! top, newest at the bottom, scrolled to the bottom by default. Each
//! row shows the sender, body, created-at timestamp, and a "read by X
//! at Y" indicator when read-receipts exist.
//!
//! Scope is intentionally tight: v1 is read-only. There is no compose
//! UI (the faculty CLI handles writes), no reply threading, no search,
//! no attachments, and no cross-branch identity lookup against the
//! `relations` branch — people are shown by the first 8 hex chars of
//! their id.
//!
//! ```ignore
//! let mut panel = MessagesPanel::new("./self.pile", "local-messages");
//! // Inside a GORBIE card:
//! panel.render(ctx);
//! ```

use std::collections::HashMap;
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

use crate::schemas::local_messages::{local, KIND_MESSAGE_ID, KIND_READ_ID};

/// Handle to a long-string blob (message bodies).
type TextHandle = Value<Handle<Blake3, LongString>>;

/// Format an Id as a lowercase hex string.
fn fmt_id_full(id: Id) -> String {
    format!("{id:x}")
}

/// First 8 hex chars of an Id — compact label for the sender/recipient
/// pills. v1 does not cross-reference the `relations` branch for
/// human-friendly names.
fn id_prefix(id: Id) -> String {
    let s = fmt_id_full(id);
    if s.len() > 8 {
        s[..8].to_string()
    } else {
        s
    }
}

// ── Color palette (reuses compass.rs conventions) ────────────────────

fn color_frame() -> egui::Color32 {
    // RAL 7016 anthracite grey — matches compass column frame.
    egui::Color32::from_rgb(0x29, 0x32, 0x36)
}

fn color_bubble() -> egui::Color32 {
    // Slightly lighter than the frame so message bubbles stand out on
    // top of the panel. Matches the compass "card_bg" shade.
    egui::Color32::from_rgb(0x33, 0x3b, 0x40)
}

fn color_muted() -> egui::Color32 {
    // RAL 7012 basalt grey — matches compass color_muted().
    egui::Color32::from_rgb(0x4d, 0x55, 0x59)
}

fn color_sender() -> egui::Color32 {
    // RAL 6032 signal green — matches playground diagnostics
    // `color_local_msg`: the accent for a message/from pill.
    egui::Color32::from_rgb(0x23, 0x7f, 0x52)
}

fn color_recipient() -> egui::Color32 {
    // Neutral recipient pill — caller-side metadata only; we don't
    // colour-code recipients uniquely in v1.
    egui::Color32::from_rgb(0x4a, 0x56, 0x5c)
}

fn color_read() -> egui::Color32 {
    // RAL 6017 may green — playground diagnostics used this for the
    // "read" accent. Keeping the same conceptual mapping here.
    egui::Color32::from_rgb(0x4a, 0x77, 0x29)
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

// ── Live messages connection ─────────────────────────────────────────

/// Opened pile + cached fact space + workspace for blob reads.
struct MessagesLive {
    space: TribleSet,
    ws: Workspace<Pile<Blake3>>,
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

        let bid = find_branch(&mut repo, branch_name)
            .ok_or_else(|| format!("no '{branch_name}' branch found"))?;
        let mut ws = repo
            .pull(bid)
            .map_err(|e| format!("pull {branch_name}: {e:?}"))?;
        let space = ws
            .checkout(..)
            .map_err(|e| format!("checkout {branch_name}: {e:?}"))?
            .into_facts();

        Ok(MessagesLive { space, ws })
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

    /// Collect every message with its from/to/body/created_at and fold
    /// in the read-receipt events that target it.
    fn messages(&mut self) -> Vec<MessageRow> {
        let mut by_id: HashMap<Id, MessageRow> = HashMap::new();

        // Message enumeration: every entity tagged KIND_MESSAGE_ID with
        // required attributes. from/to are GenId values; pattern!
        // unifies them as Id.
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

        // Read-receipt pairing: each receipt is a separate entity tagged
        // KIND_READ_ID, pointing at the message via about_message, with
        // a reader + read_at (NsTAIInterval). We fold the latest ts per
        // (message, reader) into the message row.
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

        // Stable read-order per row: newest first.
        for row in by_id.values_mut() {
            row.reads.sort_by(|a, b| b.1.cmp(&a.1));
        }

        by_id.into_values().collect()
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

fn format_age_key(now_key: i128, past_key: i128) -> String {
    format_age(now_key, Some(past_key))
}

// ── Widget ───────────────────────────────────────────────────────────

/// GORBIE-embeddable local-messages panel.
///
/// Reads append-only direct messages from a pile's local-messages
/// branch and renders them as a chat-style log (oldest-first). Each
/// row shows sender + recipient pills, the body, created-at age, and
/// a "read by X (ageY)" chip for every read receipt.
///
/// ```ignore
/// let mut panel = MessagesPanel::new("./self.pile", "local-messages");
/// // Inside a GORBIE card:
/// panel.render(ctx);
/// ```
pub struct MessagesPanel {
    pile_path: PathBuf,
    branch_name: String,
    // Wrapped in a Mutex so the widget is `Send + Sync`. GORBIE's state
    // storage requires that across threads, and `Workspace<Pile<Blake3>>`
    // uses interior-mutability types (Cell/RefCell) that aren't Sync.
    live: Option<Mutex<MessagesLive>>,
    error: Option<String>,
    viewport_height: f32,
    /// Tracks the first render so we can scroll to the bottom (newest)
    /// on initial paint.
    first_render: bool,
}

impl MessagesPanel {
    /// Build a panel pointing at a pile on disk and a named branch.
    /// The pile is not opened until the first [`render`](Self::render)
    /// call.
    pub fn new(pile_path: impl Into<PathBuf>, branch_name: impl Into<String>) -> Self {
        Self {
            pile_path: pile_path.into(),
            branch_name: branch_name.into(),
            live: None,
            error: None,
            viewport_height: 500.0,
            first_render: true,
        }
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
        // Chat-style order: oldest first so newest ends up at the
        // bottom of the scroll area.
        messages.sort_by(|a, b| {
            a.sort_key()
                .cmp(&b.sort_key())
                .then_with(|| a.id.cmp(&b.id))
        });

        drop(live);

        let now = now_tai_ns();
        let viewport_height = self.viewport_height;
        let branch_name = self.branch_name.clone();
        let scroll_to_bottom = self.first_render;
        self.first_render = false;

        ctx.section(&format!("Messages: {branch_name}"), |ctx| {
            ctx.label(format!("{} messages", messages.len()));

            let ui = ctx.ui_mut();
            if messages.is_empty() {
                ui.label("No messages yet.");
                return;
            }

            let mut scroll = egui::ScrollArea::vertical()
                .id_salt(("messages_panel", branch_name.as_str()))
                .max_height(viewport_height)
                .auto_shrink([false, false]);
            if scroll_to_bottom {
                scroll = scroll.vertical_scroll_offset(f32::MAX);
            }
            scroll.show(ui, |ui| {
                ui.set_width(ui.available_width());
                for msg in &messages {
                    render_message(ui, msg, now);
                    ui.add_space(6.0);
                }
            });
        });
    }
}

// ── Row rendering ────────────────────────────────────────────────────

fn render_message(ui: &mut egui::Ui, msg: &MessageRow, now: i128) {
    egui::Frame::NONE
        .fill(color_bubble())
        .stroke(egui::Stroke::new(1.0, color_frame()))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // Header row: from -> to, plus age.
            ui.horizontal(|ui| {
                render_chip(ui, &id_prefix(msg.from), color_sender());
                ui.label(egui::RichText::new("->").monospace().small().color(color_muted()));
                render_chip(ui, &id_prefix(msg.to), color_recipient());
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
                        let label = format!("read by {} ({})", id_prefix(*reader), format_age_key(now, *ts));
                        render_chip(ui, &label, color_read());
                    }
                });
            }

            // Short id footer — useful when correlating with faculty
            // CLI output that logs `[<hex>]` ids.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&id_prefix(msg.id))
                        .monospace()
                        .small()
                        .color(color_muted()),
                );
            });
        });
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
