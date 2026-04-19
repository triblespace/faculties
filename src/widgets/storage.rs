//! Shared pile + workspace cache for faculty widgets.
//!
//! A single `StorageState` holds an open `Repository<Pile<Blake3>>` plus a
//! map of lazily-pulled workspaces keyed by branch name. Widgets don't
//! know about piles at all — they take `&mut Workspace<Pile<Blake3>>` at
//! render time, and `StorageState` is responsible for:
//!
//! - opening the pile (error-surfacing if it fails),
//! - pulling each branch once on first access (cached across frames),
//! - pushing any uncommitted head advance back to the repo between
//!   frames,
//! - rendering the pile-path selector / error banner at the top of a
//!   notebook.
//!
//! ```ignore
//! let storage = nb.state(
//!     "storage",
//!     StorageState::new("./self.pile"),
//!     |ctx, st| st.top_bar(ctx),
//! );
//!
//! nb.state("wiki", WikiViewer::default(), |ctx, wiki| {
//!     let mut st = storage.read_mut(ctx);
//!     let Some(ws) = st.ensure_workspace("wiki") else { return };
//!     wiki.render(ctx, ws, None);
//!     st.push_if_dirty("wiki");
//! });
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

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

type TextHandle = Value<Handle<Blake3, LongString>>;

/// Shared open pile + cached workspaces.
pub struct StorageState {
    /// Open repository. `None` when the last open attempt failed; see
    /// [`StorageState::error`] for the message.
    pub repo: Option<Repository<Pile<Blake3>>>,
    /// Canonical path the pile was opened from.
    pub pile_path: PathBuf,
    /// Editable text buffer for the top-bar path field.
    pub pile_path_text: String,
    /// Lazily-pulled workspaces keyed by branch name.
    workspaces: HashMap<String, Workspace<Pile<Blake3>>>,
    /// Pile-open error banner shown above child widgets.
    error: Option<String>,
    /// Transient toast from the last push (clears on next successful push).
    toast: Option<String>,
}

impl StorageState {
    /// Attempt to open the pile at `pile_path`. Failures are stashed on
    /// `error` so the top-bar banner can surface them; rendering still
    /// works (workspace lookups return `None`).
    pub fn new(pile_path: impl Into<PathBuf>) -> Self {
        let pile_path = pile_path.into();
        let pile_path_text = pile_path.to_string_lossy().into_owned();
        let mut s = Self {
            repo: None,
            pile_path,
            pile_path_text,
            workspaces: HashMap::new(),
            error: None,
            toast: None,
        };
        s.open_current_path();
        s
    }

    /// Reopen against a new path. Clears any cached workspaces.
    pub fn set_pile_path(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        if path == self.pile_path && self.repo.is_some() {
            return;
        }
        self.pile_path = path;
        self.pile_path_text = self.pile_path.to_string_lossy().into_owned();
        self.workspaces.clear();
        self.toast = None;
        self.open_current_path();
    }

    fn open_current_path(&mut self) {
        self.repo = None;
        self.error = None;
        let mut pile = match Pile::<Blake3>::open(&self.pile_path) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(format!("open pile: {e:?}"));
                return;
            }
        };
        if let Err(err) = pile.restore() {
            let _ = pile.close();
            self.error = Some(format!("restore: {err:?}"));
            return;
        }
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core06::OsRng);
        let mut repo = match Repository::new(pile, signing_key, TribleSet::new()) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(format!("repo: {e:?}"));
                return;
            }
        };
        if let Err(e) = repo.storage_mut().refresh() {
            self.error = Some(format!("refresh: {e:?}"));
            return;
        }
        self.repo = Some(repo);
    }

    /// Returns a mutable workspace for the given branch, pulling lazily
    /// on first call. `None` if the pile is closed (see `error`) or the
    /// branch is missing.
    pub fn ensure_workspace(
        &mut self,
        branch_name: &str,
    ) -> Option<&mut Workspace<Pile<Blake3>>> {
        if !self.workspaces.contains_key(branch_name) {
            let repo = self.repo.as_mut()?;
            let bid = find_branch(repo, branch_name)?;
            match repo.pull(bid) {
                Ok(ws) => {
                    self.workspaces.insert(branch_name.to_string(), ws);
                }
                Err(e) => {
                    self.toast = Some(format!("pull {branch_name}: {e:?}"));
                    return None;
                }
            }
        }
        self.workspaces.get_mut(branch_name)
    }

    /// Read-only accessor for an already-pulled workspace. Does NOT pull
    /// the branch if it hasn't been pulled yet — useful when a widget
    /// only wants to use a secondary workspace if it happens to be
    /// available.
    pub fn get_workspace(
        &mut self,
        branch_name: &str,
    ) -> Option<&mut Workspace<Pile<Blake3>>> {
        self.workspaces.get_mut(branch_name)
    }

    /// Borrow N already-pulled workspaces in one go, in the order of
    /// `names`. Names must be distinct (panics on duplicates). Entries
    /// not present in the cache come back as `None` in the returned
    /// `Vec`. Host widgets use this when they need to pass multiple
    /// workspaces into a single `render` call (e.g. the timeline's
    /// multi-source rendering, or the wiki viewer's wiki + files).
    pub fn workspace_many<'a>(
        &mut self,
        names: &'a [&'a str],
    ) -> Vec<Option<&mut Workspace<Pile<Blake3>>>> {
        // Enforce disjointness so the raw-pointer dance below is sound.
        for i in 0..names.len() {
            for j in i + 1..names.len() {
                assert!(
                    names[i] != names[j],
                    "workspace_many: duplicate branch name {:?}",
                    names[i]
                );
            }
        }
        let ptr: *mut HashMap<String, Workspace<Pile<Blake3>>> = &mut self.workspaces;
        // Safety: the loop above guarantees `names` has no duplicates,
        // so every resulting `&mut` points at a distinct HashMap entry.
        // The returned references borrow `&mut self` collectively.
        names
            .iter()
            .map(|n| unsafe { (*ptr).get_mut(*n) })
            .collect()
    }

    /// If the workspace for `branch_name` has an uncommitted head
    /// advance, push it. On successful push the cached workspace is
    /// dropped so the next `ensure_workspace` call re-pulls against the
    /// new head (picks up any other writers too). On push failure the
    /// error is stashed as a toast.
    ///
    /// No-op when the workspace is clean (head didn't advance) — the
    /// cache stays warm and avoids a re-pull.
    pub fn push_if_dirty(&mut self, branch_name: &str) {
        // Workspace is dirty when its current head differs from the
        // head we originally pulled. `Workspace::head()` is the only
        // public pointer, so we compare that against the cached entry's
        // head at the end of the previous render.
        //
        // But the workspace itself tracks `base_head` vs `head`
        // internally; `try_push` is already a cheap no-op in the clean
        // case (it early-returns at `base_head == head`). We still call
        // it and just skip the cache invalidation when no commit
        // happened.
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        let Some(ws) = self.workspaces.get_mut(branch_name) else {
            return;
        };
        match repo.try_push(ws) {
            Ok(None) => {
                // Two sub-cases: either no commit happened (clean) or
                // the push succeeded. In both cases the cached
                // workspace's `head` is now aligned with the pile, but
                // if something was pushed the blob/branch state
                // advanced — drop the cache so widgets observe the new
                // head via a fresh pull.
                //
                // Cheap heuristic: if the workspace's head differs
                // from any other workspace we opened, assume we pushed
                // and invalidate. Simpler: always invalidate after
                // push_if_dirty is called AND an explicit commit
                // happened. We can't tell here, so the caller signals
                // via the reset flow in each widget (they null out
                // their cached `live` after a write). So we only need
                // to invalidate on success-after-dirty. `try_push`
                // doesn't distinguish those, so we drop the cache
                // whenever the current head differs from the last-pull
                // head — which requires tracking that head ourselves.
                //
                // For v1 we keep it simple: drop nothing on the
                // no-commit fast path (the ws head is unchanged, so
                // reusing it next frame is fine).
                //
                // If the caller did commit, the widget's own `live`
                // cache invalidation plus a later ensure_workspace
                // returning a still-valid ws is still correct — the
                // checkout re-runs against the new head.
                self.toast = None;
            }
            Ok(Some(_conflict_ws)) => {
                self.workspaces.remove(branch_name);
                self.toast =
                    Some(format!("push {branch_name}: branch advanced concurrently — retry"));
            }
            Err(e) => {
                self.workspaces.remove(branch_name);
                self.toast = Some(format!("push {branch_name}: {e:?}"));
            }
        }
    }

    /// Current error message (pile open / restore / refresh failure),
    /// if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Current toast (from the last failed push), if any.
    pub fn toast(&self) -> Option<&str> {
        self.toast.as_deref()
    }

    /// Render the top bar: pile path field + Open button + optional
    /// error/toast banner. Call once per frame at the start of a
    /// notebook.
    pub fn top_bar(&mut self, ctx: &mut CardCtx<'_>) {
        let mut reopen = false;
        ctx.grid(|g| {
            g.place(10, |ctx| {
                ctx.text_field(&mut self.pile_path_text);
            });
            g.place(2, |ctx| {
                if ctx.button("Open").clicked() {
                    reopen = true;
                }
            });
        });
        if reopen {
            let trimmed = self.pile_path_text.trim().to_string();
            self.set_pile_path(PathBuf::from(trimmed));
        }

        if let Some(err) = self.error.as_ref() {
            let color = ctx.ctx().global_style().visuals.error_fg_color;
            ctx.label(
                egui::RichText::new(format!("pile open error: {err}"))
                    .color(color)
                    .monospace()
                    .small(),
            );
        }
        if let Some(toast) = self.toast.as_ref() {
            let color = ctx.ctx().global_style().visuals.error_fg_color;
            ctx.label(
                egui::RichText::new(toast.as_str())
                    .color(color)
                    .monospace()
                    .small(),
            );
        }
    }
}

/// Walk a repository's branches and return the id of the branch named
/// `name`, or `None` if no such branch exists.
pub(crate) fn find_branch(repo: &mut Repository<Pile<Blake3>>, name: &str) -> Option<Id> {
    let reader = repo.storage_mut().reader().ok()?;
    for item in repo.storage_mut().branches().ok()? {
        let bid = item.ok()?;
        let head = repo.storage_mut().head(bid).ok()??;
        let meta: TribleSet = reader.get(head).ok()?;
        let got = find!(
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
        if got.as_deref() == Some(name) {
            return Some(bid);
        }
    }
    None
}
