//! Shared pile state for faculty widgets.
//!
//! A single `StorageState` holds an open `Repository<Pile<Blake3>>` and
//! renders the top-bar pile-path selector / error banner. Widgets pull a
//! fresh [`Workspace`] each frame via [`StorageState::workspace`]; if the
//! callsite mutates the workspace, it pushes back with
//! [`StorageState::push`]. Nothing is cached across frames — the storage
//! itself (pile + repo) is the only shared state.
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
//!     let Some(mut ws) = st.workspace("wiki") else { return };
//!     wiki.render(ctx, &mut ws, None);
//!     st.push(&mut ws); // no-op if head didn't advance
//! });
//! ```

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

/// Shared open pile + repository. Workspaces are pulled fresh per call.
pub struct StorageState {
    /// Open repository. `None` when the last open attempt failed; see
    /// [`StorageState::error`] for the message.
    repo: Option<Repository<Pile<Blake3>>>,
    /// Canonical path the pile was opened from.
    pile_path: PathBuf,
    /// Editable text buffer for the top-bar path field.
    pile_path_text: String,
    /// Pile-open error banner shown above child widgets.
    error: Option<String>,
    /// Transient toast from the last push (clears on next successful push).
    toast: Option<String>,
}

impl StorageState {
    /// Stash the pile path for lazy open. No I/O happens here — the
    /// pile is opened on the first call that needs it (`workspace`,
    /// `top_bar`, `push`).
    ///
    /// This matters because `GORBIE::NotebookCtx::state` takes its
    /// initial value eagerly: it's constructed every frame and
    /// discarded when state already exists. A heavy constructor
    /// (opening a pile) every frame would spam pile-not-closed warnings
    /// and churn mmap handles.
    pub fn new(pile_path: impl Into<PathBuf>) -> Self {
        let pile_path = pile_path.into();
        let pile_path_text = pile_path.to_string_lossy().into_owned();
        Self {
            repo: None,
            pile_path,
            pile_path_text,
            error: None,
            toast: None,
        }
    }

    /// Open the pile at `self.pile_path` if it isn't already open.
    /// No-op if the repo is already open or the last open attempt
    /// failed (see `error`).
    fn ensure_open(&mut self) {
        if self.repo.is_some() || self.error.is_some() {
            return;
        }
        self.open_current_path();
    }

    /// Reopen against a new path.
    pub fn set_pile_path(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        if path == self.pile_path && self.repo.is_some() {
            return;
        }
        self.pile_path = path;
        self.pile_path_text = self.pile_path.to_string_lossy().into_owned();
        self.toast = None;
        self.open_current_path();
    }

    fn open_current_path(&mut self) {
        // Cleanly close the existing repo before replacing it, so the
        // old pile's drop path doesn't emit a "dropped without close()"
        // warning.
        if let Some(repo) = self.repo.take() {
            let _ = repo.close();
        }
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

    /// Pull a fresh workspace for `branch`. `None` if no repo is open or
    /// the branch is missing. Each call is a fresh pull — workspaces are
    /// NOT cached across frames.
    pub fn workspace(&mut self, branch: &str) -> Option<Workspace<Pile<Blake3>>> {
        self.ensure_open();
        let repo = self.repo.as_mut()?;
        let bid = find_branch(repo, branch)?;
        match repo.pull(bid) {
            Ok(ws) => Some(ws),
            Err(e) => {
                self.toast = Some(format!("pull {branch}: {e:?}"));
                None
            }
        }
    }

    /// Push `ws` back. Internally a no-op when the workspace's head
    /// didn't advance (the underlying `Repository::push` short-circuits
    /// on `base_head == head`), so callers can invoke this
    /// unconditionally. On CAS failure / storage error, stores a toast.
    /// Uses `Repository::push` which handles merge-retry internally.
    pub fn push(&mut self, ws: &mut Workspace<Pile<Blake3>>) {
        self.ensure_open();
        let Some(repo) = self.repo.as_mut() else {
            return;
        };
        match repo.push(ws) {
            Ok(()) => {
                self.toast = None;
            }
            Err(e) => {
                self.toast = Some(format!("push: {e:?}"));
            }
        }
    }

    /// Current error message (pile open / restore / refresh failure),
    /// if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Current toast (from the last failed push / pull), if any.
    pub fn toast(&self) -> Option<&str> {
        self.toast.as_deref()
    }

    /// Render the top bar: pile path field + Open button + optional
    /// error/toast banner. Call once per frame at the start of a
    /// notebook.
    pub fn top_bar(&mut self, ctx: &mut CardCtx<'_>) {
        self.ensure_open();
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

    /// Cleanly close the underlying pile. Called automatically on drop
    /// but exposed so callers can surface close failures.
    pub fn close(&mut self) -> Result<(), String> {
        if let Some(repo) = self.repo.take() {
            repo.close()
                .map_err(|e| format!("close pile: {e:?}"))?;
        }
        Ok(())
    }
}

impl Drop for StorageState {
    fn drop(&mut self) {
        // Take the repo out so `close` can consume it. Swallow errors on
        // drop — nothing to do with them here; callers who care should
        // call `close()` explicitly before dropping.
        let _ = self.close();
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

// Compile-time check that `StorageState` is `Send + Sync`. Required for
// GORBIE 0.11.1's RwLock-backed state store.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<StorageState>();
};
