//! Bake the git commit into every faculty binary, so an installed binary can
//! answer the version-skew question ("which build am I actually running?") —
//! the question at the heart of the 2026-07-03 stale-binary incident.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn main() {
    let hash = git(&["rev-parse", "--short=9", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|out| !out.is_empty());
    println!(
        "cargo:rustc-env=FACULTIES_GIT_VERSION={hash}{}",
        if dirty { "-dirty" } else { "" }
    );

    // Re-run when HEAD moves so the baked hash stays honest. Ask git where its
    // files are: in a linked worktree `.git` is a file, HEAD and index live in
    // the worktree's own git dir, and branch refs live in the common dir. A
    // watched path that does not exist makes cargo re-run the script on every
    // build, so only existing paths are named; without git (a packaged crate)
    // nothing but this script is watched.
    let mut watched: Vec<PathBuf> = Vec::new();
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let git_dir = PathBuf::from(git_dir);
        watched.push(git_dir.join("HEAD"));
        watched.push(git_dir.join("index"));
        if let Some(common) = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
            let common = Path::new(&common);
            if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
                watched.push(common.join(branch));
            }
            watched.push(common.join("packed-refs"));
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    for path in watched.iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
