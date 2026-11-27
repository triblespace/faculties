//! Mistral Vibe 3.1.2 unified session recovery journal, store v1.
//!
//! A published headless export identifies the session and the run's outcome.
//! The native session directory also exists before a headless export can be
//! written. Both store the same journal. Generation checkpoints and their
//! chunks hold resumable context and UI history, which can have been rewritten
//! by compaction. The journal's paired agent model inputs and completion
//! results are the source of the Archive history.

mod journal;
#[cfg(test)]
pub(crate) mod tests;

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anybytes::Bytes;
use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::archive_source::{self, ProjectedSource, ProjectionStats, UnitAnnotations};
use crate::schemas::blockdag::source_projection::SOURCE_VIBE;

pub const NATIVE_SESSIONS: &str = "logs/session/unified";
const STORE_FORMAT: &str = "mistral.vibe.unified-session-store/v1";
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectionSummary {
    pub session_id: String,
    /// This is Vibe's claim about how the run ended, independent of our coverage.
    pub export_outcome: Option<String>,
    pub segments: usize,
    pub journal_records: usize,
    pub completed_calls: usize,
    pub catalog_replacements: usize,
    pub pending_calls: usize,
    pub failed_calls: usize,
    /// Calls whose only durable reply is a stream fragment the UI settled.
    pub streamed_calls: usize,
    pub unsupported_parts: usize,
    pub unmatched_assistant_inputs: usize,
    pub trailing_bytes_held: usize,
    /// First unavailable sequence before the current generation.
    pub missing_sequence: Option<u64>,
    pub stats: ProjectionStats,
}

impl ProjectionSummary {
    /// Coverage of the durable agent call stream from sequence one. An
    /// export can say the run finished while the source still has gaps.
    /// Native-only reads make no claim about the end of the headless run.
    pub fn coverage(&self) -> &'static str {
        if self.stats.projections_emitted == 0 {
            "missing"
        } else if self.export_outcome.as_deref() != Some("finished")
            || self.missing_sequence.is_some()
            || self.trailing_bytes_held != 0
            || self.pending_calls != 0
            || self.failed_calls != 0
            || self.streamed_calls != 0
            || self.unsupported_parts != 0
            || self.unmatched_assistant_inputs != 0
            || self.stats.unresolved_tool_results != 0
        {
            "partial"
        } else {
            "complete"
        }
    }
}

#[derive(Deserialize)]
struct Export {
    schema_version: u64,
    session_id: Option<String>,
    journal_dir: Option<String>,
    outcome: String,
}

#[derive(Deserialize)]
struct Current {
    generation: String,
    manifest_sha256: String,
    session_id: String,
    snapshot_sequence: u64,
    store_format: String,
    store_format_minor: u64,
}

#[derive(Deserialize)]
struct Manifest {
    generation: String,
    manifest_version: u64,
    recovery_journal_segment: Segment,
    session_id: String,
    snapshot_sequence: u64,
}

#[derive(Deserialize)]
struct Segment {
    first_sequence: u64,
    path: String,
}

#[derive(Deserialize)]
struct Metadata {
    session_id: String,
}

fn check_path(path: &Path) -> Result<()> {
    for parent in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        if fs::symlink_metadata(parent)?.file_type().is_symlink() {
            bail!("symbolic link in Vibe source {}", parent.display());
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<Bytes> {
    check_path(path).with_context(|| format!("inspect Vibe source {}", path.display()))?;
    if !fs::symlink_metadata(path)?.is_file() {
        bail!("Vibe source is not a regular file: {}", path.display());
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read Vibe source {}", path.display()))?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        bail!(
            "Vibe document exceeds its store size limit: {}",
            path.display()
        );
    }
    Ok(Bytes::from_source(bytes))
}

fn decode<T: DeserializeOwned>(bytes: &Bytes, name: &str) -> Result<T> {
    // Serde's type diagnostics can quote field values. A source error should
    // identify only the document and position, even for malformed private data.
    serde_json::from_slice(bytes).map_err(|error| {
        anyhow::anyhow!(
            "invalid Vibe {name} at line {}, column {}",
            error.line(),
            error.column(),
        )
    })
}

pub(crate) fn valid_session_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, byte)| match i {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase(),
        })
}

fn number(name: &str) -> Option<u64> {
    if name.len() != 16 || !name.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

fn is_missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Select the source for a launch whose Vibe session UUID has been independently
/// identified. A previous run's export cannot establish that identity. A
/// different export UUID leaves only the expected native session eligible.
pub fn run_source(
    output_dir: &Path,
    vibe_home: &Path,
    session_id: &str,
) -> Result<Option<PathBuf>> {
    if !valid_session_id(session_id) {
        bail!("invalid expected Vibe session id");
    }
    let export = read_export(&output_dir.join("export.json"));
    match export {
        Ok(export)
            if export.session_id.as_deref() == Some(session_id)
                && export.journal_dir.as_deref() == Some("session") =>
        {
            match read_current(&output_dir.join("session"), session_id) {
                Ok(_) => return Ok(Some(output_dir.to_owned())),
                Err(error) if is_missing(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(_) => {}
        Err(error) if is_missing(&error) => {}
        Err(error) => return Err(error),
    }
    let native = vibe_home.join(NATIVE_SESSIONS).join(session_id);
    Ok(active_segment(&native)?.map(|_| native))
}

fn read_export(path: &Path) -> Result<Export> {
    decode_export(&read(path)?)
}

fn decode_export(bytes: &Bytes) -> Result<Export> {
    let export: Export = decode(bytes, "export")?;
    if export.schema_version != 1
        || export
            .session_id
            .as_deref()
            .is_some_and(|id| !valid_session_id(id))
        || export
            .journal_dir
            .as_deref()
            .is_some_and(|dir| dir != "session")
    {
        bail!("unsupported Vibe export schema or session reference");
    }
    if !matches!(
        export.outcome.as_str(),
        "finished"
            | "usage_error"
            | "config_error"
            | "infrastructure_failure"
            | "turn_limit"
            | "token_limit"
            | "price_limit"
            | "deadline"
            | "terminated"
            | "length"
            | "refusal"
            | "aborted"
    ) {
        bail!("unsupported Vibe export outcome");
    }
    Ok(export)
}

struct Store {
    current: Current,
    pointer: Bytes,
    active: u64,
}

fn decode_current(pointer: &Bytes, expected: &str) -> Result<Current> {
    let current: Current = decode(pointer, "CURRENT")?;
    if current.store_format != STORE_FORMAT
        || current.store_format_minor != 10
        || number(&current.generation).is_none()
        || current.session_id != expected
    {
        bail!("unsupported Vibe store format or session identity mismatch");
    }
    Ok(current)
}

fn read_current(root: &Path, expected: &str) -> Result<Current> {
    decode_current(&read(&root.join("CURRENT"))?, expected)
}

pub(crate) fn active_segment(root: &Path) -> Result<Option<PathBuf>> {
    let expected = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| valid_session_id(name))
        .context("Vibe native source must be its exact UUID directory")?;
    match read_current(root, expected) {
        Ok(current) => {
            let active = current
                .snapshot_sequence
                .checked_add(1)
                .context("Vibe sequence overflow")?;
            Ok(Some(
                root.join("journal").join(format!("{active:016}.jsonl")),
            ))
        }
        Err(error) if is_missing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

impl Store {
    fn open(
        root: &Path,
        expected: &str,
        read_input: &mut impl FnMut(&Path) -> Result<Bytes>,
    ) -> Result<Option<Self>> {
        let pointer = match read_input(&root.join("CURRENT")) {
            Ok(pointer) => pointer,
            Err(error) if is_missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let current = decode_current(&pointer, expected)?;
        let manifest = read_input(
            &root
                .join("generations")
                .join(&current.generation)
                .join("manifest.json"),
        )?;
        // The writer appends a newline to the canonical document on disk.
        // CURRENT hashes the canonical bytes before that terminator.
        let body = manifest
            .strip_suffix(b"\n")
            .context("unterminated Vibe manifest")?;
        if hex::encode(Sha256::digest(body)) != current.manifest_sha256 {
            bail!("Vibe generation manifest digest mismatch");
        }
        let manifest: Manifest = decode(&manifest, "manifest")?;
        let active = current
            .snapshot_sequence
            .checked_add(1)
            .context("Vibe sequence overflow")?;
        if manifest.manifest_version != 1
            || manifest.generation != current.generation
            || manifest.session_id != expected
            || manifest.snapshot_sequence != current.snapshot_sequence
            || manifest.recovery_journal_segment.first_sequence != active
            || manifest.recovery_journal_segment.path != format!("journal/{active:016}.jsonl")
        {
            bail!("Vibe manifest does not match CURRENT and its journal segment");
        }
        Ok(Some(Self {
            current,
            pointer,
            active,
        }))
    }

    fn segments(&self, root: &Path) -> Result<Vec<(u64, PathBuf)>> {
        let journal = root.join("journal");
        check_path(&journal)?;
        let mut segments = Vec::new();
        for entry in fs::read_dir(journal)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(sequence) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl"))
                .and_then(number)
                .filter(|sequence| *sequence <= self.active)
            else {
                continue;
            };
            segments.push((sequence, entry.path()));
        }
        segments.sort_by_key(|(sequence, _)| *sequence);
        if segments.last().map(|(sequence, _)| *sequence) != Some(self.active) {
            bail!("Vibe journal does not hold CURRENT's recovery segment");
        }
        Ok(segments)
    }
}

/// Project exactly one exported run directory or one native UUID directory.
/// An explicit export is never substituted for a native session or vice versa.
pub fn project_path(
    path: &Path,
    emit: impl FnMut(ProjectedSource) -> Result<()>,
) -> Result<ProjectionSummary> {
    project(path, None, read, emit)
}

/// Project the exact provider session captured for a run. Recheck that UUID
/// while reading the selected input so a reused output directory cannot
/// substitute a different session between selection and publication.
pub fn project_run(
    output_dir: &Path,
    vibe_home: &Path,
    session_id: &str,
    emit: impl FnMut(ProjectedSource) -> Result<()>,
) -> Result<Option<ProjectionSummary>> {
    run_source(output_dir, vibe_home, session_id)?
        .map(|path| project(&path, Some(session_id), read, emit))
        .transpose()
}

/// Identify the only session an exclusively allocated launch home holds.
/// When both sources name a session they must agree.
pub(crate) fn launched_session(output: &Path, home: &Path) -> Result<Option<String>> {
    let exported = match read_export(&output.join("export.json")) {
        Ok(export) => export.session_id,
        Err(error) if is_missing(&error) => None,
        Err(error) => return Err(error),
    };
    let native = home.join(NATIVE_SESSIONS);
    let entries = match check_path(&native).and_then(|()| fs::read_dir(&native).map_err(Into::into))
    {
        Ok(entries) => entries,
        Err(error) if is_missing(&error) => return Ok(exported),
        Err(error) => return Err(error),
    };
    let mut session = None;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().filter(|id| valid_session_id(id)) else {
            continue;
        };
        if !entry.file_type()?.is_dir() || session.is_some() {
            bail!("Vibe launch home does not identify one native session");
        }
        session = Some(id.to_owned());
    }
    if exported
        .as_ref()
        .zip(session.as_ref())
        .is_some_and(|(a, b)| a != b)
    {
        bail!("Vibe export and launch home identify different sessions");
    }
    Ok(exported.or(session))
}

/// Read one exact run and fingerprint the bytes actually passed to the parser.
/// File names, lengths and the selected source kind delimit the input.
pub(crate) fn project_run_inputs(
    output: &Path,
    home: &Path,
    session: &str,
    emit: impl FnMut(ProjectedSource) -> Result<()>,
) -> Result<Option<(ProjectionSummary, u64, blake3::Hash)>> {
    let Some(path) = run_source(output, home, session)? else {
        return Ok(None);
    };
    let mut digest = blake3::Hasher::new_derive_key("Faculties Vibe settlement source v1");
    digest.update(if path.as_path() == output {
        b"export"
    } else {
        b"native"
    });
    let mut total = 0;
    let summary = project(
        &path,
        Some(session),
        |input| {
            let bytes = read(input)?;
            let name = input.strip_prefix(&path)?.as_os_str().as_encoded_bytes();
            let len = bytes.len() as u64;
            digest.update(&(name.len() as u64).to_le_bytes());
            digest.update(name);
            digest.update(&len.to_le_bytes());
            digest.update(&bytes);
            total += len;
            Ok(bytes)
        },
        emit,
    )?;
    Ok(Some((summary, total, digest.finalize())))
}

fn project(
    path: &Path,
    expected: Option<&str>,
    mut read_input: impl FnMut(&Path) -> Result<Bytes>,
    emit: impl FnMut(ProjectedSource) -> Result<()>,
) -> Result<ProjectionSummary> {
    check_path(path).with_context(|| format!("inspect Vibe directory {}", path.display()))?;
    if !path.is_dir() {
        bail!("Vibe import needs an exact export or native session directory");
    }
    let export = if path
        .parent()
        .is_some_and(|path| path.ends_with(NATIVE_SESSIONS))
    {
        None
    } else {
        match read_input(&path.join("export.json")).and_then(|bytes| decode_export(&bytes)) {
            Ok(export) => Some(export),
            Err(error) if is_missing(&error) => None,
            Err(error) => return Err(error),
        }
    };
    let (root, mut summary) = match export {
        Some(export) => {
            let summary = ProjectionSummary {
                session_id: export.session_id.unwrap_or_default(),
                export_outcome: Some(export.outcome),
                ..ProjectionSummary::default()
            };
            if expected.is_some_and(|id| id != summary.session_id) {
                bail!("Vibe export changed after exact run selection");
            }
            if summary.session_id.is_empty() || export.journal_dir.is_none() {
                return Ok(summary);
            }
            (path.join("session"), summary)
        }
        None => {
            let session_id = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| valid_session_id(name))
                .context(
                    "Vibe export is missing; a native source must be its exact UUID directory",
                )?;
            if expected.is_some_and(|id| id != session_id) {
                bail!("Vibe native directory does not match the selected run");
            }
            (
                path.to_owned(),
                ProjectionSummary {
                    session_id: session_id.to_owned(),
                    ..ProjectionSummary::default()
                },
            )
        }
    };

    let Some(store) = Store::open(&root, &summary.session_id, &mut read_input)? else {
        return Ok(summary);
    };
    // The native index metadata may not yet exist after an interruption.
    // When present it must identify the same held session as CURRENT.
    match read_input(&root.join("meta.json")) {
        Ok(bytes) => {
            let meta: Metadata = decode(&bytes, "session metadata")?;
            if meta.session_id != store.current.session_id {
                bail!("Vibe session metadata and CURRENT identify different sessions");
            }
        }
        Err(error) if is_missing(&error) => {}
        Err(error) => return Err(error),
    }

    let mut scanner = journal::Scanner::default();
    let mut next = 1;
    for (first, segment) in store.segments(&root)? {
        if first != next {
            if first < next {
                bail!("overlapping Vibe journal segments");
            }
            summary.missing_sequence = Some(next);
            break;
        }
        let bytes = read_input(&segment)?;
        next = scanner.scan_segment(first, bytes, &mut summary)?;
        summary.segments += 1;
    }
    if read(&root.join("CURRENT"))? != store.pointer {
        bail!("Vibe CURRENT changed while selecting journal segments; retry");
    }
    let (records, pending) = scanner.finish();
    summary.pending_calls = pending;
    summary.stats = archive_source::project_records(
        SOURCE_VIBE,
        path,
        records,
        UnitAnnotations {
            source_locator: Some(format!("session/{}", summary.session_id)),
            ..UnitAnnotations::default()
        },
        emit,
    )?;
    Ok(summary)
}
