//! Host-side Git and Python adapter for the direct CLI. Native admission,
//! source provenance and revision construction stay in Wiki operations.

use super::{cli, Wiki};
use crate::out::Out;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const ADAPTER: &[(&str, &[u8])] = &[
    (
        "gollum-full-plan.py",
        include_bytes!("../../scripts/gollum-full-plan.py"),
    ),
    (
        "gollum-asciidoc.py",
        include_bytes!("../../scripts/gollum-asciidoc.py"),
    ),
    ("gollum-plan", include_bytes!("../../scripts/gollum-plan")),
];

fn run(command: &mut Command, operation: &str) -> Result<Output> {
    let output = command.output().with_context(|| operation.to_owned())?;
    if !output.status.success() {
        bail!(
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn local_git_process(program: &str) -> Command {
    let mut git = Command::new(program);
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        git.env_remove(name);
    }
    git.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file");
    git
}

fn refs(output: &[u8]) -> Result<BTreeMap<&[u8], &[u8]>> {
    output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let space = line
                .iter()
                .position(|byte| *byte == b' ')
                .context("read Git reference")?;
            Ok((&line[space + 1..], &line[..space]))
        })
        .collect()
}

fn unpack_bundle(source: &Path, directory: &Path) -> Result<std::path::PathBuf> {
    let bundle = directory.join("source.bundle");
    let repo = directory.join("source.git");
    fs::copy(source, &bundle).context("freeze supplied Git bundle")?;
    run(
        local_git_process("git")
            .args(["clone", "--mirror", "--quiet", "--"])
            .arg(&bundle)
            .arg(&repo),
        "open frozen Git bundle (a self-contained bundle is required)",
    )?;
    let advertised = run(
        local_git_process("git")
            .args(["bundle", "list-heads"])
            .arg(&bundle),
        "list bundled references",
    )?;
    let advertised = refs(&advertised.stdout)?;
    let mirrored = run(
        local_git_process("git")
            .arg("-C")
            .arg(&repo)
            .args(["for-each-ref", "--format=%(objectname) %(refname)"]),
        "list mirrored references",
    )?;
    let mirrored = refs(&mirrored.stdout)?;
    for (name, oid) in advertised {
        if name == b"HEAD" {
            let oid = std::str::from_utf8(oid).context("invalid Git object id")?;
            run(
                local_git_process("git").arg("-C").arg(&repo).args([
                    "update-ref",
                    "--no-deref",
                    "HEAD",
                    oid,
                ]),
                "retain source HEAD",
            )?;
        } else if mirrored.get(name) != Some(&oid) {
            bail!(
                "Git mirror did not retain bundled reference {}",
                String::from_utf8_lossy(name)
            );
        }
    }
    Ok(repo)
}

/// Translate a Gollum history and import it, or with `repair` (`Some(apply)`)
/// correct the histories an earlier import left with image links.
pub(super) fn import(
    wiki: &Wiki,
    source: &Path,
    tags: &[String],
    bases: &[String],
    repair: Option<bool>,
    out: &mut Out<'_>,
) -> Result<()> {
    let source = source
        .canonicalize()
        .context("open local Gollum repository or bundle")?;
    let temp = tempfile::Builder::new().prefix("wiki-gollum-").tempdir()?;
    let repo = if source.is_dir() {
        source
    } else if source.is_file() {
        unpack_bundle(&source, temp.path())?
    } else {
        bail!("expected a local Git directory or a self-contained Git bundle");
    };
    for (script, bytes) in ADAPTER {
        fs::write(temp.path().join(script), bytes)?;
    }
    let directory = temp.path().join("plan");
    let mut translate = local_git_process("python3");
    translate
        .arg("-I")
        .arg(temp.path().join("gollum-full-plan.py"))
        .arg(&repo)
        .arg(&directory);
    for base in bases {
        translate.arg(format!("--source-base={base}"));
    }
    let translation = run(
        &mut translate,
        "translate Gollum history with python3 and git",
    )?;
    let plan = cli::read_plan(&directory.join("plan.json"))?;
    let summary = std::str::from_utf8(&translation.stdout)?.trim().to_owned();
    if let Some(apply) = repair {
        let report = wiki.repair_gollum_images(plan, apply)?;
        if !summary.is_empty() {
            out.line(summary)?;
        }
        return report_repair(&report, apply, out);
    }
    let report = wiki.import_gollum_plan(plan, tags)?;
    if !summary.is_empty() {
        out.line(summary)?;
    }
    cli::report_import(&report, out)
}

fn report_repair(report: &super::ImageRepairReport, apply: bool, out: &mut Out<'_>) -> Result<()> {
    let mut applied = 0;
    let mut skipped = 0;
    for head in &report.heads {
        applied += head.successor.map_or(0, |_| head.applied);
        skipped += head.skipped.len();
        match head.successor {
            Some(successor) => out.line(format!(
                "CORRECT  {:x} -> {successor:x}  {} images  entry {:x}  {}",
                head.head, head.applied, head.entry, head.title
            ))?,
            None if head.skipped.is_empty() => continue,
            None => out.line(format!(
                "KEEP     {:x}  entry {:x}  {}",
                head.head, head.entry, head.title
            ))?,
        }
        for reason in &head.skipped {
            out.line(format!("  skipped: {reason}"))?;
        }
    }
    for head in &report.moved {
        out.line(format!(
            "MOVED    {head:x}  gained a successor while planning; not corrected"
        ))?;
    }
    out.line(format!(
        "{} heads with imported images: {} corrected ({applied} images), {skipped} images left as links, {}",
        report.heads.len(),
        report.corrected(),
        if apply {
            format!("{} publications", report.publications)
        } else {
            "dry run: nothing published (add --apply)".to_owned()
        }
    ))
}
