//! Shared pile + repository handle so that multiple faculty widgets can
//! coexist over a single `Pile<Blake3>` instead of each one opening its
//! own.
//!
//! Before this, every widget's internal `*Live` struct held its own
//! `Repository` and the `PileInspector` composition opened the same pile
//! file four times on first render (and closed it four times on every
//! pile-path change). With `SharedPile` a `PileInspector` opens once,
//! hands out cheap `clone()`s to each child, and each widget pulls its
//! own `Workspace` for the branch it cares about.
//!
//! The Repository is behind a `parking_lot::Mutex` — pulls and pushes
//! take `&mut Repository`, and keeping them cheap by not re-opening the
//! pile is the whole point.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use triblespace::core::blob::schemas::simplearchive::SimpleArchive;
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

struct SharedPileInner {
    path: PathBuf,
    repo: Repository<Pile<Blake3>>,
    /// Cached branch-name → branch-id lookups. Populated on demand.
    branch_ids: HashMap<String, Id>,
}

/// Cheaply-cloneable handle to a single open pile + its `Repository`.
///
/// The underlying `Repository<Pile<Blake3>>` is held behind an `Arc<Mutex<_>>`
/// — all callers share the same open file. Dropping the last clone closes
/// the pile.
#[derive(Clone)]
pub struct SharedPile {
    inner: Arc<Mutex<SharedPileInner>>,
}

impl SharedPile {
    /// Open a pile file on disk. Returns a handle that can be cheaply
    /// cloned to multiple widgets.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let mut pile = Pile::<Blake3>::open(&path).map_err(|e| format!("open pile: {e:?}"))?;
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

        Ok(Self {
            inner: Arc::new(Mutex::new(SharedPileInner {
                path,
                repo,
                branch_ids: HashMap::new(),
            })),
        })
    }

    /// Path this pile was opened from.
    pub fn path(&self) -> PathBuf {
        self.inner.lock().path.clone()
    }

    /// Resolve a branch name to its id, looking it up on first call and
    /// caching on subsequent calls. `None` if no branch with that name
    /// exists on the pile.
    pub fn branch_id(&self, name: &str) -> Option<Id> {
        let mut g = self.inner.lock();
        if let Some(&id) = g.branch_ids.get(name) {
            return Some(id);
        }
        let id = find_branch_by_name(&mut g.repo, name)?;
        g.branch_ids.insert(name.to_string(), id);
        Some(id)
    }

    /// Pull a fresh `Workspace` for the given branch.
    pub fn pull_branch(&self, name: &str) -> Result<Workspace<Pile<Blake3>>, String> {
        let bid = self
            .branch_id(name)
            .ok_or_else(|| format!("no '{name}' branch found"))?;
        let mut g = self.inner.lock();
        g.repo.pull(bid).map_err(|e| format!("pull {name}: {e:?}"))
    }

    /// Push a workspace back to the shared repository. Returns
    /// `Ok(None)` on a successful push, `Ok(Some(conflict_ws))` if the
    /// branch head advanced concurrently and the caller needs to merge,
    /// and `Err(..)` for a real storage failure.
    pub fn try_push(
        &self,
        ws: &mut Workspace<Pile<Blake3>>,
    ) -> Result<Option<Workspace<Pile<Blake3>>>, String> {
        let mut g = self.inner.lock();
        g.repo.try_push(ws).map_err(|e| format!("push: {e:?}"))
    }

    /// Iterate commit handles of a branch walking `ancestors` from head.
    /// Used by the timeline widget to enumerate events without having to
    /// hold its own repo handle.
    pub fn with_repo<R>(&self, f: impl FnOnce(&mut Repository<Pile<Blake3>>) -> R) -> R {
        let mut g = self.inner.lock();
        f(&mut g.repo)
    }
}

/// Internal helper that mirrors each widget's formerly-duplicated
/// `find_branch` copy.
fn find_branch_by_name(repo: &mut Repository<Pile<Blake3>>, name: &str) -> Option<Id> {
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

/// Unused right now but keeps the `SimpleArchive` import honest — the
/// timeline widget needs to `get::<TribleSet, SimpleArchive>(commit_handle)`
/// against a reader pulled from the shared repo.
#[allow(dead_code)]
pub type CommitHandle = Value<Handle<Blake3, SimpleArchive>>;
