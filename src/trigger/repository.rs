//! Read-only evidence about a repository's **cached local refs**. No operation
//! here fetches, contacts a remote, refreshes the index, or changes a ref. A
//! zero behind count is not evidence that the remote server is up to date.
//!
//! Ref reads are not atomic. The compared commit IDs are captured explicitly,
//! and the ancestry comparison uses those IDs rather than mutable ref names.
//! Counts describe the locally available commit graph, including any shallow
//! history boundary; they do not establish remote completeness.
//! Partial-clone/promisor repositories are deliberately unsupported: even
//! apparently read-only object queries can otherwise initiate a lazy fetch.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Comparison {
    /// Short local branch name, with no refs/heads/ prefix.
    pub branch: String,
    /// Full local upstream ref, normally refs/remotes/<remote>/<branch>.
    pub upstream: String,
    pub head_oid: String,
    pub upstream_oid: String,
    /// Commits reachable from captured upstream_oid but not captured head_oid.
    pub behind: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Inspection {
    Compared(Comparison),
    NoUpstream {
        branch: String,
        head_oid: String,
    },
    Detached {
        head_oid: String,
    },
    /// Symbolic HEAD names a local branch for which no ref currently exists.
    /// This does not claim that the branch has never existed historically.
    Unborn {
        branch: String,
    },
}

#[derive(Debug)]
pub enum InspectionError {
    NotRepository {
        path: PathBuf,
        detail: String,
    },
    Invocation {
        operation: &'static str,
        source: std::io::Error,
    },
    GitFailed {
        operation: &'static str,
        detail: String,
    },
    InvalidOutput {
        operation: &'static str,
        detail: String,
    },
    UnsupportedPartialClone {
        configuration: String,
    },
    MalformedUpstream {
        branch: String,
        detail: String,
    },
    MissingUpstream {
        branch: String,
        upstream: String,
    },
    ComparisonFailed {
        head_oid: String,
        upstream_oid: String,
        detail: String,
    },
}

impl fmt::Display for InspectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRepository { path, detail } =>
                write!(formatter, "{} is not a readable Git repository: {detail}", path.display()),
            Self::Invocation { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::GitFailed { operation, detail } |
            Self::InvalidOutput { operation, detail } => write!(formatter, "{operation}: {detail}"),
            Self::UnsupportedPartialClone { configuration } => write!(formatter,
                "resident-only inspection does not support partial-clone/promisor repositories ({configuration}); no fetch was performed"),
            Self::MalformedUpstream { branch, detail } =>
                write!(formatter, "branch {branch:?} has an unresolved upstream configuration: {detail}"),
            Self::MissingUpstream { branch, upstream } =>
                write!(formatter, "branch {branch:?} has no locally available upstream ref {upstream:?}"),
            Self::ComparisonFailed { head_oid, upstream_oid, detail } =>
                write!(formatter, "compare cached commits {head_oid}..{upstream_oid}: {detail}"),
        }
    }
}

impl std::error::Error for InspectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invocation { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Inspect only refs and objects already available in this explicit repository.
/// A configured but missing/broken upstream is an error, never a quiet result.
pub fn inspect(repo: &Path) -> Result<Inspection, InspectionError> {
    let repository = git(repo, "locate repository", &["rev-parse", "--git-dir"])?;
    if !repository.status.success() {
        let detail = failure(&repository);
        if String::from_utf8_lossy(&repository.stderr).contains("not a git repository") {
            return Err(InspectionError::NotRepository {
                path: repo.to_owned(),
                detail,
            });
        }
        return Err(InspectionError::GitFailed {
            operation: "locate repository",
            detail,
        });
    }
    refuse_partial_clone(repo)?;

    let symbolic = git(
        repo,
        "read symbolic HEAD",
        &["symbolic-ref", "--quiet", "HEAD"],
    )?;
    if symbolic.status.code() == Some(1) {
        return Ok(Inspection::Detached {
            head_oid: commit_oid(repo, "HEAD")?,
        });
    }
    let full_branch = successful_line(symbolic, "read symbolic HEAD")?;
    let branch = full_branch
        .strip_prefix("refs/heads/")
        .ok_or_else(|| InspectionError::InvalidOutput {
            operation: "read symbolic HEAD",
            detail: format!("HEAD names a non-branch ref {full_branch:?}"),
        })?
        .to_owned();
    if !ref_exists(repo, &full_branch)? {
        return Ok(Inspection::Unborn { branch });
    }
    let head_oid = commit_oid(repo, &full_branch)?;

    // Ask Git to interpret the binding; do not duplicate its remote/fetch
    // mapping rules. The configured branch is pinned by name, not mutable HEAD.
    // Presence probes distinguish absent configuration from a broken binding.
    let remote = configured(repo, &format!("branch.{branch}.remote"))?;
    let merge = configured(repo, &format!("branch.{branch}.merge"))?;
    if !remote && !merge {
        return Ok(Inspection::NoUpstream { branch, head_oid });
    }
    let upstream = git(
        repo,
        "resolve configured upstream",
        &["for-each-ref", "--format=%(upstream)", "--", &full_branch],
    )?;
    if !upstream.status.success() {
        return Err(InspectionError::MalformedUpstream {
            branch,
            detail: failure(&upstream),
        });
    }
    let upstream = output_line(&upstream.stdout, "resolve configured upstream")?;
    if upstream.is_empty() || !upstream.starts_with("refs/") {
        return Err(InspectionError::MalformedUpstream {
            branch,
            detail: "Git could not map the configured upstream to one local ref".into(),
        });
    }
    if !ref_exists(repo, &upstream)? {
        return Err(InspectionError::MissingUpstream { branch, upstream });
    }
    let upstream_oid = commit_oid(repo, &upstream)?;
    let behind = compare(repo, &head_oid, &upstream_oid)?;
    Ok(Inspection::Compared(Comparison {
        branch,
        upstream,
        head_oid,
        upstream_oid,
        behind,
    }))
}

fn git(repo: &Path, operation: &'static str, args: &[&str]) -> Result<Output, InspectionError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    resident_environment(&mut command);
    command
        .output()
        .map_err(|source| InspectionError::Invocation { operation, source })
}

fn resident_environment(command: &mut Command) {
    // A hook's inherited local-repository environment must not redirect refs,
    // objects, graph boundaries, config, or namespaces away from explicit -C.
    // Removing CONFIG_COUNT also makes inherited CONFIG_KEY_n/VALUE_n inert.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_SHALLOW_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(name);
    }
    // Git documents an empty GIT_ALLOW_PROTOCOL as denying every transport,
    // overriding even protocol.<name>.allow=always. This works on Git 2.43,
    // where GIT_NO_LAZY_FETCH itself is not yet understood. Do not rely on the
    // newer variable alone: partial-clone configuration is refused up front.
    // https://git-scm.com/docs/git#Documentation/git.txt-GITALLOWPROTOCOL
    command
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
}

fn refuse_partial_clone(repo: &Path) -> Result<(), InspectionError> {
    // These config-only commands do not resolve objects. Check before the
    // first HEAD/ref peel. A concurrent config change still cannot enable a
    // transport because every child inherits the empty protocol allow-list.
    let output = git(
        repo,
        "inspect partial-clone configuration",
        &[
            "config",
            "--name-only",
            "--get-regexp",
            r"^(extensions\.partialclone|remote\..*\.(promisor|partialclonefilter))$",
        ],
    )?;
    if output.status.code() == Some(1) {
        return Ok(());
    }
    if !output.status.success() {
        return Err(InspectionError::GitFailed {
            operation: "inspect partial-clone configuration",
            detail: failure(&output),
        });
    }
    let names =
        std::str::from_utf8(&output.stdout).map_err(|error| InspectionError::InvalidOutput {
            operation: "inspect partial-clone configuration",
            detail: error.to_string(),
        })?;
    for name in names.lines() {
        if name.ends_with(".promisor") {
            let output = git(
                repo,
                "read promisor flag",
                &["config", "--type=bool", "--get-all", name],
            )?;
            if !output.status.success() {
                return Err(InspectionError::GitFailed {
                    operation: "read promisor flag",
                    detail: failure(&output),
                });
            }
            if output
                .stdout
                .split(|byte| *byte == b'\n')
                .all(|value| value == b"false" || value.is_empty())
            {
                continue;
            }
        }
        return Err(InspectionError::UnsupportedPartialClone {
            configuration: name.to_owned(),
        });
    }
    Ok(())
}

fn failure(output: &Output) -> String {
    format!(
        "{}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn output_line(bytes: &[u8], operation: &'static str) -> Result<String, InspectionError> {
    let text = std::str::from_utf8(bytes).map_err(|error| InspectionError::InvalidOutput {
        operation,
        detail: format!("non-UTF-8 output: {error}"),
    })?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.contains(['\n', '\r', '\0']) {
        return Err(InspectionError::InvalidOutput {
            operation,
            detail: "expected one output line".into(),
        });
    }
    Ok(line.to_owned())
}

fn successful_line(output: Output, operation: &'static str) -> Result<String, InspectionError> {
    if !output.status.success() {
        return Err(InspectionError::GitFailed {
            operation,
            detail: failure(&output),
        });
    }
    output_line(&output.stdout, operation)
}

fn ref_exists(repo: &Path, reference: &str) -> Result<bool, InspectionError> {
    let output = git(
        repo,
        "inspect local ref",
        &["show-ref", "--verify", "--quiet", reference],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(InspectionError::GitFailed {
            operation: "inspect local ref",
            detail: failure(&output),
        }),
    }
}

fn configured(repo: &Path, key: &str) -> Result<bool, InspectionError> {
    let output = git(
        repo,
        "read upstream configuration",
        &["config", "--get-all", key],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(InspectionError::GitFailed {
            operation: "read upstream configuration",
            detail: failure(&output),
        }),
    }
}

fn commit_oid(repo: &Path, reference: &str) -> Result<String, InspectionError> {
    let object = format!("{reference}^{{commit}}");
    let output = git(
        repo,
        "resolve captured commit",
        &["rev-parse", "--verify", "--end-of-options", &object],
    )?;
    let oid = successful_line(output, "resolve captured commit")?;
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(InspectionError::InvalidOutput {
            operation: "resolve captured commit",
            detail: format!("not a full SHA-1/SHA-256 object ID: {oid:?}"),
        });
    }
    Ok(oid)
}

fn compare(repo: &Path, head_oid: &str, upstream_oid: &str) -> Result<u64, InspectionError> {
    let range = format!("{head_oid}..{upstream_oid}");
    let output = git(
        repo,
        "compare captured commits",
        &["rev-list", "--count", &range, "--"],
    )?;
    if !output.status.success() {
        return Err(InspectionError::ComparisonFailed {
            head_oid: head_oid.into(),
            upstream_oid: upstream_oid.into(),
            detail: failure(&output),
        });
    }
    let count = output_line(&output.stdout, "compare captured commits")?;
    count
        .parse()
        .map_err(|_| InspectionError::ComparisonFailed {
            head_oid: head_oid.into(),
            upstream_oid: upstream_oid.into(),
            detail: format!("Git returned an invalid count {count:?}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Trigger fixture")
            .env("GIT_AUTHOR_EMAIL", "trigger@example.invalid")
            .env("GIT_COMMITTER_NAME", "Trigger fixture")
            .env("GIT_COMMITTER_EMAIL", "trigger@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn repository(parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        std::fs::create_dir(&path).unwrap();
        fixture_git(&path, &["init", "--initial-branch=main"]);
        path
    }

    fn commit(repo: &Path, message: &str) -> String {
        fixture_git(repo, &["commit", "--allow-empty", "-m", message]);
        fixture_git(repo, &["rev-parse", "HEAD"])
    }

    fn compared(repo: &Path) -> Comparison {
        match inspect(repo).unwrap() {
            Inspection::Compared(comparison) => comparison,
            other => panic!("expected cached comparison, got {other:?}"),
        }
    }

    #[test]
    fn remote_advance_is_invisible_until_explicit_fetch_then_behind_and_caught_up() {
        let directory = tempfile::tempdir().unwrap();
        let remote = directory.path().join("bare remote.git");
        fixture_git(
            directory.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ],
        );
        let source = repository(directory.path(), "source working tree");
        let initial = commit(&source, "initial");
        fixture_git(
            &source,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        fixture_git(&source, &["push", "--set-upstream", "origin", "main"]);
        let local = directory.path().join("local with spaces");
        fixture_git(
            directory.path(),
            &["clone", remote.to_str().unwrap(), local.to_str().unwrap()],
        );
        let before = compared(&local);
        assert_eq!(before.behind, 0);
        assert_eq!(before.head_oid, initial);
        assert_eq!(before.upstream, "refs/remotes/origin/main");

        let advanced = commit(&source, "remote advance");
        fixture_git(&source, &["push", "origin", "main"]);
        assert_eq!(
            compared(&local),
            before,
            "inspection must not fetch or claim remote freshness"
        );

        fixture_git(&local, &["fetch", "origin"]);
        let behind = compared(&local);
        assert_eq!(behind.behind, 1);
        assert_eq!(behind.head_oid, initial);
        assert_eq!(behind.upstream_oid, advanced);
        fixture_git(&local, &["merge", "--ff-only", "origin/main"]);
        let caught_up = compared(&local);
        assert_eq!(caught_up.behind, 0);
        assert_eq!(caught_up.head_oid, advanced);
        assert_eq!(caught_up.upstream_oid, advanced);
        // Captured OIDs still describe the old comparison after HEAD changes.
        assert_eq!(
            compare(&local, &behind.head_oid, &behind.upstream_oid).unwrap(),
            1
        );
    }

    #[test]
    fn absent_upstream_detached_unborn_and_not_repository_are_distinct() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            inspect(directory.path()),
            Err(InspectionError::NotRepository { .. })
        ));
        let repo = repository(directory.path(), "local tree");
        assert_eq!(
            inspect(&repo).unwrap(),
            Inspection::Unborn {
                branch: "main".into()
            }
        );
        let head = commit(&repo, "initial");
        assert_eq!(
            inspect(&repo).unwrap(),
            Inspection::NoUpstream {
                branch: "main".into(),
                head_oid: head.clone(),
            }
        );
        fixture_git(&repo, &["checkout", "--detach", &head]);
        assert_eq!(
            inspect(&repo).unwrap(),
            Inspection::Detached { head_oid: head }
        );
    }

    #[test]
    fn malformed_and_missing_upstream_are_errors_not_clean_conditions() {
        let directory = tempfile::tempdir().unwrap();
        let repo = repository(directory.path(), "local");
        commit(&repo, "initial");
        fixture_git(&repo, &["config", "branch.main.remote", "origin"]);
        assert!(matches!(
            inspect(&repo),
            Err(InspectionError::MalformedUpstream { .. })
        ));
        fixture_git(
            &repo,
            &["config", "branch.main.merge", "refs/heads/missing"],
        );
        fixture_git(
            &repo,
            &["config", "remote.origin.url", repo.to_str().unwrap()],
        );
        fixture_git(
            &repo,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        assert!(matches!(
            inspect(&repo),
            Err(InspectionError::MissingUpstream { .. })
        ));
        fixture_git(&repo, &["config", "branch.main.merge", "not a valid ref"]);
        assert!(inspect(&repo).is_err());
    }

    #[test]
    fn locally_configured_branch_upstream_and_failed_comparison_are_explicit() {
        let directory = tempfile::tempdir().unwrap();
        let repo = repository(directory.path(), "local");
        let first = commit(&repo, "initial");
        fixture_git(&repo, &["branch", "base"]);
        fixture_git(&repo, &["branch", "--set-upstream-to=base", "main"]);
        let comparison = compared(&repo);
        assert_eq!(comparison.upstream, "refs/heads/base");
        assert_eq!(comparison.head_oid, first);
        assert_eq!(comparison.behind, 0);
        assert!(matches!(
            compare(&repo, &first, &"f".repeat(first.len())),
            Err(InspectionError::ComparisonFailed { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn missing_promisor_object_never_invokes_transport_or_becomes_a_clean_result() {
        let directory = tempfile::tempdir().unwrap();
        let repo = repository(directory.path(), "partial");
        let head = commit(&repo, "promised commit");
        let transport = directory.path().join("dummy-transport.sh");
        let marker = directory.path().join("transport-invoked");
        std::fs::write(&transport, "#!/bin/sh\nprintf invoked > \"$1\"\nexit 1\n").unwrap();
        let url = format!("ext::sh {} {}", transport.display(), marker.display());
        fixture_git(&repo, &["config", "remote.origin.url", &url]);
        fixture_git(&repo, &["config", "remote.origin.promisor", "true"]);
        fixture_git(&repo, &["config", "protocol.ext.allow", "always"]);
        let object = repo.join(".git/objects").join(&head[..2]).join(&head[2..]);
        assert!(
            object.is_file(),
            "fixture commit must be a loose resident object"
        );
        std::fs::remove_file(&object).unwrap();

        let error = inspect(&repo).unwrap_err();
        assert!(matches!(
            error,
            InspectionError::UnsupportedPartialClone { .. }
        ));
        assert!(error.to_string().contains("resident-only"));
        assert!(
            !marker.exists(),
            "inspection must not launch even a local transport helper"
        );
        assert!(
            !object.exists(),
            "inspection must not retrieve the missing object"
        );
        assert!(!repo.join(".git/FETCH_HEAD").exists());

        // Defense in depth works even on older Git and with an explicit
        // permissive repository setting. The only possible helper is this
        // local dummy; there is no real network endpoint in the fixture.
        let denied = git(&repo, "fixture protocol denial", &["ls-remote", "origin"]).unwrap();
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("not allowed"));
        assert!(!marker.exists());
    }

    #[test]
    fn inherited_object_namespace_and_config_overrides_do_not_redirect_explicit_path() {
        let directory = tempfile::tempdir().unwrap();
        let repo = repository(directory.path(), "requested");
        let expected = commit(&repo, "requested commit");
        let other = repository(directory.path(), "unrelated");
        commit(&other, "wrong commit");
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "--verify", "HEAD^{commit}"])
            .env("GIT_DIR", other.join(".git"))
            .env("GIT_COMMON_DIR", other.join(".git"))
            .env("GIT_OBJECT_DIRECTORY", other.join(".git/objects"))
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                other.join(".git/objects"),
            )
            .env("GIT_NAMESPACE", "unrelated-namespace")
            .env("GIT_SHALLOW_FILE", other.join("wrong-shallow-file"))
            .env("GIT_CONFIG", other.join(".git/config"))
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.bare")
            .env("GIT_CONFIG_VALUE_0", "true")
            .env("GIT_ALLOW_PROTOCOL", "ext:file:ssh")
            .env("GIT_NO_LAZY_FETCH", "0");
        resident_environment(&mut command);
        for name in [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_SHALLOW_FILE",
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
        ] {
            assert!(command
                .get_envs()
                .any(|(key, value)| key == name && value.is_none()));
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output_line(&output.stdout, "fixture HEAD").unwrap(),
            expected
        );
    }

    #[test]
    fn explicitly_false_promisor_flag_does_not_make_a_complete_repository_unsupported() {
        let directory = tempfile::tempdir().unwrap();
        let repo = repository(directory.path(), "complete");
        commit(&repo, "initial");
        fixture_git(&repo, &["config", "remote.origin.promisor", "false"]);
        assert!(matches!(
            inspect(&repo).unwrap(),
            Inspection::NoUpstream { .. }
        ));
    }
}
