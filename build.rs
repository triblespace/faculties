//! Bake the git commit into every faculty binary, so an installed binary can
//! answer the version-skew question ("which build am I actually running?") —
//! the question at the heart of the 2026-07-03 stale-binary incident.

use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    println!(
        "cargo:rustc-env=FACULTIES_GIT_VERSION={hash}{}",
        if dirty { "-dirty" } else { "" }
    );
    // Re-run when HEAD moves so the baked hash stays honest.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
