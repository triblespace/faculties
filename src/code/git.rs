//! Local Git reads through libgit2; no executable, hooks, or credential helpers.
//!
//! The index and object database remain the authority for tracked paths and
//! revisions. Object contents are delivered one at a time, not copied into a
//! second repository catalogue. Git object IDs are provenance, never pile IDs.
//!
//! History uses literal, non-overlapping `-S` occurrence counts on native
//! tree-diff pairs, including binary blobs, after native rename detection.
//! Like ordinary `git log -S`, it walks HEAD and omits merge diffs. Libgit2's
//! similarity heuristic can pair ambiguous/inexact renames differently from
//! Git. External textconv/filter programs and custom diff drivers are never
//! executed: history searches stored bytes, not a program's transformed text.
//! History order is libgit2's topological/time order; clock-skewed merge graphs
//! and timestamp ties can differ from Git's default traversal order.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use git2::{Diff, DiffFindOptions, ErrorCode, ObjectType, Oid, Repository, RepositoryOpenFlags};

/// One blob named by a tree listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
    pub path: String,
    pub object: String,
}

/// One commit a pickaxe search found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Change {
    pub commit: String,
    pub date: String,
    pub subject: String,
}

fn repository(dir: &Path) -> Result<Repository> {
    // Do not discover a parent checkout or honor ambient GIT_DIR in place of
    // the directory the caller supplied.
    Repository::open_ext(
        dir,
        RepositoryOpenFlags::NO_SEARCH,
        std::iter::empty::<&Path>(),
    )
    .with_context(|| format!("open Git repository {}", dir.display()))
}

/// Whether `dir` is the root of a Git working copy (including linked worktrees).
pub fn is_repository(dir: &Path) -> bool {
    dir.join(".git").exists() && repository(dir).is_ok_and(|repo| !repo.is_bare())
}

/// The revision a repository's HEAD currently names.
pub fn head(dir: &Path) -> Result<String> {
    let repo = repository(dir)?;
    let id = repo.head()?.peel_to_commit()?.id();
    Ok(id.to_string())
}

/// Resolve a revision, including abbreviated IDs and annotated tags, to a commit.
pub fn resolve(dir: &Path, revision: &str) -> Result<String> {
    let repo = repository(dir)?;
    let id = repo.revparse_single(revision)?.peel_to_commit()?.id();
    Ok(id.to_string())
}

/// Whether the index or working tree differs from HEAD in a tracked path.
pub fn is_dirty(dir: &Path) -> Result<bool> {
    let repo = repository(dir)?;
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(false)
        .include_ignored(false)
        .include_unmodified(false)
        .update_index(false);
    let statuses = repo.statuses(Some(&mut options))?;
    for entry in statuses.iter() {
        if entry.status() == git2::Status::WT_MODIFIED {
            if let Some(delta) = entry.index_to_workdir() {
                if delta.old_file().mode() == git2::FileMode::Commit
                    && delta.new_file().mode() == git2::FileMode::Commit
                {
                    // INCLUDE_UNTRACKED=false does not propagate into
                    // libgit2's submodule scan. Git status -uno does: recheck
                    // only this otherwise-dirty gitlink with that policy.
                    // Preserve stronger per-submodule ignore policies;
                    // staged changes still count independently.
                    let path = delta.new_file().path().context("Git submodule path")?;
                    let name = path.to_str().context("non-UTF-8 Git submodule path")?;
                    let module = repo.find_submodule(name)?;
                    // libgit2 exposes the .gitmodules rule, but does not
                    // apply Git's local-config override for this accessor.
                    let key = format!("submodule.{}.ignore", module.name()?);
                    let configured = match repo.config()?.get_string(&key) {
                        Ok(value) => match value.as_str() {
                            "none" => git2::SubmoduleIgnore::None,
                            "untracked" => git2::SubmoduleIgnore::Untracked,
                            "dirty" => git2::SubmoduleIgnore::Dirty,
                            "all" => git2::SubmoduleIgnore::All,
                            _ => bail!("invalid {key}: {value}"),
                        },
                        Err(error) if error.code() == ErrorCode::NotFound => module.ignore_rule(),
                        Err(error) => return Err(error.into()),
                    };
                    let ignore = match configured {
                        git2::SubmoduleIgnore::All => git2::SubmoduleIgnore::All,
                        git2::SubmoduleIgnore::Dirty => git2::SubmoduleIgnore::Dirty,
                        _ => git2::SubmoduleIgnore::Untracked,
                    };
                    let status = repo.submodule_status(name, ignore)?;
                    let dirty = git2::SubmoduleStatus::WD_ADDED
                        | git2::SubmoduleStatus::WD_DELETED
                        | git2::SubmoduleStatus::WD_MODIFIED
                        | git2::SubmoduleStatus::WD_INDEX_MODIFIED
                        | git2::SubmoduleStatus::WD_WD_MODIFIED;
                    if !status.intersects(dirty) {
                        continue;
                    }
                }
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// Every index path, unfiltered, including symlinks, gitlinks and conflict stages.
pub fn ls_files(dir: &Path) -> Result<Vec<String>> {
    let repo = repository(dir)?;
    let index = repo.index()?;
    Ok(index
        .iter()
        .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
        .collect())
}

/// Every path and blob object at one revision; gitlinks are not blobs.
pub fn ls_tree(dir: &Path, revision: &str) -> Result<Vec<TreeEntry>> {
    fn walk(
        repo: &Repository,
        tree: &git2::Tree<'_>,
        prefix: &mut Vec<u8>,
        out: &mut Vec<TreeEntry>,
    ) -> Result<()> {
        for entry in tree {
            let prefix_len = prefix.len();
            prefix.extend_from_slice(entry.name_bytes());
            match entry.kind() {
                Some(ObjectType::Blob) => out.push(TreeEntry {
                    path: String::from_utf8_lossy(prefix).into_owned(),
                    object: entry.id().to_string(),
                }),
                Some(ObjectType::Tree) => {
                    prefix.push(b'/');
                    walk(repo, &repo.find_tree(entry.id())?, prefix, out)?;
                }
                _ => {}
            }
            prefix.truncate(prefix_len);
        }
        Ok(())
    }
    let repo = repository(dir)?;
    let tree = repo.revparse_single(revision)?.peel_to_tree()?;
    let mut entries = Vec::new();
    walk(&repo, &tree, &mut Vec::new(), &mut entries)?;
    Ok(entries)
}

/// Deliver each requested object once, in order, reading one object at a time
/// (the callback owns its Vec). Missing objects are skipped, as with cat-file batch;
/// malformed IDs and corrupt objects are errors. A callback error stops reads.
pub fn cat_objects(
    dir: &Path,
    objects: &[String],
    mut receive: impl FnMut(&str, Vec<u8>) -> Result<()>,
) -> Result<()> {
    if objects.is_empty() {
        return Ok(());
    }
    let repo = repository(dir)?;
    let odb = repo.odb()?;
    for object in objects {
        let id = Oid::from_str(object).with_context(|| format!("Git object ID {object}"))?;
        let payload = match odb.read(id) {
            Ok(payload) => payload,
            Err(error) if error.code() == ErrorCode::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read Git object {id}")),
        };
        receive(&id.to_string(), payload.data().to_vec())?;
    }
    Ok(())
}

fn changes<'repo>(
    repo: &'repo Repository,
    commit: &git2::Commit<'repo>,
    parent: Option<usize>,
) -> Result<Diff<'repo>> {
    let tree = commit.tree()?;
    let old = parent
        .map(|index| commit.parent(index)?.tree())
        .transpose()?;
    let mut options = git2::DiffOptions::new();
    options.include_typechange(true);
    let mut diff = repo.diff_tree_to_tree(old.as_ref(), Some(&tree), Some(&mut options))?;
    // Git defaults to rename detection on; libgit2's config defaults differ.
    let config = repo.config()?;
    let renames = match config.get_string("diff.renames") {
        Ok(value) => value,
        Err(error) if error.code() == ErrorCode::NotFound => "true".to_owned(),
        Err(error) => return Err(error.into()),
    };
    let copies = renames.eq_ignore_ascii_case("copies");
    let enabled = copies
        || config.get_bool("diff.renames").or_else(|error| {
            if error.code() == ErrorCode::NotFound {
                Ok(true)
            } else {
                Err(error)
            }
        })?;
    if enabled {
        let mut find = DiffFindOptions::new();
        find.renames(true).copies(copies);
        // Git's default is 1000; zero requests an unlimited search.
        let limit = match config.get_i64("diff.renameLimit") {
            Ok(value) if value >= 0 => usize::try_from(value)?,
            Ok(_) => bail!("diff.renameLimit must not be negative"),
            Err(error) if error.code() == ErrorCode::NotFound => 1000,
            Err(error) => return Err(error.into()),
        };
        find.rename_limit(if limit == 0 { usize::MAX } else { limit });
        diff.find_similar(Some(&mut find))?;
    }
    Ok(diff)
}

fn occurrences(mut bytes: &[u8], needle: &[u8]) -> usize {
    let mut count = 0;
    while let Some(offset) = memchr::memmem::find(bytes, needle) {
        count += 1;
        bytes = &bytes[offset + needle.len()..];
    }
    count
}

fn file_occurrences(repo: &Repository, file: git2::DiffFile<'_>, needle: &[u8]) -> Result<usize> {
    if file.id().is_zero() {
        return Ok(0);
    }
    if file.mode() == git2::FileMode::Commit {
        // Gitlink diff payload; its commit need not exist in the parent's ODB.
        return Ok(occurrences(
            format!("Subproject commit {}\n", file.id()).as_bytes(),
            needle,
        ));
    }
    let blob = repo.find_blob(file.id())?;
    Ok(occurrences(blob.content(), needle))
}

/// Commits whose native diff changes a literal identifier's occurrence count.
/// Counts are per file pair, not across the tree: moving an occurrence between
/// files is a hit; moving it within one file is not. Pure renames are not hits
/// when rename detection is enabled.
pub fn pickaxe(dir: &Path, identifier: &str, limit: usize) -> Result<Vec<Change>> {
    if identifier.is_empty() || identifier.contains('\0') {
        bail!("Git pickaxe identifier must be nonempty and contain no NUL");
    }
    let repo = repository(dir)?;
    let mut walk = repo.revwalk()?;
    // Time alone globally sorts even a parent newer than its child ahead of
    // HEAD. Keep descendants first, including repositories with clock skew.
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;
    walk.push_head()?;
    let mut result = Vec::new();
    if limit == 0 {
        return Ok(result);
    }
    for id in walk {
        let commit = repo.find_commit(id?)?;
        // Default log -S has no merge diff; still walk both parent histories.
        if commit.parent_count() > 1 {
            continue;
        }
        let diff = changes(&repo, &commit, (commit.parent_count() == 1).then_some(0))?;
        let mut hit = false;
        for delta in diff.deltas() {
            if delta.old_file().id() == delta.new_file().id() {
                continue;
            }
            if file_occurrences(&repo, delta.old_file(), identifier.as_bytes())?
                != file_occurrences(&repo, delta.new_file(), identifier.as_bytes())?
            {
                hit = true;
                break;
            }
        }
        if !hit {
            continue;
        }
        let when = commit.author().when();
        let zone = chrono::FixedOffset::east_opt(when.offset_minutes() * 60)
            .context("invalid Git author timezone")?;
        let date = chrono::DateTime::from_timestamp(when.seconds(), 0)
            .context("Git author date out of range")?
            .with_timezone(&zone)
            .format("%Y-%m-%d")
            .to_string();
        result.push(Change {
            commit: commit.id().to_string(),
            date,
            subject: String::from_utf8_lossy(commit.summary_bytes().unwrap_or_default())
                .into_owned(),
        });
        if result.len() == limit {
            break;
        }
    }
    Ok(result)
}

/// Paths a commit touched, for explaining a pickaxe hit. A merge reports paths
/// changed against every parent (combined-diff semantics). Names are returned
/// literally, without Git display quoting or trimming significant whitespace.
pub fn commit_paths(dir: &Path, revision: &str, limit: usize) -> Result<Vec<String>> {
    let repo = repository(dir)?;
    let commit = repo.revparse_single(revision)?.peel_to_commit()?;
    let mut paths: Option<BTreeSet<Vec<u8>>> = None;
    for index in 0..commit.parent_count().max(1) {
        let diff = changes(
            &repo,
            &commit,
            (commit.parent_count() != 0).then_some(index),
        )?;
        let changed: BTreeSet<_> = diff
            .deltas()
            .filter_map(|delta| {
                delta
                    .new_file()
                    .path_bytes()
                    .or(delta.old_file().path_bytes())
                    .map(Vec::from)
            })
            .collect();
        paths = Some(match paths {
            None => changed,
            Some(previous) => previous.intersection(&changed).cloned().collect(),
        });
    }
    Ok(paths
        .into_iter()
        .flatten()
        .take(limit)
        .map(|path| String::from_utf8_lossy(&path).into_owned())
        .collect())
}

#[cfg(test)]
#[path = "git/tests.rs"]
mod tests;
