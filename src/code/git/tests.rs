use super::*;
use git2::{Index, IndexEntry, IndexTime, Signature, Time};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    repo: Repository,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut options = git2::RepositoryInitOptions::new();
        options.external_template(false).initial_head("main");
        let repo = Repository::init_opts(dir.path(), &options).unwrap();
        repo.config()
            .unwrap()
            .set_str("diff.renames", "true")
            .unwrap();
        repo.config()
            .unwrap()
            .set_bool("core.autocrlf", false)
            .unwrap();
        Self { dir, repo }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn commit(
        &self,
        files: &[(&str, &[u8], u32)],
        parents: &[Oid],
        seconds: i64,
        message: &str,
    ) -> Oid {
        let mut index = Index::new().unwrap();
        for (path, bytes, mode) in files {
            let id = if *mode == 0o160000 {
                Oid::from_str(std::str::from_utf8(bytes).unwrap()).unwrap()
            } else {
                self.repo.blob(bytes).unwrap()
            };
            index.add(&entry(path.as_bytes(), id, *mode)).unwrap();
        }
        let tree_id = index.write_tree_to(&self.repo).unwrap();
        let tree = self.repo.find_tree(tree_id).unwrap();
        let signature = Signature::new(
            "Fixture",
            "fixture@example.invalid",
            &Time::new(seconds, -60),
        )
        .unwrap();
        let parents: Vec<_> = parents
            .iter()
            .map(|id| self.repo.find_commit(*id).unwrap())
            .collect();
        let refs: Vec<_> = parents.iter().collect();
        let id = self
            .repo
            .commit(None, &signature, &signature, message, &tree, &refs)
            .unwrap();
        self.repo
            .reference("refs/heads/main", id, true, "fixture")
            .unwrap();
        id
    }
}

fn entry(path: &[u8], id: Oid, mode: u32) -> IndexEntry {
    IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        file_size: 0,
        id,
        flags: 0,
        flags_extended: 0,
        path: path.to_vec(),
    }
}

#[test]
fn revisions_tags_detached_head_and_repository_boundary() {
    let fixture = Fixture::new();
    assert!(is_repository(fixture.path()));
    assert!(head(fixture.path()).is_err());
    let id = fixture.commit(
        &[("src/lib.rs", b"fn first() {}", 0o100644)],
        &[],
        3600,
        "first",
    );
    assert_eq!(head(fixture.path()).unwrap(), id.to_string());
    assert_eq!(
        resolve(fixture.path(), &id.to_string()[..12]).unwrap(),
        id.to_string()
    );
    let sig = Signature::new("Fixture", "fixture@example.invalid", &Time::new(3600, 0)).unwrap();
    fixture
        .repo
        .tag(
            "version",
            fixture.repo.find_commit(id).unwrap().as_object(),
            &sig,
            "version",
            false,
        )
        .unwrap();
    assert_eq!(resolve(fixture.path(), "version").unwrap(), id.to_string());
    fixture.repo.set_head_detached(id).unwrap();
    assert_eq!(head(fixture.path()).unwrap(), id.to_string());
    let blob = fixture.repo.blob(b"not a commit").unwrap();
    assert!(resolve(fixture.path(), &blob.to_string()).is_err());
    std::fs::create_dir(fixture.path().join("nested")).unwrap();
    assert!(!is_repository(&fixture.path().join("nested")));
    assert!(head(&fixture.path().join("nested")).is_err());
}

#[test]
fn tracked_status_ignores_untracked_and_does_not_write_index() {
    let fixture = Fixture::new();
    std::fs::write(fixture.path().join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(fixture.path().join("tracked.rs"), "fn first() {}\n").unwrap();
    let mut index = fixture.repo.index().unwrap();
    index.add_path(Path::new("tracked.rs")).unwrap();
    index.add_path(Path::new(".gitignore")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let sig = Signature::new("Fixture", "fixture@example.invalid", &Time::new(3600, 0)).unwrap();
    fixture
        .repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "tracked",
            &fixture.repo.find_tree(tree_id).unwrap(),
            &[],
        )
        .unwrap();
    std::fs::write(fixture.path().join("ignored.rs"), "ignored").unwrap();
    std::fs::write(fixture.path().join("untracked.rs"), "untracked").unwrap();
    let index_path = fixture.repo.path().join("index");
    let before = std::fs::read(&index_path).unwrap();
    assert!(!is_dirty(fixture.path()).unwrap());
    assert_eq!(
        ls_files(fixture.path()).unwrap(),
        [".gitignore", "tracked.rs"]
    );
    std::fs::write(fixture.path().join("tracked.rs"), "fn changed() {}\n").unwrap();
    assert!(is_dirty(fixture.path()).unwrap());
    assert_eq!(std::fs::read(&index_path).unwrap(), before);
    index.add_path(Path::new("tracked.rs")).unwrap();
    index.write().unwrap();
    assert!(is_dirty(fixture.path()).unwrap());
    std::fs::remove_file(fixture.path().join("tracked.rs")).unwrap();
    assert!(is_dirty(fixture.path()).unwrap());
    assert!(ls_files(fixture.path())
        .unwrap()
        .contains(&"tracked.rs".to_owned()));
}

#[test]
fn tree_blobs_symlinks_gitlinks_and_literal_filenames() {
    let fixture = Fixture::new();
    let root = fixture.commit(&[], &[], 3600, "empty");
    let root_text = root.to_string();
    let id = fixture.commit(
        &[
            ("dir/nested.rs", b"fn nested() {}", 0o100644),
            ("link.rs", b"dir/nested.rs", 0o120000),
            ("module", root_text.as_bytes(), 0o160000),
            (" space\t\n.rs ", b"fn odd() {}", 0o100755),
        ],
        &[root],
        7200,
        "tree",
    );
    let paths: Vec<_> = ls_tree(fixture.path(), &id.to_string())
        .unwrap()
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    assert_eq!(paths, [" space\t\n.rs ", "dir/nested.rs", "link.rs"]);
    assert_eq!(
        commit_paths(fixture.path(), &id.to_string(), 10).unwrap(),
        [" space\t\n.rs ", "dir/nested.rs", "link.rs", "module"]
    );
    assert_eq!(
        commit_paths(fixture.path(), &id.to_string(), 1).unwrap(),
        [" space\t\n.rs "]
    );
}

#[test]
fn object_delivery_is_ordered_skips_missing_and_stops_on_callback_error() {
    let fixture = Fixture::new();
    let a = fixture.repo.blob(b"one\0binary").unwrap().to_string();
    let b = fixture
        .repo
        .blob(&vec![b'x'; 2 * 1024 * 1024])
        .unwrap()
        .to_string();
    let missing = Oid::ZERO_SHA1.to_string();
    let mut seen = Vec::new();
    cat_objects(
        fixture.path(),
        &[a.clone(), missing, b.clone(), a.clone()],
        |id, bytes| {
            seen.push((id.to_owned(), bytes.len()));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        seen,
        [
            (a.clone(), 10),
            (b.clone(), 2 * 1024 * 1024),
            (a.clone(), 10)
        ]
    );
    let mut calls = 0;
    let error = cat_objects(fixture.path(), &[a, b], |_, _| {
        calls += 1;
        bail!("consumer stopped")
    })
    .unwrap_err();
    assert!(error.to_string().contains("consumer stopped"));
    assert_eq!(calls, 1);
    assert!(cat_objects(fixture.path(), &["bad ID".into()], |_, _| Ok(())).is_err());
    assert!(cat_objects(Path::new("/not/a/repository"), &[], |_, _| Ok(())).is_ok());
}

#[test]
fn pickaxe_is_literal_nonoverlapping_per_file_and_binary_safe() {
    let fixture = Fixture::new();
    let root = fixture.commit(
        &[("a.rs", b"aaaa\0needle\n", 0o100644)],
        &[],
        3600,
        "root\ncontinued\n\nbody",
    );
    let moved = fixture.commit(
        &[("a.rs", b"needle\0aaaa\n", 0o100644)],
        &[root],
        7200,
        "move within file",
    );
    let changed = fixture.commit(
        &[("a.rs", b"needle\0aa\n", 0o100644)],
        &[moved],
        10800,
        "remove occurrence",
    );
    let hits = pickaxe(fixture.path(), "aa", 10).unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| hit.commit.clone())
            .collect::<Vec<_>>(),
        [changed.to_string(), root.to_string()]
    );
    assert_eq!(hits[1].subject, "root continued");
    assert_eq!(hits[1].date, "1970-01-01");
    assert_eq!(pickaxe(fixture.path(), "aa", 1).unwrap().len(), 1);
    assert!(pickaxe(fixture.path(), "aa", 0).unwrap().is_empty());
    assert!(pickaxe(fixture.path(), "a+", 10).unwrap().is_empty());
    assert!(pickaxe(fixture.path(), "", 10).is_err());
    assert!(pickaxe(fixture.path(), "a\0", 10).is_err());
    assert_eq!(
        pickaxe(fixture.path(), "needle", 10).unwrap()[0].commit,
        root.to_string()
    );
    let across = fixture.commit(
        &[("a.rs", b"\0aa\n", 0o100644), ("b.rs", b"needle", 0o100644)],
        &[changed],
        14400,
        "move across files",
    );
    assert_eq!(
        pickaxe(fixture.path(), "needle", 10).unwrap()[0].commit,
        across.to_string()
    );
}

#[test]
fn rename_and_rename_with_edit_follow_native_pairing_and_config() {
    let fixture = Fixture::new();
    let initial =
        b"fn needle() {}\nfn preserved_a() {}\nfn preserved_b() {}\nfn preserved_c() {}\n";
    let edited = b"fn gone() {}\nfn preserved_a() {}\nfn preserved_b() {}\nfn preserved_c() {}\n";
    let root = fixture.commit(&[("old.rs", initial, 0o100644)], &[], 3600, "root");
    let rename = fixture.commit(&[("new.rs", initial, 0o100644)], &[root], 7200, "rename");
    assert_eq!(
        pickaxe(fixture.path(), "needle", 10)
            .unwrap()
            .iter()
            .map(|hit| hit.commit.clone())
            .collect::<Vec<_>>(),
        [root.to_string()]
    );
    assert_eq!(
        commit_paths(fixture.path(), &rename.to_string(), 10).unwrap(),
        ["new.rs"]
    );
    let edit = fixture.commit(
        &[("third.rs", edited, 0o100644)],
        &[rename],
        10800,
        "rename and edit",
    );
    assert_eq!(
        pickaxe(fixture.path(), "needle", 10).unwrap()[0].commit,
        edit.to_string()
    );
    assert_eq!(
        commit_paths(fixture.path(), &edit.to_string(), 10).unwrap(),
        ["third.rs"]
    );
    fixture
        .repo
        .config()
        .unwrap()
        .set_bool("diff.renames", false)
        .unwrap();
    assert_eq!(
        pickaxe(fixture.path(), "needle", 10)
            .unwrap()
            .iter()
            .map(|hit| hit.commit.clone())
            .collect::<Vec<_>>(),
        [edit.to_string(), rename.to_string(), root.to_string()]
    );
}

#[test]
fn merges_are_not_pickaxe_hits_but_both_parent_histories_are_walked() {
    let fixture = Fixture::new();
    let root = fixture.commit(&[], &[], 3600, "root");
    let left = fixture.commit(
        &[("left.rs", b"needle left", 0o100644)],
        &[root],
        7200,
        "left",
    );
    let right = fixture.commit(
        &[("right.rs", b"needle right", 0o100644)],
        &[root],
        10800,
        "right",
    );
    let merge = fixture.commit(
        &[
            ("left.rs", b"needle left", 0o100644),
            ("right.rs", b"needle right", 0o100644),
            ("resolution.rs", b"needle merge-only", 0o100644),
        ],
        &[left, right],
        14400,
        "merge",
    );
    assert_eq!(
        pickaxe(fixture.path(), "needle", 10)
            .unwrap()
            .iter()
            .map(|hit| hit.commit.clone())
            .collect::<Vec<_>>(),
        [right.to_string(), left.to_string()]
    );
    assert!(pickaxe(fixture.path(), "merge-only", 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        commit_paths(fixture.path(), &merge.to_string(), 10).unwrap(),
        ["resolution.rs"]
    );
}

#[test]
fn author_date_uses_authors_offset_not_utc() {
    let fixture = Fixture::new();
    fixture.commit(&[("a.rs", b"needle", 0o100644)], &[], 1800, "late locally");
    assert_eq!(
        pickaxe(fixture.path(), "needle", 10).unwrap()[0].date,
        "1969-12-31"
    );
}

#[test]
fn clock_skew_does_not_put_a_parent_before_its_child() {
    let fixture = Fixture::new();
    let root = fixture.commit(&[("a.rs", b"needle", 0o100644)], &[], 7200, "root");
    let child = fixture.commit(
        &[("a.rs", b"needle needle", 0o100644)],
        &[root],
        3600,
        "older child",
    );
    assert_eq!(
        pickaxe(fixture.path(), "needle", 10)
            .unwrap()
            .iter()
            .map(|hit| hit.commit.clone())
            .collect::<Vec<_>>(),
        [child.to_string(), root.to_string()]
    );
}

#[test]
fn linked_worktree_uses_its_own_head_and_index() {
    let fixture = Fixture::new();
    let id = fixture.commit(&[("main.rs", b"main", 0o100644)], &[], 3600, "main");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("linked");
    fixture.repo.worktree("linked", &path, None).unwrap();
    assert!(path.join(".git").is_file());
    assert!(is_repository(&path));
    assert_eq!(head(&path).unwrap(), id.to_string());
    assert_eq!(ls_files(&path).unwrap(), ["main.rs"]);
    std::fs::write(path.join("main.rs"), "linked edit").unwrap();
    assert!(is_dirty(&path).unwrap());
}

fn submodule_fixture() -> (Fixture, Repository) {
    let fixture = Fixture::new();
    // Setup only; no clone, remote or transport operation is performed.
    let mut module = fixture
        .repo
        .submodule("file:///fixture-unused", Path::new("module"), false)
        .unwrap();
    let child = module.open().unwrap();
    std::fs::write(fixture.path().join("module/tracked.rs"), "fn child() {}\n").unwrap();
    let mut index = child.index().unwrap();
    index.add_path(Path::new("tracked.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let signature =
        Signature::new("Fixture", "fixture@example.invalid", &Time::new(3600, 0)).unwrap();
    child
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "child",
            &child.find_tree(tree_id).unwrap(),
            &[],
        )
        .unwrap();
    module.add_finalize().unwrap();
    drop(module);
    let tree_id = fixture.repo.index().unwrap().write_tree().unwrap();
    fixture
        .repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "parent",
            &fixture.repo.find_tree(tree_id).unwrap(),
            &[],
        )
        .unwrap();
    (fixture, child)
}

#[test]
fn submodule_untracked_content_is_not_tracked_dirt() {
    let (fixture, _child) = submodule_fixture();
    assert!(!is_dirty(fixture.path()).unwrap());
    std::fs::write(fixture.path().join("module/untracked.rs"), "untracked").unwrap();
    assert!(!is_dirty(fixture.path()).unwrap());
    std::fs::write(fixture.path().join("module/tracked.rs"), "changed").unwrap();
    assert!(is_dirty(fixture.path()).unwrap());
    assert_eq!(ls_files(fixture.path()).unwrap(), [".gitmodules", "module"]);
    assert_eq!(ls_tree(fixture.path(), "HEAD").unwrap().len(), 1);
}

#[test]
fn conflict_stages_and_tracked_ignored_paths_stay_in_index_listing() {
    let fixture = Fixture::new();
    let id = fixture.repo.blob(b"content").unwrap();
    let mut index = fixture.repo.index().unwrap();
    index.add(&entry(b"ignored.rs", id, 0o100644)).unwrap();
    let mut base = entry(b"conflict.rs", id, 0o100644);
    base.flags = 1 << 12;
    index.add(&base).unwrap();
    base.flags = 2 << 12;
    index.add(&base).unwrap();
    index.write().unwrap();
    std::fs::write(fixture.path().join(".gitignore"), "ignored.rs\n").unwrap();
    assert_eq!(
        ls_files(fixture.path()).unwrap(),
        ["conflict.rs", "conflict.rs", "ignored.rs"]
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_tree_and_index_names_are_lossy_only_at_public_string_boundary() {
    let fixture = Fixture::new();
    let id = fixture.repo.blob(b"fn native() {}").unwrap();
    let mut index = fixture.repo.index().unwrap();
    index
        .add(&entry(b"dir\xff/file\xfe.rs", id, 0o100644))
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    assert_eq!(
        ls_files(fixture.path()).unwrap(),
        ["dir\u{fffd}/file\u{fffd}.rs"]
    );
    assert_eq!(
        ls_tree(fixture.path(), &tree_id.to_string()).unwrap()[0].path,
        "dir\u{fffd}/file\u{fffd}.rs"
    );
}

/// Fixture-only oracle. The production module and ordinary tests never spawn;
/// run this explicitly with `--ignored` before the empty-PATH native-only run.
#[test]
#[ignore = "differential control requires the Git CLI"]
fn differential_against_git_cli_on_fixture_only() {
    fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
    fn compare_history(fixture: &Fixture, needle: &str) {
        let old = git(
            fixture.path(),
            &[
                "log",
                "-S",
                needle,
                "--max-count",
                "20",
                "--date=short",
                "--pretty=format:%H%x1f%ad%x1f%s",
            ],
        );
        let expected: Vec<_> = String::from_utf8(old)
            .unwrap()
            .lines()
            .map(|line| {
                let mut fields = line.splitn(3, '\u{1f}');
                Change {
                    commit: fields.next().unwrap().into(),
                    date: fields.next().unwrap().into(),
                    subject: fields.next().unwrap().into(),
                }
            })
            .collect();
        let actual = pickaxe(fixture.path(), needle, 20).unwrap();
        assert_eq!(actual, expected, "pickaxe {needle}");
        for hit in actual {
            let paths = git(
                fixture.path(),
                &["show", "--name-only", "--pretty=format:", &hit.commit],
            );
            let expected: Vec<_> = String::from_utf8(paths)
                .unwrap()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(String::from)
                .collect();
            assert_eq!(
                commit_paths(fixture.path(), &hit.commit, 20).unwrap(),
                expected
            );
        }
    }
    let fixture = Fixture::new();
    let source = b"fn needle() {}\nfn unchanged_a() {}\nfn unchanged_b() {}\nfn unchanged_c() {}\n";
    let root = fixture.commit(
        &[
            ("a.rs", source, 0o100644),
            ("binary.rs", b"needle\0aaaa", 0o100644),
        ],
        &[],
        3600,
        "root\ncontinued\n\nbody",
    );
    let rename = fixture.commit(
        &[
            ("b.rs", source, 0o100644),
            ("binary.rs", b"needle\0aaaa", 0o100644),
        ],
        &[root],
        7200,
        "rename",
    );
    let edit = fixture.commit(
        &[
            (
                "c.rs",
                b"fn removed() {}\nfn unchanged_a() {}\nfn unchanged_b() {}\nfn unchanged_c() {}\n",
                0o100644,
            ),
            ("binary.rs", b"needle\0aa", 0o100644),
        ],
        &[rename],
        10800,
        "rename edit",
    );
    let side = fixture.commit(
        &[(
            "side.rs",
            b"fn on_an_unrelated_branch() { needle(); }\n",
            0o100644,
        )],
        &[root],
        9000,
        "side",
    );
    fixture.commit(
        &[("merged.rs", b"needle merge-only", 0o100644)],
        &[edit, side],
        14400,
        "merge",
    );
    for needle in ["needle", "aa", "merge-only", "unchanged_a", "absent"] {
        compare_history(&fixture, needle);
    }
    fixture
        .repo
        .config()
        .unwrap()
        .set_bool("diff.renames", false)
        .unwrap();
    compare_history(&fixture, "needle");
    assert_eq!(
        head(fixture.path()).unwrap(),
        String::from_utf8(git(fixture.path(), &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
    );
    assert_eq!(
        resolve(fixture.path(), "HEAD~1").unwrap(),
        String::from_utf8(git(fixture.path(), &["rev-parse", "HEAD~1^{commit}"]))
            .unwrap()
            .trim()
    );

    // Populate the worktree/index natively; the oracle only reads the fixture.
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    fixture.repo.checkout_head(Some(&mut checkout)).unwrap();
    std::fs::write(fixture.path().join("untracked.rs"), "not tracked").unwrap();
    let expected: Vec<_> = git(fixture.path(), &["ls-files", "-z"])
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    assert_eq!(ls_files(fixture.path()).unwrap(), expected);
    for content in ["needle merge-only", "changed"] {
        std::fs::write(fixture.path().join("merged.rs"), content).unwrap();
        assert_eq!(
            is_dirty(fixture.path()).unwrap(),
            !git(
                fixture.path(),
                &["status", "--porcelain", "--untracked-files=no"]
            )
            .is_empty()
        );
    }
    let raw = git(fixture.path(), &["ls-tree", "-r", "-z", "HEAD"]);
    let expected: Vec<_> = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let record = String::from_utf8_lossy(part);
            let (meta, path) = record.split_once('\t').unwrap();
            TreeEntry {
                path: path.into(),
                object: meta.split_whitespace().nth(2).unwrap().into(),
            }
        })
        .collect();
    assert_eq!(ls_tree(fixture.path(), "HEAD").unwrap(), expected);
    for entry in expected {
        let expected = git(fixture.path(), &["cat-file", "blob", &entry.object]);
        cat_objects(fixture.path(), &[entry.object], |_, actual| {
            assert_eq!(actual, expected);
            Ok(())
        })
        .unwrap();
    }

    let (fixture, _child) = submodule_fixture();
    for content in [None, Some("untracked"), Some("tracked")] {
        if let Some(path) = content {
            std::fs::write(fixture.path().join(format!("module/{path}.rs")), "changed").unwrap();
        }
        assert_eq!(
            is_dirty(fixture.path()).unwrap(),
            !git(
                fixture.path(),
                &["status", "--porcelain", "--untracked-files=no"]
            )
            .is_empty()
        );
    }
    for ignore in ["none", "untracked", "dirty", "all"] {
        fixture
            .repo
            .config()
            .unwrap()
            .set_str("submodule.module.ignore", ignore)
            .unwrap();
        assert_eq!(
            is_dirty(fixture.path()).unwrap(),
            !git(
                fixture.path(),
                &["status", "--porcelain", "--untracked-files=no"]
            )
            .is_empty(),
            "submodule ignore={ignore}"
        );
    }

    // Clock skew must not turn the traversal into a global timestamp sort.
    let fixture = Fixture::new();
    let root = fixture.commit(&[("a.rs", b"needle", 0o100644)], &[], 7200, "root");
    fixture.commit(
        &[("a.rs", b"needle needle", 0o100644)],
        &[root],
        3600,
        "older child",
    );
    compare_history(&fixture, "needle");

    // A deliberate library boundary, not a parity assertion: libgit2's
    // inexact similarity pairs these tiny binary/text blobs while Git does
    // not. Keep this observed -S/path difference visible when upgrading it.
    let fixture = Fixture::new();
    let root = fixture.commit(
        &[("old.rs", b"needle\0aaaa", 0o100644)],
        &[],
        3600,
        "binary",
    );
    let changed = fixture.commit(&[("new.rs", b"needle", 0o100644)], &[root], 7200, "text");
    assert_eq!(
        commit_paths(fixture.path(), &changed.to_string(), 10).unwrap(),
        ["new.rs"]
    );
    let old_paths = String::from_utf8(git(
        fixture.path(),
        &[
            "show",
            "--name-only",
            "--pretty=format:",
            &changed.to_string(),
        ],
    ))
    .unwrap();
    assert_eq!(
        old_paths
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>(),
        ["new.rs", "old.rs"]
    );
    assert_eq!(
        pickaxe(fixture.path(), "needle", 10)
            .unwrap()
            .iter()
            .map(|hit| hit.commit.clone())
            .collect::<Vec<_>>(),
        [root.to_string()]
    );
    let old_history = String::from_utf8(git(
        fixture.path(),
        &["log", "-S", "needle", "--pretty=format:%H"],
    ))
    .unwrap();
    assert_eq!(
        old_history.lines().collect::<Vec<_>>(),
        [changed.to_string(), root.to_string()]
    );
}
