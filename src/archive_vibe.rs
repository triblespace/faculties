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
    let export = match read_export(&output_dir.join("export.json")) {
        Ok(export) => Some(export),
        Err(error) if is_missing(&error) => None,
        Err(error) => return Err(error),
    };
    if has_export_store(output_dir, session_id, export)? {
        return Ok(Some(output_dir.to_owned()));
    }
    let native = vibe_home.join(NATIVE_SESSIONS).join(session_id);
    Ok(active_segment(&native)?.map(|_| native))
}

fn has_export_store(output: &Path, session: &str, export: Option<Export>) -> Result<bool> {
    if !export.is_some_and(|export| {
        export.session_id.as_deref() == Some(session)
            && export.journal_dir.as_deref() == Some("session")
    }) {
        return Ok(false);
    }
    match read_current(&output.join("session"), session) {
        Ok(_) => Ok(true),
        Err(error) if is_missing(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_export(path: &Path) -> Result<Export> {
    decode_export(&read(path)?)
}

/// Unsupported export metadata cannot establish a session or an outcome. A
/// UUID it does name can still contradict the independently identified run.
fn settlement_export(path: &Path, expected: Option<&str>) -> Result<(Option<Export>, bool)> {
    let bytes = match read(path) {
        Ok(bytes) => bytes,
        Err(error) if is_missing(&error) => return Ok((None, false)),
        Err(error) => return Err(error),
    };
    let export = decode_export(&bytes).ok();
    let claimed = match &export {
        Some(export) => export.session_id.clone(),
        None => decode::<serde_json::Value>(&bytes, "export")
            .ok()
            .and_then(|json| {
                json.get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| valid_session_id(id))
                    .map(str::to_owned)
            }),
    };
    if claimed
        .as_deref()
        .zip(expected)
        .is_some_and(|(a, b)| a != b)
    {
        bail!("Vibe export and launch home identify different sessions");
    }
    let unsupported = export.is_none();
    Ok((export, unsupported))
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
    project(path, None, read, None, emit)
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
        .map(|path| project(&path, Some(session_id), read, None, emit))
        .transpose()
}

/// Identify the only session an exclusively allocated launch home holds.
/// When both sources name a session they must agree.
pub(crate) fn launched_session(output: &Path, home: &Path) -> Result<Option<String>> {
    let native = home.join(NATIVE_SESSIONS);
    let entries = match check_path(&native).and_then(|()| fs::read_dir(&native).map_err(Into::into))
    {
        Ok(entries) => Some(entries),
        Err(error) if is_missing(&error) => None,
        Err(error) => return Err(error),
    };
    let mut session = None;
    for entry in entries.into_iter().flatten() {
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
    let (export, _) = settlement_export(&output.join("export.json"), session.as_deref())?;
    Ok(export.and_then(|export| export.session_id).or(session))
}

/// A run-local receipt for the documents and for the validated journal prefix.
/// The journal digest covers complete records in sequence across all segments.
#[derive(Debug, Eq, PartialEq, serde::Serialize, Deserialize)]
pub(crate) struct RunCapture {
    pub bytes: u64,
    pub documents: String,
    pub records: usize,
    pub journal: String,
    pub complete: bool,
}

pub(crate) struct RunInputs {
    pub summary: ProjectionSummary,
    pub capture: RunCapture,
    pub unsupported_export: bool,
    journal_prefixes: Vec<blake3::Hash>,
}

impl RunInputs {
    pub(crate) fn extends(&self, held: &RunCapture) -> bool {
        held.records
            .checked_sub(1)
            .and_then(|index| self.journal_prefixes.get(index))
            .is_some_and(|digest| digest.to_hex().as_str() == held.journal)
    }
}

struct JournalProof {
    digest: blake3::Hasher,
    prefixes: Vec<blake3::Hash>,
}

impl JournalProof {
    fn record(&mut self, line: &[u8]) {
        self.digest.update(line);
        self.digest.update(b"\n");
        self.prefixes.push(self.digest.finalize());
    }
}

/// Read one exact run and fingerprint the bytes actually passed to the parser.
/// File names, lengths and the selected source kind delimit the input.
pub(crate) fn project_run_inputs(
    output: &Path,
    home: &Path,
    session: &str,
    emit: impl FnMut(ProjectedSource) -> Result<()>,
) -> Result<Option<RunInputs>> {
    if !valid_session_id(session) {
        bail!("invalid expected Vibe session id");
    }
    let (export, unsupported) = settlement_export(&output.join("export.json"), Some(session))?;
    if has_export_store(output, session, export)? {
        return read_run_inputs(output, session, true, emit).map(Some);
    }
    let mut native = project_native_inputs(home, session, emit)?;
    match native.as_mut() {
        Some(native) => native.unsupported_export = unsupported,
        None if unsupported => {
            bail!("Vibe export metadata is unsupported and no native source is readable")
        }
        None => {}
    }
    Ok(native)
}

pub(crate) fn project_native_inputs(
    home: &Path,
    session: &str,
    emit: impl FnMut(ProjectedSource) -> Result<()>,
) -> Result<Option<RunInputs>> {
    if !valid_session_id(session) {
        bail!("invalid expected Vibe session id");
    }
    let path = home.join(NATIVE_SESSIONS).join(session);
    if active_segment(&path)?.is_none() {
        return Ok(None);
    }
    read_run_inputs(&path, session, false, emit).map(Some)
}

fn read_run_inputs(
    path: &Path,
    session: &str,
    exported: bool,
    emit: impl FnMut(ProjectedSource) -> Result<()>,
) -> Result<RunInputs> {
    let mut digest = blake3::Hasher::new_derive_key("Faculties Vibe settlement source v1");
    digest.update(if exported { b"export" } else { b"native" });
    let mut total = 0;
    let mut proof = JournalProof {
        digest: blake3::Hasher::new_derive_key("Faculties Vibe settlement journal v1"),
        prefixes: Vec::new(),
    };
    let summary = project(
        path,
        Some(session),
        |input| {
            let bytes = read(input)?;
            let name = input.strip_prefix(path)?.as_os_str().as_encoded_bytes();
            let len = bytes.len() as u64;
            digest.update(&(name.len() as u64).to_le_bytes());
            digest.update(name);
            digest.update(&len.to_le_bytes());
            digest.update(&bytes);
            total += len;
            Ok(bytes)
        },
        Some(&mut proof),
        emit,
    )?;
    let capture = RunCapture {
        bytes: total,
        documents: digest.finalize().to_hex().to_string(),
        records: summary.journal_records,
        journal: proof.digest.finalize().to_hex().to_string(),
        complete: summary.coverage() == "complete",
    };
    Ok(RunInputs {
        summary,
        capture,
        unsupported_export: false,
        journal_prefixes: proof.prefixes,
    })
}

fn project(
    path: &Path,
    expected: Option<&str>,
    mut read_input: impl FnMut(&Path) -> Result<Bytes>,
    mut proof: Option<&mut JournalProof>,
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
        next = scanner.scan_segment(first, bytes, &mut summary, proof.as_deref_mut())?;
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
