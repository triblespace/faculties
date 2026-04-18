//! Minimal GORBIE-embeddable wiki viewer.
//!
//! Renders wiki fragments from a triblespace pile. Shows a list of
//! fragments (sorted by title) in one section and the selected
//! fragment's content (rendered as Typst) in another.
//!
//! Scope is intentionally tight: v1 is fragment-only. It does not
//! resolve `files:` links, render a force-directed graph, or handle
//! link navigation.

use std::collections::BTreeMap;
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

use crate::schemas::wiki::{attrs as wiki, KIND_VERSION_ID, TAG_ARCHIVED_ID, WIKI_BRANCH_NAME};

/// Handle to a long-string blob living in a pile.
type TextHandle = Value<Handle<Blake3, LongString>>;

/// Format an Id as a lowercase hex string.
fn fmt_id(id: Id) -> String {
    format!("{id:x}")
}

// ── live wiki connection ─────────────────────────────────────────────

/// Opened pile + cached wiki fact space + workspace for blob reads.
struct WikiLive {
    wiki_space: TribleSet,
    wiki_ws: Workspace<Pile<Blake3>>,
}

impl WikiLive {
    fn open(path: &Path) -> Result<Self, String> {
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

        let wiki_bid = find_branch(&mut repo, WIKI_BRANCH_NAME)
            .ok_or_else(|| format!("no '{WIKI_BRANCH_NAME}' branch found"))?;
        let mut wiki_ws = repo
            .pull(wiki_bid)
            .map_err(|e| format!("pull wiki: {e:?}"))?;
        let wiki_space = wiki_ws
            .checkout(..)
            .map_err(|e| format!("checkout wiki: {e:?}"))?
            .into_facts();

        Ok(WikiLive {
            wiki_space,
            wiki_ws,
        })
    }

    fn text(&mut self, h: TextHandle) -> String {
        self.wiki_ws
            .get::<View<str>, LongString>(h)
            .map(|v| {
                let s: &str = v.as_ref();
                s.to_string()
            })
            .unwrap_or_default()
    }

    fn title(&mut self, vid: Id) -> String {
        find!(h: TextHandle, pattern!(&self.wiki_space, [{ vid @ wiki::title: ?h }]))
            .next()
            .map(|h| self.text(h))
            .unwrap_or_default()
    }

    fn content(&mut self, vid: Id) -> String {
        find!(h: TextHandle, pattern!(&self.wiki_space, [{ vid @ wiki::content: ?h }]))
            .next()
            .map(|h| self.text(h))
            .unwrap_or_default()
    }

    fn tags(&self, vid: Id) -> Vec<Id> {
        find!(tag: Id, pattern!(&self.wiki_space, [{ vid @ metadata::tag: ?tag }]))
            .filter(|t| *t != KIND_VERSION_ID)
            .collect()
    }

    fn is_archived(&self, vid: Id) -> bool {
        self.tags(vid).contains(&TAG_ARCHIVED_ID)
    }

    /// Latest non-archived (fragment_id, version_id) pairs sorted by title.
    fn fragments_sorted(&mut self) -> Vec<(Id, Id)> {
        let mut latest: BTreeMap<Id, (Id, i128)> = BTreeMap::new();
        for (vid, frag, ts) in find!(
            (vid: Id, frag: Id, ts: (i128, i128)),
            pattern!(&self.wiki_space, [{
                ?vid @
                metadata::tag: &KIND_VERSION_ID,
                wiki::fragment: ?frag,
                metadata::created_at: ?ts,
            }])
        ) {
            let replace = match latest.get(&frag) {
                None => true,
                Some((_, prev_key)) => ts.0 > *prev_key,
            };
            if replace {
                latest.insert(frag, (vid, ts.0));
            }
        }
        let mut entries: Vec<(Id, Id)> = latest
            .into_iter()
            .map(|(frag, (vid, _))| (frag, vid))
            .filter(|(_, vid)| !self.is_archived(*vid))
            .collect();
        entries.sort_by(|a, b| {
            self.title(a.1)
                .to_lowercase()
                .cmp(&self.title(b.1).to_lowercase())
        });
        entries
    }

    /// Find the latest version id for a given fragment id.
    fn latest_version(&self, fragment_id: Id) -> Option<Id> {
        find!(
            (vid: Id, ts: (i128, i128)),
            pattern!(&self.wiki_space, [{
                ?vid @
                metadata::tag: &KIND_VERSION_ID,
                wiki::fragment: &fragment_id,
                metadata::created_at: ?ts,
            }])
        )
        .max_by_key(|(_, ts)| ts.0)
        .map(|(vid, _)| vid)
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

// ── widget ───────────────────────────────────────────────────────────

/// GORBIE-embeddable wiki fragment viewer.
///
/// Opens a triblespace pile lazily on first render. Shows two
/// sections: a list of fragments (click to select) and the selected
/// fragment's Typst-rendered content.
///
/// ```ignore
/// let mut viewer = WikiViewer::new("./self.pile");
/// // Inside a GORBIE card:
/// viewer.render(ctx);
/// ```
pub struct WikiViewer {
    pile_path: PathBuf,
    // Wrapped in a Mutex so the widget is `Send + Sync` — GORBIE's
    // `NotebookCtx::state` requires that for cross-thread state storage.
    // `WikiLive` holds a `Workspace<Pile<Blake3>>` which uses `Cell`
    // internally and is not `Sync`.
    live: Option<Mutex<WikiLive>>,
    selected_fragment: Option<Id>,
    error: Option<String>,
}

impl WikiViewer {
    /// Build a viewer pointing at a pile on disk. The pile is not opened
    /// until the first [`render`](Self::render) call.
    pub fn new(pile_path: impl Into<PathBuf>) -> Self {
        Self {
            pile_path: pile_path.into(),
            live: None,
            selected_fragment: None,
            error: None,
        }
    }

    /// Render the viewer into a GORBIE card context.
    pub fn render(&mut self, ctx: &mut CardCtx<'_>) {
        // Lazy pile open on first render.
        if self.live.is_none() && self.error.is_none() {
            match WikiLive::open(&self.pile_path) {
                Ok(live) => self.live = Some(Mutex::new(live)),
                Err(e) => self.error = Some(e),
            }
        }

        if let Some(err) = &self.error {
            ctx.label(format!("wiki viewer error: {err}"));
            return;
        }

        let Some(live_lock) = self.live.as_ref() else {
            // Shouldn't reach here — either live or error is set.
            ctx.label("wiki viewer not initialized");
            return;
        };
        let mut live = live_lock.lock();

        // Pre-compute all the data we need up front so the UI closures
        // don't have to juggle dual mutable borrows of `self`.
        let entries = live.fragments_sorted();

        // Auto-select the first fragment on first render, so the content
        // section has something to show.
        if self.selected_fragment.is_none() {
            if let Some((frag, _)) = entries.first() {
                self.selected_fragment = Some(*frag);
            }
        }

        // Resolve titles for the list once — keeps the UI closure free
        // of mutable queries against `live`.
        let listed: Vec<(Id, String)> = entries
            .iter()
            .map(|(frag, vid)| {
                let title = live.title(*vid);
                let label = if title.is_empty() {
                    fmt_id(*frag)
                } else {
                    format!("{title}  ({})", &fmt_id(*frag)[..8])
                };
                (*frag, label)
            })
            .collect();

        let selected_content = self
            .selected_fragment
            .and_then(|frag| live.latest_version(frag))
            .map(|vid| (live.title(vid), live.content(vid)));

        drop(live);

        let selected_fragment = &mut self.selected_fragment;

        // ── Fragments list ───────────────────────────────────────────
        ctx.section("Fragments", |ctx| {
            egui::ScrollArea::vertical()
                .max_height(400.0)
                .auto_shrink([false, false])
                .show(ctx.ui_mut(), |ui| {
                    for (frag, label) in &listed {
                        let selected = *selected_fragment == Some(*frag);
                        if ui.selectable_label(selected, label).clicked() {
                            *selected_fragment = Some(*frag);
                        }
                    }
                });
        });

        // ── Selected fragment content ────────────────────────────────
        ctx.section("Content", |ctx| {
            match selected_content {
                Some((title, content)) if !content.is_empty() => {
                    if !title.is_empty() {
                        ctx.label(format!("# {title}"));
                    }
                    ctx.typst(&content);
                }
                Some((title, _)) => {
                    if !title.is_empty() {
                        ctx.label(format!("# {title}"));
                    }
                    ctx.label("(no content)");
                }
                None => {
                    ctx.label("select a fragment");
                }
            }
        });
    }
}
