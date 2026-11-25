//! Host-local session discovery and incremental ingestion. This is an mtime
//! optimization, not a catalogue of Archive contents or proof of byte coverage.
//! Moving/backdating old files requires an explicit `All` pass. Scheduling and
//! machine-wide admission/resource limits belong to the caller.
//!
//! A file written within the quiet window is LIVE, not deferred: agent windows
//! keep one conversation for ever, so their transcripts never go quiet. A live
//! file is imported up to its last finished record (the native importer holds
//! back a trailing partial line and whatever its message does not yet show
//! complete) and is selected again on the next pass, because its modification
//! time is at or after this pass's cutoff, which is where progress stops.
//! Node identity is structural, so the next import converges with this one and
//! only appends. Quiet age decides only when a held-back tail is flushed.
use super::{Archive, ImportReceipt, ImportSource};
use crate::archive_agy::{BRAIN_PATH, TRANSCRIPT_PATH};
use crate::archive_collection::ArchiveImportWriter;
use crate::archive_vibe;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use triblespace::core::collection::{
    CollectionCommit, CollectionData, CollectionHandle, CollectionRead, CollectionRecordSelector,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum SessionSource {
    ClaudeCode,
    Codex,
    Agy,
    AgyZooids,
    VibeZooids,
}
impl SessionSource {
    fn is_agy(self) -> bool {
        matches!(self, Self::Agy | Self::AgyZooids)
    }
}
impl From<SessionSource> for ImportSource {
    fn from(source: SessionSource) -> Self {
        match source {
            SessionSource::ClaudeCode => Self::ClaudeCode,
            SessionSource::Codex => Self::Codex,
            SessionSource::Agy | SessionSource::AgyZooids => Self::Agy,
            SessionSource::VibeZooids => Self::Vibe,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRoot {
    pub source: SessionSource,
    pub path: PathBuf,
}
#[derive(Clone, Copy, Debug)]
pub enum SessionStart {
    Incremental,
    All,
    Since(SystemTime),
}
#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub roots: Vec<SessionRoot>,
    pub state_dir: PathBuf,
    pub start: SessionStart,
    pub quiet: Duration,
    /// One fixed clock observation for selection and native finished-before.
    pub now: SystemTime,
}
#[derive(Debug)]
pub struct SessionReport {
    pub imported: usize,
    /// Of `imported`, files written within the quiet window: imported up to
    /// their last finished record and selected again next pass.
    pub live: usize,
    pub unchanged: usize,
    pub other_files: usize,
    pub pieces: usize,
    /// Counts publication work, including already resident COMMITs.
    pub commits: usize,
    pub cutoff: SystemTime,
    pub collection: String,
    /// The pile at the bound path is a different file than last time (a
    /// compaction swap) and still holds the last COMMIT this checkpoint
    /// published, so progress continued and the checkpoint now names it.
    pub rebound: bool,
}
pub enum SessionEvent<'a> {
    /// Precedes `Importing` for a file written within the quiet window.
    Live(&'a Path),
    Importing(&'a Path),
    Imported {
        path: &'a Path,
        receipt: &'a ImportReceipt,
    },
    /// Delivered before checkpoint replacement: output failure leaves progress alone.
    Complete(&'a SessionReport),
}
#[derive(Debug)]
pub struct SessionBusy;
impl std::fmt::Display for SessionBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session ingestion is busy (host-local ingestion lock; not GB10 admission)"
        )
    }
}
impl std::error::Error for SessionBusy {}

/// Conventional local roots, including archived Codex rollouts and Agy's
/// window and zooid transcripts and native Vibe zooid journals. Newly bound
/// provider roots get their own first pass.
pub fn default_roots(home: &Path, codex_home: Option<&Path>) -> Result<Vec<SessionRoot>> {
    let mut roots = Vec::new();
    let mut add = |source, path: PathBuf| -> Result<()> {
        match fs::symlink_metadata(&path) {
            Ok(_) => roots.push(SessionRoot { source, path }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).with_context(|| format!("inspect root {:?}", path)),
        }
        Ok(())
    };
    add(SessionSource::ClaudeCode, home.join(".claude/projects"))?;
    add(SessionSource::Agy, home.join(BRAIN_PATH))?;
    add(
        SessionSource::AgyZooids,
        crate::spawn::LaunchConfig::under_home(home).state_dir,
    )?;
    add(
        SessionSource::VibeZooids,
        crate::spawn::LaunchConfig::under_home(home).state_dir,
    )?;
    for entry in fs::read_dir(home).with_context(|| format!("enumerate home {:?}", home))? {
        let entry = entry.with_context(|| format!("enumerate home {:?}", home))?;
        if entry.file_name().as_encoded_bytes().starts_with(b".codex") {
            let kind = entry
                .file_type()
                .with_context(|| format!("inspect Codex home {:?}", entry.path()))?;
            if kind.is_symlink() {
                bail!("symlink Codex home refused: {:?}", entry.path());
            }
            if !kind.is_dir() {
                continue;
            }
            for name in ["sessions", "archived_sessions"] {
                add(SessionSource::Codex, entry.path().join(name))?;
            }
        }
    }
    if let Some(home) = codex_home {
        for name in ["sessions", "archived_sessions"] {
            add(SessionSource::Codex, home.join(name))?;
        }
    }
    normalize_roots(roots)
}

fn no_symlink_components(path: &Path) -> Result<()> {
    for parent in path.ancestors() {
        if parent.as_os_str().is_empty() {
            continue;
        }
        let metadata =
            fs::symlink_metadata(parent).with_context(|| format!("inspect path {:?}", parent))?;
        if metadata.file_type().is_symlink() {
            bail!("symlink path refused: {:?}", parent);
        }
    }
    Ok(())
}

fn normalize_roots(roots: Vec<SessionRoot>) -> Result<Vec<SessionRoot>> {
    let mut normalized = Vec::new();
    for mut root in roots {
        no_symlink_components(&root.path)?;
        if !fs::metadata(&root.path)?.is_dir() {
            bail!("session root is not a directory: {:?}", root.path);
        }
        root.path = fs::canonicalize(&root.path)?;
        normalized.push(root);
    }
    normalized.sort_by(|a, b| a.path.cmp(&b.path).then(a.source.cmp(&b.source)));
    let mut unique: Vec<SessionRoot> = Vec::new();
    for root in normalized {
        if let Some(parent) = unique.iter().find(|p| {
            root.path.starts_with(&p.path)
                && !(root.path == p.path
                    && p.source == SessionSource::AgyZooids
                    && root.source == SessionSource::VibeZooids)
        }) {
            if parent.source != root.source {
                bail!(
                    "overlapping roots have different adapters: {:?} and {:?}",
                    parent.path,
                    root.path
                );
            }
            continue;
        }
        unique.push(root);
    }
    unique.sort_by(|a, b| a.source.cmp(&b.source).then(a.path.cmp(&b.path)));
    Ok(unique)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}
fn identity(meta: &fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        FileIdentity {}
    }
}
#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BoundPath {
    name: Vec<u8>,
    identity: FileIdentity,
}
fn bound_path(path: &Path) -> Result<BoundPath> {
    let canonical = fs::canonicalize(path).with_context(|| format!("resolve {:?}", path))?;
    Ok(BoundPath {
        name: canonical.as_os_str().as_encoded_bytes().to_vec(),
        identity: identity(&fs::metadata(&canonical)?),
    })
}
#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Binding {
    pile: BoundPath,
    collection: String,
    roots: Vec<(SessionSource, BoundPath)>,
}
impl Binding {
    /// A newly bound provider source starts without applying the old cutoff to it.
    /// All previously bound roots must still match, including their identity.
    fn added_discovery_sources(&self, next: &Self) -> Option<Vec<SessionSource>> {
        let mut existing = self.roots.iter().peekable();
        let mut added = Vec::new();
        for root in &next.roots {
            if existing.next_if(|old| *old == root).is_some() {
                continue;
            }
            let source = root.0;
            if !(source.is_agy() || source == SessionSource::VibeZooids)
                || added.contains(&source)
                || self.roots.iter().any(|old| old.0 == source)
            {
                return None;
            }
            added.push(source);
        }
        existing.peek().is_none().then_some(added)
    }
}
#[derive(Serialize, Deserialize)]
struct Checkpoint {
    binding: Binding,
    through: SystemTime,
    /// What the pile must still hold for this progress to carry over to a new
    /// file at the same path. Absent in checkpoints written before it existed.
    #[serde(default)]
    last: Option<LastPublication>,
}
/// The newest COMMIT a successful pass appended, by its data member and its
/// record fingerprint. Only a COMMIT the pile did not hold before its pass
/// counts: identical COMMITs deduplicate, so a republished one sits at an old
/// position and is no evidence of what was appended after it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct LastPublication {
    data: String,
    fingerprint: String,
}
impl LastPublication {
    fn of(commit: &CollectionCommit) -> Self {
        Self {
            data: hex::encode(commit.data().raw),
            fingerprint: hex::encode(commit.fingerprint().raw()),
        }
    }
    /// Whether this exact signed COMMIT is among the snapshot's records.
    fn held_by<S: CollectionRead>(
        &self,
        snapshot: &S,
        collection: CollectionHandle,
    ) -> Result<bool> {
        let raw = |hex_text: &str| -> Result<[u8; 32]> {
            hex::decode(hex_text)
                .ok()
                .and_then(|raw| raw.try_into().ok())
                .ok_or_else(|| anyhow!("session checkpoint names a malformed last publication"))
        };
        let (data, fingerprint) = (raw(&self.data)?, raw(&self.fingerprint)?);
        holds(snapshot, collection, CollectionData::new(data), fingerprint)
    }
}
/// Whether the snapshot holds the COMMIT with this data member and record
/// fingerprint, answered from the member index.
fn holds<S: CollectionRead>(
    snapshot: &S,
    collection: CollectionHandle,
    data: CollectionData,
    fingerprint: [u8; 32],
) -> Result<bool> {
    let selectors = BTreeSet::from([CollectionRecordSelector::CommitMember(collection, data)]);
    let records = snapshot
        .select_records(&selectors)
        .map_err(|error| anyhow!("look up a published COMMIT: {error}"))?;
    Ok(records
        .iter()
        .any(|record| record.fingerprint().raw() == fingerprint))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Observation {
    identity: FileIdentity,
    len: u64,
    modified: SystemTime,
}
fn observe_file(path: &Path) -> Result<Observation> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("stat {:?}", path))?;
    if !metadata.is_file() {
        bail!("nonregular source refused: {:?}", path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!("hard-linked source refused: {:?}", path);
        }
    }
    File::open(path).with_context(|| format!("open {:?}", path))?;
    Ok(Observation {
        identity: identity(&metadata),
        len: metadata.len(),
        modified: metadata.modified()?,
    })
}
fn sidecar(path: &Path, source: SessionSource) -> Result<Option<Observation>> {
    let path = match source {
        SessionSource::ClaudeCode => path.with_extension("meta.json"),
        SessionSource::VibeZooids => {
            return observe_file(&vibe_session(path)?.join("CURRENT")).map(Some)
        }
        _ => return Ok(None),
    };
    match fs::symlink_metadata(&path) {
        Ok(_) => observe_file(&path).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("inspect sidecar {:?}", path)),
    }
}

fn vibe_session(segment: &Path) -> Result<&Path> {
    segment
        .parent()
        .filter(|path| path.ends_with("journal"))
        .and_then(Path::parent)
        .context("Vibe discovery candidate is not a journal segment")
}

struct Candidate {
    source: SessionSource,
    path: PathBuf,
    observed: Observation,
    sidecar: Option<Observation>,
}
impl Candidate {
    fn observe(path: PathBuf, source: SessionSource) -> Result<Self> {
        Ok(Self {
            source,
            observed: observe_file(&path)?,
            sidecar: sidecar(&path, source)?,
            path,
        })
    }

    fn modified(&self) -> SystemTime {
        self.sidecar.as_ref().map_or(self.observed.modified, |s| {
            s.modified.max(self.observed.modified)
        })
    }

    fn source_path(&self) -> Result<&Path> {
        if self.source == SessionSource::VibeZooids {
            vibe_session(&self.path)
        } else {
            Ok(&self.path)
        }
    }
    /// The file is the one discovered, appended to at most. Appends during
    /// the pass are next-pass work: they post-date the cutoff, and the
    /// importer reads one frozen prefix. Replacement, truncation, a same-size
    /// rewrite, or a changed sidecar abort before publication.
    fn check(&self) -> Result<()> {
        no_symlink_components(&self.path)?;
        let now = observe_file(&self.path)?;
        let appended = now.identity == self.observed.identity
            && (now == self.observed || now.len > self.observed.len);
        if !appended || sidecar(&self.path, self.source)? != self.sidecar {
            bail!(
                "source or sidecar changed since discovery other than by appending: {:?}; rerun explicitly",
                self.path
            );
        }
        if self.source == SessionSource::VibeZooids
            && archive_vibe::active_segment(self.source_path()?)?.as_deref() != Some(&self.path)
        {
            bail!(
                "Vibe active journal changed since discovery: {:?}",
                self.path
            );
        }
        Ok(())
    }
}

fn discover_agy(root: &Path, source: SessionSource, files: &mut Vec<Candidate>) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("read directory {:?}", root))? {
        let entry = entry.with_context(|| format!("enumerate {:?}", root))?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join(TRANSCRIPT_PATH);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                no_symlink_components(&path)?;
                files.push(Candidate::observe(path, source)?);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).with_context(|| format!("stat {:?}", path)),
        }
    }
    Ok(())
}

fn discover_agy_zooids(root: &SessionRoot, files: &mut Vec<Candidate>) -> Result<()> {
    for entry in
        fs::read_dir(&root.path).with_context(|| format!("read directory {:?}", root.path))?
    {
        let entry = entry.with_context(|| format!("enumerate {:?}", root.path))?;
        if Path::new(&entry.file_name())
            .extension()
            .is_none_or(|ext| ext != "agy")
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let brain = entry.path().join(BRAIN_PATH);
        match fs::symlink_metadata(&brain) {
            Ok(_) => {
                no_symlink_components(&brain)?;
                discover_agy(&brain, root.source, files)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).with_context(|| format!("stat {:?}", brain)),
        }
    }
    Ok(())
}

fn discover_vibe_zooids(root: &SessionRoot, files: &mut Vec<Candidate>) -> Result<()> {
    for home in fs::read_dir(&root.path)? {
        let home = home?;
        let path = home.path();
        if path.extension().is_none_or(|ext| ext != "vibe") || !home.file_type()?.is_dir() {
            continue;
        }
        let native = path.join(archive_vibe::NATIVE_SESSIONS);
        match no_symlink_components(&native) {
            Ok(()) => {}
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue
            }
            Err(error) => return Err(error),
        }
        for session in fs::read_dir(native)? {
            let session = session?;
            if !session
                .file_name()
                .to_str()
                .is_some_and(archive_vibe::valid_session_id)
                || !session.file_type()?.is_dir()
            {
                continue;
            }
            if let Some(segment) = archive_vibe::active_segment(&session.path())? {
                no_symlink_components(&segment)?;
                files.push(Candidate::observe(segment, root.source)?);
            }
        }
    }
    Ok(())
}

fn discover(roots: &[SessionRoot]) -> Result<(Vec<Candidate>, usize)> {
    // This is a transient file list for one observation, never a retained catalogue.
    let mut files = Vec::new();
    let mut other = 0;
    for root in roots {
        if root.source == SessionSource::VibeZooids {
            let start = files.len();
            discover_vibe_zooids(root, &mut files)?;
            files[start..].sort_by(|a, b| a.path.cmp(&b.path));
            continue;
        }
        if root.source.is_agy() {
            let start = files.len();
            if root.source == SessionSource::AgyZooids {
                discover_agy_zooids(root, &mut files)?;
            } else {
                discover_agy(&root.path, root.source, &mut files)?;
            }
            files[start..].sort_by(|a, b| a.path.cmp(&b.path));
            continue;
        }
        let mut pending = vec![root.path.clone()];
        while let Some(path) = pending.pop() {
            let metadata =
                fs::symlink_metadata(&path).with_context(|| format!("stat {:?}", path))?;
            if metadata.is_dir() {
                let mut children = fs::read_dir(&path)
                    .with_context(|| format!("read directory {:?}", path))?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<Vec<_>>>()
                    .with_context(|| format!("enumerate {:?}", path))?;
                children.sort();
                pending.extend(children.into_iter().rev());
            } else if metadata.is_file() {
                if path.extension().is_some_and(|e| e == "jsonl") {
                    files.push(Candidate::observe(path, root.source)?);
                } else {
                    other += 1;
                }
            } else {
                bail!("symlink or special entry refused: {:?}", path);
            }
        }
    }
    Ok((files, other))
}

fn state_file(path: &Path) -> Result<Option<Checkpoint>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => bail!("nonregular checkpoint refused: {:?}", path),
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("stat checkpoint {:?}", path)),
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        bail!("oversized checkpoint: {:?}", path);
    }
    Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
        format!("decode checkpoint {:?}", path)
    })?))
}

impl Archive {
    /// Run one finite folder observation. A failed file preserves previous
    /// publications but never advances local progress. Native per-file receipts
    /// retain tail/held-back qualifications; success is not complete-byte coverage.
    /// Explicitly flushes publication before progress, including with shared
    /// storage, but never closes a caller-owned shared store.
    pub fn ingest_sessions(
        &self,
        options: &SessionOptions,
        mut event: impl FnMut(SessionEvent<'_>) -> Result<()>,
    ) -> Result<SessionReport> {
        let cutoff = options
            .now
            .checked_sub(options.quiet)
            .ok_or_else(|| anyhow!("quiet age precedes clock epoch"))?;
        fs::create_dir_all(&options.state_dir).context("create session progress directory")?;
        no_symlink_components(&options.state_dir)?;
        let lock_path = options.state_dir.join("ingest-sessions.lock");
        let mut open = OpenOptions::new();
        open.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock = open
            .open(&lock_path)
            .context("open session ingestion lock")?;
        if !lock.metadata()?.is_file() {
            bail!("nonregular session lock");
        }
        match lock.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => return Err(SessionBusy.into()),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).context("lock session ingestion")
            }
        }
        let checkpoint_path = options.state_dir.join("ingest-sessions.json");
        let previous = state_file(&checkpoint_path)?;
        if matches!(options.start, SessionStart::Incremental) && previous.is_none() {
            bail!("no bound session checkpoint; choose --all or --since explicitly (legacy stamps are not inherited)");
        }
        let roots = normalize_roots(options.roots.clone())?;
        if roots.is_empty() {
            bail!("no session roots selected");
        }
        let root_binding = roots
            .iter()
            .map(|r| Ok((r.source, bound_path(&r.path)?)))
            .collect::<Result<Vec<_>>>()?;
        let (files, other_files) = discover(&roots)?;
        let pile_binding = bound_path(self.storage.path())?;
        let (report, checkpoint) = self.storage.with_store(|store, signer, runtime| {
            use triblespace::core::repo::SnapshotSource;
            let start = store.snapshot().context("freeze the pile before the session pass")?;
            let mut writer = ArchiveImportWriter::from_store_with_storage(&mut *store, signer, runtime.clone(), &self.storage)?;
            let collection = hex::encode(writer.collection_handle().raw);
            let binding = Binding { pile: pile_binding, collection: collection.clone(), roots: root_binding };
            let mut rebound = false;
            let (since, added_sources) = match options.start {
                SessionStart::All => (None, Vec::new()),
                SessionStart::Since(time) => (Some(time), Vec::new()),
                SessionStart::Incremental => {
                    let previous = previous.as_ref().expect("checked checkpoint");
                    if previous.binding.pile.name != binding.pile.name
                        || previous.binding.collection != binding.collection
                    {
                        bail!("session destination or roots changed; choose --all or --since explicitly");
                    }
                    let added_sources = previous.binding.added_discovery_sources(&binding).ok_or_else(|| {
                        anyhow!("session destination or roots changed; choose --all or --since explicitly")
                    })?;
                    if previous.binding.pile.identity != binding.pile.identity {
                        // A compacted pile must still hold the last publication.
                        let held = match &previous.last {
                            Some(last) => last.held_by(&start, writer.collection_handle())?,
                            None => false,
                        };
                        if !held { bail!("session destination or roots changed; choose --all or --since explicitly"); }
                        rebound = true;
                    }
                    (Some(previous.through), added_sources)
                }
            };
            if since.is_some_and(|time| time > cutoff) { bail!("session lower bound is after eligibility cutoff (clock moved backwards or --since is too recent)"); }
            let mut last = previous
                .as_ref()
                .filter(|p| p.binding.collection == binding.collection && p.binding.pile.name == binding.pile.name)
                .and_then(|p| p.last.clone());
            let mut report = SessionReport { imported: 0, live: 0, unchanged: 0, other_files,
                pieces: 0, commits: 0, cutoff, collection, rebound };
            for file in &files {
                // Inclusive lower edge intentionally rechecks the prior boundary.
                // A file live in the previous pass is at or after it by definition.
                let lower_bound = if added_sources.contains(&file.source) {
                    None
                } else {
                    since
                };
                if lower_bound.is_some_and(|time| file.modified() < time) { report.unchanged += 1; continue; }
                let live = file.modified() >= cutoff;
                let result = (|| {
                    let source_path = file.source_path()?;
                    if live { event(SessionEvent::Live(source_path))?; }
                    event(SessionEvent::Importing(source_path))?;
                    file.check()?;
                    let summary = super::local::project_path(source_path, file.source.into(), Some(cutoff), |fragment, identity, _| writer.stage(fragment, identity))?;
                    file.check()?;
                    let publication = writer.commit_unit()?;
                    for commit in publication.commits.iter().rev() {
                        if !holds(&start, commit.collection(), commit.data(), commit.fingerprint().raw())? {
                            last = Some(LastPublication::of(commit));
                            break;
                        }
                    }
                    report.pieces += publication.pieces;
                    report.commits += publication.commits.len();
                    report.imported += 1;
                    report.live += usize::from(live);
                    event(SessionEvent::Imported { path: source_path, receipt: &ImportReceipt { summary, publication } })
                })();
                result.with_context(|| format!("import {:?}; checkpoint unchanged", file.path))?;
            }
            // Detect root replacement during the observation; new files and growth
            // after discovery are next-pass work, not continuous coverage.
            for (root, (_, observed)) in roots.iter().zip(&binding.roots) {
                no_symlink_components(&root.path)?;
                if bound_path(&root.path)? != *observed { bail!("session root changed: {:?}", root.path); }
            }
            drop(writer);
            // with_store closes one-shot storage, but shared owners deliberately
            // keep their store open. Progress requires an explicit flush in BOTH
            // cases, not an inference from the closure returning successfully.
            use triblespace::core::repo::StorageFlush;
            store.flush().context("flush session publications before checkpoint")?;
            Ok((report, Checkpoint { binding, through: cutoff, last }))
        })?;
        // Explicit flush, one-shot close (if owned), and caller output precede progress.
        event(SessionEvent::Complete(&report))?;
        let temporary = options
            .state_dir
            .join(format!("ingest-sessions.{}.tmp", std::process::id()));
        let mut open = OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let mut file = open
            .open(&temporary)
            .context("create provisional session checkpoint")?;
        file.write_all(&serde_json::to_vec(&checkpoint)?)?;
        file.sync_all()?;
        fs::rename(&temporary, &checkpoint_path)
            .context("replace successful session checkpoint")?;
        // The lock file is never unlinked. Closing releases the OS lock, including
        // after a crash; another process must not lock a replacement inode.
        drop(lock);
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ImportSummary;
    use crate::out::Out;
    use clap::Parser;
    use std::time::UNIX_EPOCH;

    const CODEX: &str = concat!(
        "{\"timestamp\":\"2026-08-16T08:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child\",\"session_id\":\"parent\",\"parent_thread_id\":\"parent\"}}\n",
        "{\"timestamp\":\"2026-08-16T08:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"synthetic session\"}}\n"
    );
    const CLAUDE: &str = "{\"type\":\"user\",\"sessionId\":\"synthetic\",\"uuid\":\"first\",\"message\":{\"role\":\"user\",\"content\":\"synthetic Claude\"}}\n";
    const AGY: &str =
        "{\"source\":\"USER_INPUT\",\"content\":\"synthetic Agy\",\"step_index\":1}\n";
    const AGY_SESSION: &str = "aaaaaaaa-aaaa-4aaa-8aaa-000000000001";
    fn time(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }
    struct Fixture {
        directory: tempfile::TempDir,
        archive: Archive,
        options: SessionOptions,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let pile = directory.path().join("test.pile");
            let key = directory.path().join("test.key");
            File::create(&pile).unwrap();
            crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
            let root = directory.path().join("sessions");
            fs::create_dir(&root).unwrap();
            let options = SessionOptions {
                roots: vec![SessionRoot {
                    source: SessionSource::Codex,
                    path: root,
                }],
                state_dir: directory.path().join("state"),
                start: SessionStart::All,
                now: time(10000),
                quiet: Duration::from_secs(3600),
            };
            Self {
                directory,
                archive: Archive::new(pile, Some(key)),
                options,
            }
        }
        fn file(&self, name: &str, body: &str, modified: u64) -> PathBuf {
            let path = self.options.roots[0].path.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(time(modified))
                .unwrap();
            path
        }
        fn run(&self) -> Result<SessionReport> {
            self.archive.ingest_sessions(&self.options, |_| Ok(()))
        }
        fn checkpoint(&self) -> Vec<u8> {
            fs::read(self.options.state_dir.join("ingest-sessions.json")).unwrap()
        }
        fn facts(&self) -> triblespace::core::trible::TribleSet {
            use crate::storage::FactView;
            let view = pollster::block_on(crate::archive_collection::ensure_local(
                self.archive.storage.path(),
                self.archive.storage.key_path(),
            ))
            .unwrap();
            let mut facts = triblespace::core::trible::TribleSet::new();
            for fact in view.facts().unwrap().iter() {
                facts.insert(&fact);
            }
            facts
        }
    }

    #[test]
    fn live_file_is_imported_and_seen_again_next_time_without_another_write() {
        let mut f = Fixture::new();
        f.file("old.jsonl", CODEX, 1000);
        f.file("recent.jsonl", CODEX, 8000);
        let first = f.run().unwrap();
        assert_eq!((first.imported, first.live), (2, 1));
        assert_eq!(
            state_file(&f.options.state_dir.join("ingest-sessions.json"))
                .unwrap()
                .unwrap()
                .through,
            time(6400)
        );
        f.options.start = SessionStart::Incremental;
        f.options.now = time(14000);
        let next = f.run().unwrap();
        assert_eq!((next.imported, next.live, next.unchanged), (1, 0, 1));
    }

    #[test]
    fn old_checkpoint_imports_older_agy_history_and_keeps_other_roots_progress() {
        let mut f = Fixture::new();
        let home = f.directory.path().join("home");
        fs::create_dir_all(home.join(".codex/sessions")).unwrap();
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        f.options.roots = default_roots(&home, None).unwrap();
        assert_eq!(f.options.roots.len(), 2);
        f.file("original.jsonl", CLAUDE, 1000);
        fs::write(home.join(".codex/sessions/rollout.jsonl"), CODEX).unwrap();
        File::open(home.join(".codex/sessions/rollout.jsonl"))
            .unwrap()
            .set_modified(time(1000))
            .unwrap();
        assert_eq!(f.run().unwrap().imported, 2);
        let before = f.facts();

        let brain = home.join(BRAIN_PATH);
        let full = brain.join(AGY_SESSION).join(TRANSCRIPT_PATH);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, AGY).unwrap();
        File::open(&full).unwrap().set_modified(time(1000)).unwrap();
        for path in [
            full.with_file_name("transcript.jsonl"),
            full.with_file_name("other.jsonl"),
            full.with_file_name("chunks").join("part.jsonl"),
            brain.join(AGY_SESSION).join("transcript_full.jsonl"),
            brain.join(AGY_SESSION).join("nested").join(TRANSCRIPT_PATH),
            brain.join("index.sqlite"),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "not a transcript\n").unwrap();
        }

        f.options.roots = default_roots(&home, None).unwrap();
        f.options.now = time(16000);
        f.options.start = SessionStart::Incremental;
        let migrated = f.run().unwrap();
        assert_eq!((migrated.imported, migrated.unchanged), (1, 2));
        let after = f.facts();
        assert!(before.difference(&after).is_empty());
        assert!(after.len() > before.len());
        let next = f.run().unwrap();
        assert_eq!((next.imported, next.unchanged), (0, 3));
    }

    #[test]
    fn new_agy_zooid_homes_do_not_change_the_checkpoint_binding() {
        let mut f = Fixture::new();
        let home = f.directory.path().join("home");
        let spawn = crate::spawn::LaunchConfig::under_home(&home).state_dir;
        fs::create_dir_all(&spawn).unwrap();
        f.options.roots = default_roots(&home, None).unwrap();
        assert_eq!(f.options.roots[0].source, SessionSource::AgyZooids);
        assert_eq!(f.run().unwrap().imported, 0);
        let checkpoint = f.options.state_dir.join("ingest-sessions.json");
        let before = state_file(&checkpoint).unwrap().unwrap().binding;

        let first = f.file(
            &format!("first.agy/{BRAIN_PATH}/{AGY_SESSION}/{TRANSCRIPT_PATH}"),
            AGY,
            8000,
        );
        let second = f.file(
            &format!("second.agy/{BRAIN_PATH}/two/{TRANSCRIPT_PATH}"),
            &AGY.replace("synthetic Agy", "another conversation"),
            8100,
        );
        f.file(
            &format!("another.codex/{BRAIN_PATH}/{AGY_SESSION}/{TRANSCRIPT_PATH}"),
            "ignore me",
            8100,
        );

        f.options.roots = default_roots(&home, None).unwrap();
        f.options.start = SessionStart::Incremental;
        f.options.now = time(13000);
        let mut imported = Vec::new();
        let report = f
            .archive
            .ingest_sessions(&f.options, |event| {
                if let SessionEvent::Imported { path, .. } = event {
                    imported.push(path.to_owned());
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(report.imported, 2);
        assert_eq!(imported, [first, second]);
        assert_eq!(state_file(&checkpoint).unwrap().unwrap().binding, before);
    }

    #[test]
    fn adding_vibe_to_the_same_spawn_root_backfills_only_vibe_and_replays_explicit_import() {
        use crate::archive_vibe::tests::{finished_native, SESSION};

        let mut f = Fixture::new();
        let home = f.directory.path().join("home");
        let spawn = crate::spawn::LaunchConfig::under_home(&home).state_dir;
        let codex = home.join(".codex/sessions");
        let claude = home.join(".claude/projects");
        let agy = spawn.join(format!(
            "one.agy/{BRAIN_PATH}/{AGY_SESSION}/{TRANSCRIPT_PATH}"
        ));
        for (path, text) in [
            (codex.join("old.jsonl"), CODEX),
            (claude.join("old.jsonl"), CLAUDE),
            (agy, AGY),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            File::open(&path).unwrap().set_modified(time(1000)).unwrap();
        }

        // The checkpoint predates Vibe, but already has AgyZooids tied to
        // the same physical directory. Its three cutoffs must still apply.
        f.options.roots = default_roots(&home, None).unwrap();
        f.options
            .roots
            .retain(|root| root.source != SessionSource::VibeZooids);
        assert_eq!(f.run().unwrap().imported, 3);

        let native = spawn
            .join("one.vibe")
            .join(archive_vibe::NATIVE_SESSIONS)
            .join(SESSION);
        finished_native(&native);
        for file in [
            native.join("CURRENT"),
            native.join("journal/0000000000000005.jsonl"),
        ] {
            File::open(file).unwrap().set_modified(time(2000)).unwrap();
        }
        let mut output = String::new();
        super::super::cli::import_paths(
            f.archive.storage.path(),
            f.archive.storage.key_path(),
            std::slice::from_ref(&native),
            super::super::cli::CliImportSource::Vibe,
            None,
            &mut crate::out::Out::new(&mut |part| {
                if let crate::out::Part::Text { text } = part {
                    output.push_str(&text);
                }
                Ok(())
            }),
        )
        .unwrap();
        assert!(output.contains("coverage=partial"));
        assert!(output.contains("nodes=6"));
        let facts = f.facts();
        let size = fs::metadata(f.archive.storage.path()).unwrap().len();

        f.options.roots = default_roots(&home, None).unwrap();
        f.options.now = time(16000);
        f.options.start = SessionStart::Incremental;
        let migrated = f.run().unwrap();
        assert_eq!((migrated.imported, migrated.unchanged), (1, 3));
        assert_eq!(f.facts(), facts);
        assert_eq!(fs::metadata(f.archive.storage.path()).unwrap().len(), size);
        let next = f.run().unwrap();
        assert_eq!((next.imported, next.unchanged), (0, 4));

        let mut thread = String::new();
        f.archive
            .thread(
                &format!("session/{SESSION}"),
                10,
                None,
                crate::archive::Reading {
                    provider: None,
                    force: false,
                },
                &mut crate::out::Out::new(&mut |part| {
                    if let crate::out::Part::Text { text } = part {
                        thread.push_str(&text);
                    }
                    Ok(())
                }),
            )
            .unwrap();
        let ordered: Vec<_> = ["synthetic rules", "synthetic question", "synthetic answer"]
            .iter()
            .map(|text| {
                thread
                    .find(text)
                    .unwrap_or_else(|| panic!("missing {text} from {thread}"))
            })
            .collect();
        assert!(ordered.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn agy_live_prefix_replays_and_appends_like_explicit_import() {
        let mut f = Fixture::new();
        f.options.roots[0].source = SessionSource::Agy;
        f.options.roots[0].path = f.directory.path().join(BRAIN_PATH);
        let tail = r#"{"source":"MODEL","type":"PLANNER_RESPONSE","content":"synthetic"#;
        let full = f.file(
            &format!("{AGY_SESSION}/{TRANSCRIPT_PATH}"),
            &format!("{AGY}{tail}"),
            8000,
        );
        let locator = format!("session/{AGY_SESSION}");
        let last = || {
            let mut output = String::new();
            f.archive
                .thread(
                    &locator,
                    10,
                    Some(1),
                    crate::archive::Reading {
                        force: false,
                        provider: None,
                    },
                    &mut Out::new(&mut |part| {
                        if let crate::out::Part::Text { text } = part {
                            output.push_str(&text);
                        }
                        Ok(())
                    }),
                )
                .unwrap();
            assert!(output.starts_with(&format!("conversation: {locator}, newest held leaf ")));
            output
        };
        let first = f
            .archive
            .ingest_sessions(&f.options, |event| {
                if let SessionEvent::Imported { receipt, .. } = event {
                    let ImportSummary::Agy(summary) = &receipt.summary else {
                        panic!("expected Agy receipt");
                    };
                    assert_eq!(summary.trailing_bytes_held, tail.len());
                    assert_eq!(summary.stats.records_seen, 1);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!((first.imported, first.live), (1, 1));
        assert!(last().contains("synthetic Agy"));
        let prefix = f.facts();

        f.options.start = SessionStart::Incremental;
        let size = fs::metadata(f.archive.storage.path()).unwrap().len();
        assert_eq!(f.run().unwrap().imported, 1);
        assert_eq!(f.facts(), prefix);
        assert_eq!(fs::metadata(f.archive.storage.path()).unwrap().len(), size);
        let checkpoint = f.checkpoint();
        f.options.now = time(14000);
        let err = f.run().unwrap_err();
        assert!(format!("{err:#}").contains(" line 2"), "{err:#}");
        assert_eq!(f.checkpoint(), checkpoint);

        let mut append = OpenOptions::new().append(true).open(&full).unwrap();
        writeln!(append, " reply\",\"step_index\":2}}").unwrap();
        append.set_modified(time(11000)).unwrap();
        let grown = f.run().unwrap();
        assert_eq!((grown.imported, grown.live), (1, 1));
        assert!(last().contains("synthetic reply"));
        let grown = f.facts();
        assert!(prefix.difference(&grown).is_empty());
        assert!(grown.len() > prefix.len());

        let explicit = Fixture::new();
        super::super::cli::import_paths(
            explicit.archive.storage.path(),
            explicit.archive.storage.key_path(),
            &[full],
            super::super::cli::CliImportSource::Agy,
            None,
            &mut Out::new(&mut |_| Ok(())),
        )
        .unwrap();
        let whole = explicit.facts();
        assert!(whole.difference(&grown).is_empty());
        assert_eq!(beyond_annotations(&grown, &whole), (0, 1));
    }

    #[test]
    fn first_failure_preserves_publications_and_failed_since_never_changes_state() {
        let mut f = Fixture::new();
        f.file("a.jsonl", CODEX, 1000);
        f.file("b.jsonl", "not JSON\n", 1000);
        f.file("c.jsonl", CODEX, 1000);
        f.options.start = SessionStart::Since(time(0));
        let mut done = Vec::new();
        let error = f
            .archive
            .ingest_sessions(&f.options, |event| {
                if let SessionEvent::Imported { path, .. } = event {
                    done.push(path.to_owned());
                }
                Ok(())
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("b.jsonl"));
        assert_eq!(done.len(), 1);
        assert!(!f.options.state_dir.join("ingest-sessions.json").exists());
        assert!(!f.facts().is_empty());
        // Repair only this synthetic fixture, establish progress, then fail a reset.
        f.file("b.jsonl", CODEX, 1000);
        f.run().unwrap();
        let before = f.checkpoint();
        f.file("b.jsonl", "bad\n", 2000);
        assert!(f.run().is_err());
        assert_eq!(f.checkpoint(), before);
    }

    #[test]
    fn uninitialized_or_changed_binding_requires_explicit_baseline() {
        let mut f = Fixture::new();
        f.file("a.jsonl", CODEX, 1000);
        f.options.start = SessionStart::Incremental;
        assert!(f
            .run()
            .unwrap_err()
            .to_string()
            .contains("--all or --since"));
        f.options.start = SessionStart::All;
        f.run().unwrap();
        let before = f.checkpoint();
        f.options.start = SessionStart::Incremental;
        let other = f.directory.path().join("other");
        fs::create_dir(&other).unwrap();
        f.options.roots.push(SessionRoot {
            source: SessionSource::Codex,
            path: other,
        });
        assert!(f.run().unwrap_err().to_string().contains("roots changed"));
        assert_eq!(f.checkpoint(), before);
        f.options.roots.pop();
        let different = Fixture::new();
        assert!(different
            .archive
            .ingest_sessions(&f.options, |_| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("destination"));
        assert_eq!(f.checkpoint(), before);
    }

    #[test]
    fn a_new_pile_file_at_the_bound_path_carries_progress_only_if_it_holds_the_last_publication() {
        let mut f = Fixture::new();
        f.file("a.jsonl", CODEX, 1000);
        f.run().unwrap();
        let pile = f.archive.storage.path().to_owned();
        // A compaction swap: the same records in a new file at the same path.
        let copy = f.directory.path().join("copy.pile");
        fs::copy(&pile, &copy).unwrap();
        fs::rename(&copy, &pile).unwrap();
        f.options.start = SessionStart::Incremental;
        f.options.now = time(14000);
        assert!(f.run().unwrap().rebound);
        assert!(!f.run().unwrap().rebound);
        // A new file that holds nothing this checkpoint published stays refused.
        let before = f.checkpoint();
        let empty = f.directory.path().join("empty.pile");
        File::create(&empty).unwrap();
        fs::rename(&empty, &pile).unwrap();
        assert!(f.run().unwrap_err().to_string().contains("destination"));
        assert_eq!(f.checkpoint(), before);
    }

    #[test]
    fn a_republished_commit_is_not_evidence_of_the_newer_appends() {
        // Identical COMMITs deduplicate, so a pass whose last file only
        // republishes may end on a record the pile held long before. An older
        // file at the path holds that record but not the pass's new appends.
        let mut f = Fixture::new();
        f.file("a.jsonl", CODEX, 1000);
        // Same content: z's COMMITs are a's, so z only ever republishes.
        f.file("z.jsonl", CODEX, 1000);
        f.run().unwrap();
        let pile = f.archive.storage.path().to_owned();
        let older = f.directory.path().join("older.pile");
        fs::copy(&pile, &older).unwrap();
        let grown = format!(
            "{CODEX}{}",
            "{\"timestamp\":\"2026-08-16T08:02:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"a later line\"}}\n"
        );
        f.file("a.jsonl", &grown, 2000);
        f.run().unwrap();
        let before = f.checkpoint();
        fs::rename(&older, &pile).unwrap();
        f.options.start = SessionStart::Incremental;
        f.options.now = time(14000);
        assert!(f.run().unwrap_err().to_string().contains("destination"));
        assert_eq!(f.checkpoint(), before);
    }

    #[test]
    fn busy_and_output_failure_leave_checkpoint_unchanged() {
        let f = Fixture::new();
        f.file("a.jsonl", CODEX, 1000);
        f.run().unwrap();
        let before = f.checkpoint();
        let lock = File::options()
            .read(true)
            .write(true)
            .open(f.options.state_dir.join("ingest-sessions.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(f.run().unwrap_err().is::<SessionBusy>());
        assert_eq!(f.checkpoint(), before);
        drop(lock);
        assert!(f
            .archive
            .ingest_sessions(&f.options, |e| {
                if matches!(e, SessionEvent::Complete(_)) {
                    bail!("synthetic output failure");
                }
                Ok(())
            })
            .is_err());
        assert_eq!(f.checkpoint(), before);
    }

    #[test]
    fn children_overlapping_roots_and_unusual_paths_match_per_file_import() {
        let mut f = Fixture::new();
        let path = f.file("subagents/ child\nwith space.jsonl", CODEX, 1000);
        f.options.roots.push(SessionRoot {
            source: SessionSource::Codex,
            path: path.parent().unwrap().to_owned(),
        });
        assert_eq!(f.run().unwrap().imported, 1);
        let direct = Fixture::new();
        let argv = vec![
            "archive".into(),
            "--pile".into(),
            direct.archive.storage.path().as_os_str().to_owned(),
            "--key".into(),
            direct
                .archive
                .storage
                .key_path()
                .unwrap()
                .as_os_str()
                .to_owned(),
            "import".into(),
            "--source".into(),
            "codex".into(),
            "--finished-after".into(),
            "0s".into(),
            path.into_os_string(),
        ];
        let cli = crate::archive::cli::Cli::try_parse_from(argv).unwrap();
        crate::archive::cli::execute(cli, &mut Out::new(&mut |_| Ok(()))).unwrap();
        assert_eq!(f.facts(), direct.facts());
    }

    #[test]
    fn default_discovery_includes_archived_and_explicit_codex_homes() {
        let home = tempfile::tempdir().unwrap();
        let custom = tempfile::tempdir().unwrap();
        fs::write(home.path().join(".codex-backup.tar"), "not a session root").unwrap();
        for relative in [
            ".codex/sessions",
            ".codex/archived_sessions",
            ".codex-astra/sessions",
            ".claude/projects",
        ] {
            fs::create_dir_all(home.path().join(relative)).unwrap();
        }
        fs::create_dir(custom.path().join("archived_sessions")).unwrap();
        let roots = default_roots(home.path(), Some(custom.path())).unwrap();
        assert_eq!(roots.len(), 5);
        assert_eq!(
            default_roots(home.path(), Some(&home.path().join(".codex")))
                .unwrap()
                .len(),
            4
        );
    }

    #[cfg(unix)]
    #[test]
    fn traversal_errors_symlinks_and_special_files_do_not_advance() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let f = Fixture::new();
        f.file("a.jsonl", CODEX, 1000);
        f.run().unwrap();
        let before = f.checkpoint();
        let directory = f.options.roots[0].path.join("unreadable");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0)).unwrap();
        assert!(f.run().is_err());
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&directory, directory.join("loop")).unwrap();
        assert!(f.run().unwrap_err().to_string().contains("symlink"));
        assert_eq!(f.checkpoint(), before);
        let mut options = f.options.clone();
        options.roots[0].path = directory.join("missing");
        assert!(f.archive.ingest_sessions(&options, |_| Ok(())).is_err());
    }

    #[test]
    fn append_after_discovery_is_next_pass_work_but_other_mutation_fails() {
        let f = Fixture::new();
        let path = f.file("a.jsonl", CODEX, 1000);
        f.archive
            .ingest_sessions(&f.options, |event| {
                if matches!(event, SessionEvent::Importing(_)) {
                    File::options()
                        .append(true)
                        .open(&path)?
                        .write_all(b"{\"partial")?;
                }
                Ok(())
            })
            .unwrap();
        assert!(f.options.state_dir.join("ingest-sessions.json").exists());
        assert!(!f.facts().is_empty());

        let f = Fixture::new();
        let path = f.file("a.jsonl", CODEX, 1000);
        let result = f.archive.ingest_sessions(&f.options, |event| {
            if matches!(event, SessionEvent::Importing(_)) {
                File::options().write(true).open(&path)?.set_len(10)?;
            }
            Ok(())
        });
        assert!(format!("{:#}", result.unwrap_err()).contains("changed since discovery"));
        assert!(!f.options.state_dir.join("ingest-sessions.json").exists());
        assert!(f.facts().is_empty());
    }

    #[test]
    fn native_tail_receipts_survive_and_claude_sidecar_changes_are_eligible() {
        let mut f = Fixture::new();
        f.file("tail.jsonl", &format!("{CODEX}partial"), 1000);
        let mut ignored = 0;
        f.archive
            .ingest_sessions(&f.options, |event| {
                if let SessionEvent::Imported { receipt, .. } = event {
                    if let ImportSummary::Codex(summary) = &receipt.summary {
                        ignored += summary.trailing_bytes_ignored;
                        assert_eq!(summary.stats.held_back_records, 0);
                    }
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(ignored, 7);
        let c = Fixture::new();
        let file = c.file("claude.jsonl", CLAUDE, 1000);
        f.options = c.options.clone();
        f.options.roots[0].source = SessionSource::ClaudeCode;
        c.archive.ingest_sessions(&f.options, |_| Ok(())).unwrap();
        let metadata = file.with_extension("meta.json");
        fs::write(&metadata, "{}").unwrap();
        File::options()
            .write(true)
            .open(&metadata)
            .unwrap()
            .set_modified(time(8000))
            .unwrap();
        f.options.start = SessionStart::Incremental;
        let live = c.archive.ingest_sessions(&f.options, |_| Ok(())).unwrap();
        assert_eq!((live.imported, live.live), (1, 1));
        f.options.now = time(14000);
        assert_eq!(
            c.archive
                .ingest_sessions(&f.options, |_| Ok(()))
                .unwrap()
                .imported,
            1
        );
    }

    #[test]
    fn shared_owner_flushes_before_progress_without_being_closed() {
        let f = Fixture::new();
        f.file("a.jsonl", CODEX, 1000);
        let storage = crate::storage::Storage::shared_local(
            f.archive.storage.path().to_owned(),
            f.archive.storage.key_path().map(Path::to_owned),
        );
        let archive = Archive::with_storage(storage.clone());
        assert_eq!(
            archive
                .ingest_sessions(&f.options, |_| Ok(()))
                .unwrap()
                .imported,
            1
        );
        assert!(!f.checkpoint().is_empty());
        // The caller retains its owner and can keep using it. Its eventual close
        // is still its responsibility; ingest's durability boundary was flush.
        storage
            .with_store(|store, _, _| {
                use triblespace::core::repo::StorageFlush;
                store.flush()?;
                Ok(())
            })
            .unwrap();
        let before = f.checkpoint();
        assert!(archive
            .ingest_sessions(&f.options, |e| {
                if matches!(e, SessionEvent::Complete(_)) {
                    bail!("reject shared output");
                }
                Ok(())
            })
            .is_err());
        assert_eq!(f.checkpoint(), before);
        storage.close().unwrap();
    }

    #[test]
    fn quiet_session_pass_uses_native_finished_before_for_claude_tail() {
        let mut f = Fixture::new();
        f.options.roots[0].source = SessionSource::ClaudeCode;
        let stopped = concat!(
            r#"{"type":"user","sessionId":"stopped","uuid":"u","parentUuid":null,"message":{"role":"user","content":"look"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"stopped","uuid":"c","parentUuid":"u","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file":"a"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"stopped","uuid":"r","parentUuid":"c","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"contents of a"}]}}"#,
            "\n"
        );
        let path = f.file("stopped.jsonl", stopped, 1000);
        let held =
            crate::archive_claude_code::project_path_finished(&path, None, |_| Ok(())).unwrap();
        let mut complete = None;
        f.archive
            .ingest_sessions(&f.options, |e| {
                if let SessionEvent::Imported { receipt, .. } = e {
                    if let ImportSummary::ClaudeCode(summary) = &receipt.summary {
                        complete = Some(summary.clone());
                    }
                }
                Ok(())
            })
            .unwrap();
        assert!(complete.unwrap().stats.content_parts > held.stats.content_parts);
    }

    /// Facts of `grown` beyond `whole` that are not leaf annotations: what a
    /// fork, or a node whose identity moved, would leave behind.
    fn beyond_annotations(
        grown: &triblespace::core::trible::TribleSet,
        whole: &triblespace::core::trible::TribleSet,
    ) -> (usize, usize) {
        use crate::schemas::blockdag::source_projection;
        let annotation = [
            source_projection::source_locator.id(),
            source_projection::source_detail.id(),
            source_projection::raw_agent_name.id(),
            source_projection::source_thread.id(),
            source_projection::raw_thread_source.id(),
            source_projection::raw_agent_path.id(),
        ];
        let extra = grown.difference(whole);
        let annotations = extra
            .iter()
            .filter(|trible| annotation.contains(trible.a()))
            .count();
        (extra.len() - annotations, annotations)
    }

    fn live_then_quiet_converges(source: SessionSource, steps: &[&str], name: &str) {
        let mut f = Fixture::new();
        f.options.roots[0].source = source;
        let mut body = String::new();
        let mut previous = triblespace::core::trible::TribleSet::new();
        let mut now = 10_000;
        for (index, step) in steps.iter().enumerate() {
            body.push_str(step);
            // Every step but the last is written within the quiet window.
            let last = index + 1 == steps.len();
            f.options.now = time(now);
            f.file(name, &body, if last { now - 7200 } else { now - 60 });
            let report = f.run().unwrap();
            assert_eq!(
                (report.imported, report.live),
                (1, usize::from(!last)),
                "step {index}"
            );
            let facts = f.facts();
            assert!(facts.len() > previous.len(), "step {index} added nothing");
            previous = facts;
            f.options.start = SessionStart::Incremental;
            now += 10_000;
        }
        let whole = Fixture::new();
        let mut options = whole.options.clone();
        options.roots[0].source = source;
        options.now = time(now);
        let path = options.roots[0].path.join(name);
        fs::write(&path, &body).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time(1000))
            .unwrap();
        whole.archive.ingest_sessions(&options, |_| Ok(())).unwrap();
        let whole = whole.facts();
        assert!(
            whole.difference(&previous).is_empty(),
            "the live imports miss nodes"
        );
        let (nodes, annotations) = beyond_annotations(&previous, &whole);
        assert_eq!(nodes, 0, "the live imports forked or moved a node");
        // The wart: each earlier pass's leaf keeps its session annotation.
        println!("{name}: {annotations} leaf annotations beyond a one-shot import");
        assert!(annotations <= 2 * (steps.len() - 1));
    }

    #[test]
    fn a_growing_claude_transcript_converges_across_live_passes() {
        let user = |uuid: &str, parent: &str, text: &str| {
            format!(
                r#"{{"type":"user","sessionId":"live","uuid":"{uuid}","parentUuid":{parent},"message":{{"role":"user","content":"{text}"}}}}"#
            )
        };
        let call = r#"{"type":"assistant","sessionId":"live","uuid":"c","parentUuid":"u","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file":"a"}}]}}"#;
        let result = r#"{"type":"user","sessionId":"live","uuid":"r","parentUuid":"c","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"contents of a"}]}}"#;
        let answer = r#"{"type":"assistant","sessionId":"live","uuid":"a","parentUuid":"p","message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"done"}]}}"#;
        let (head, tail) = answer.split_at(40);
        let steps = [
            // Ends inside msg_1: the result is held back.
            format!("{}\n{call}\n{result}\n", user("u", "null", "look")),
            // The prompt closes msg_1; the answer is half written.
            format!("{}\n{head}", user("p", r#""r""#, "go on")),
            format!("{tail}\n"),
        ];
        let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
        live_then_quiet_converges(SessionSource::ClaudeCode, &steps, "live.jsonl");

        // A compaction leaves msg_1 open on a dead branch: its result lands
        // on the first live pass, and the passes still converge.
        let compacted = [
            format!(
                "{}\n{call}\n{result}\n{}\n",
                user("u", "null", "look"),
                user("p", "null", "summary, then go on")
            ),
            format!("{answer}\n"),
        ];
        let compacted: Vec<&str> = compacted.iter().map(String::as_str).collect();
        live_then_quiet_converges(SessionSource::ClaudeCode, &compacted, "compacted.jsonl");
        let f = Fixture::new();
        let mut options = f.options.clone();
        options.roots[0].source = SessionSource::ClaudeCode;
        let path = options.roots[0].path.join("compacted.jsonl");
        fs::write(&path, compacted[0]).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time(9000))
            .unwrap();
        let mut held = None;
        f.archive
            .ingest_sessions(&options, |event| {
                if let SessionEvent::Imported { receipt, .. } = event {
                    if let ImportSummary::ClaudeCode(summary) = &receipt.summary {
                        held = Some(summary.stats.held_back_records);
                    }
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(held, Some(0));

        // The half-written line is held back, not a parse error, and the
        // unfinished message's tail is held back too.
        let f = Fixture::new();
        let mut options = f.options.clone();
        options.roots[0].source = SessionSource::ClaudeCode;
        let path = options.roots[0].path.join("live.jsonl");
        fs::write(&path, format!("{}{}", steps[0], head)).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time(9000))
            .unwrap();
        let mut seen = None;
        f.archive
            .ingest_sessions(&options, |event| {
                if let SessionEvent::Imported { receipt, .. } = event {
                    if let ImportSummary::ClaudeCode(summary) = &receipt.summary {
                        seen = Some((summary.stats.records_seen, summary.stats.held_back_records));
                    }
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, Some((3, 1)));
    }

    #[test]
    fn a_growing_codex_rollout_converges_across_live_passes() {
        let event = |minute: u32, text: &str| {
            format!(
                r#"{{"timestamp":"2026-08-16T08:{minute:02}:00Z","type":"event_msg","payload":{{"type":"agent_message","message":"{text}"}}}}"#
            )
        };
        let second = event(2, "first answer");
        let (head, tail) = second.split_at(50);
        let steps = [
            format!("{CODEX}{head}"),
            format!("{tail}\n{}\n", event(3, "second answer")),
            format!("{}\n", event(4, "third answer")),
        ];
        let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
        live_then_quiet_converges(SessionSource::Codex, &steps, "rollout.jsonl");
    }

    /// Real-data convergence of a live Claude Code session: two live passes
    /// (a cut of the file, then the rest appended) against one live import of
    /// the whole. Inputs are copies, never a live transcript or pile.
    #[test]
    #[ignore = "requires LIVE_SESSION_GROWN and LIVE_SESSION_PREFIX_LEN (bytes)"]
    fn a_real_live_session_converges_with_its_one_shot_import() {
        use std::time::Instant;
        let grown = PathBuf::from(std::env::var_os("LIVE_SESSION_GROWN").unwrap());
        let cut: u64 = std::env::var("LIVE_SESSION_PREFIX_LEN")
            .unwrap()
            .parse()
            .unwrap();
        let live = |f: &Fixture, len: Option<u64>| {
            let path = f.options.roots[0].path.join("live.jsonl");
            let mut source = File::open(&grown).unwrap();
            let mut target = File::options()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            let have = target.metadata().unwrap().len();
            std::io::Seek::seek(&mut source, std::io::SeekFrom::Start(have)).unwrap();
            let want = len.unwrap_or(u64::MAX) - have;
            std::io::copy(&mut source.take(want), &mut target).unwrap();
        };
        let pass = |f: &mut Fixture, label: &str| {
            f.options.now = SystemTime::now();
            let before = fs::metadata(f.archive.storage.path()).unwrap().len();
            let started = Instant::now();
            let mut held = (0, 0);
            let report = f
                .archive
                .ingest_sessions(&f.options, |event| {
                    if let SessionEvent::Imported { receipt, .. } = event {
                        if let ImportSummary::ClaudeCode(s) = &receipt.summary {
                            held = (s.stats.records_seen, s.stats.held_back_records);
                        }
                    }
                    Ok(())
                })
                .unwrap();
            let grew = fs::metadata(f.archive.storage.path()).unwrap().len() - before;
            println!(
                "{label}: {:.1} s, imported {} live {}, records_seen {} held_back {}, pile +{grew} bytes",
                started.elapsed().as_secs_f64(),
                report.imported,
                report.live,
                held.0,
                held.1
            );
            f.options.start = SessionStart::Incremental;
        };
        let mut two = Fixture::new();
        two.options.roots[0].source = SessionSource::ClaudeCode;
        live(&two, Some(cut));
        pass(&mut two, "first live pass (cut)");
        live(&two, None);
        pass(&mut two, "second live pass (grown)");
        pass(&mut two, "third live pass (unchanged)");
        let mut one = Fixture::new();
        one.options.roots[0].source = SessionSource::ClaudeCode;
        live(&one, None);
        pass(&mut one, "one-shot live import (grown)");
        let (two, one) = (two.facts(), one.facts());
        let (nodes, annotations) = beyond_annotations(&two, &one);
        println!(
            "facts: two-pass {} one-shot {}; missing {}; beyond: {nodes} non-annotation, {annotations} annotation",
            two.len(),
            one.len(),
            one.difference(&two).len()
        );
        assert!(one.difference(&two).is_empty());
        assert_eq!(nodes, 0);
    }

    /// Explicitly run after building only --bin archive, without Cargo's
    /// integration-test implicit all-binary build. All child inputs are synthetic.
    #[test]
    #[ignore = "requires SESSION_INGEST_TEST_ARCHIVE pointing to separately built candidate"]
    fn native_cli_exit_codes_and_collection_binding() {
        use std::process::Command;
        use triblespace::core::collection::{
            AdmissionPolicy, CollectionPolicy, CollectionStoreExt,
        };
        use triblespace::core::repo::pile::Pile;
        let binary =
            std::env::var_os("SESSION_INGEST_TEST_ARCHIVE").expect("candidate binary path");
        let f = Fixture::new();
        f.file("a.jsonl", &format!("{CODEX}partial"), 1000);
        let command = || {
            let mut command = Command::new(&binary);
            command
                .env_clear()
                .env("HOME", f.directory.path())
                .arg("--pile")
                .arg(f.archive.storage.path())
                .arg("--key")
                .arg(f.archive.storage.key_path().unwrap())
                .arg("ingest-sessions")
                .arg("--codex-root")
                .arg(&f.options.roots[0].path)
                .arg("--state-dir")
                .arg(&f.options.state_dir);
            command
        };
        let uninitialized = command().output().unwrap();
        println!(
            "native uninitialized: status={:?} {}",
            uninitialized.status.code(),
            String::from_utf8_lossy(&uninitialized.stderr)
        );
        assert_eq!(uninitialized.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&uninitialized.stderr)
            .contains("choose --all or --since explicitly"));
        assert!(!f.options.state_dir.join("ingest-sessions.json").exists());
        let first = command().arg("--all").output().unwrap();
        println!(
            "native successful pass: status={:?}\n{}\n{}",
            first.status.code(),
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&first.stderr)
        );
        assert!(first.status.success());
        assert!(String::from_utf8_lossy(&first.stdout).contains("trailing_bytes_ignored=7"));
        let before = f.checkpoint();
        let lock = File::options()
            .read(true)
            .write(true)
            .open(f.options.state_dir.join("ingest-sessions.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let busy = command().output().unwrap();
        println!(
            "native busy: status={:?} {}",
            busy.status.code(),
            String::from_utf8_lossy(&busy.stderr)
        );
        assert_eq!(busy.status.code(), Some(3));
        assert_eq!(f.checkpoint(), before);
        drop(lock);

        let signer =
            crate::storage::load_signer(f.archive.storage.path(), f.archive.storage.key_path())
                .unwrap();
        let peer = ed25519_dalek::SigningKey::from_bytes(&[0x51; 32]);
        let policy =
            AdmissionPolicy::quorum([signer.verifying_key(), peer.verifying_key()], 1, None)
                .unwrap();
        let mut store = Pile::open(f.archive.storage.path()).unwrap();
        let alternate = store
            .collection("archive", CollectionPolicy::new(policy.clone(), policy))
            .unwrap();
        store.close().unwrap();
        let changed = command()
            .env(
                "TRIBLESPACE_COLLECTION_ARCHIVE",
                hex::encode(alternate.handle().raw),
            )
            .output()
            .unwrap();
        println!(
            "native changed collection: status={:?} {}",
            changed.status.code(),
            String::from_utf8_lossy(&changed.stderr)
        );
        assert_eq!(changed.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&changed.stderr).contains("destination or roots changed"));
        assert_eq!(f.checkpoint(), before);

        f.file("b.jsonl", "bad\n", 1000);
        let failed = command().arg("--all").output().unwrap();
        println!(
            "native bad file: status={:?} {}",
            failed.status.code(),
            String::from_utf8_lossy(&failed.stderr)
        );
        assert_eq!(failed.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&failed.stderr).contains("b.jsonl"));
        assert_eq!(f.checkpoint(), before);
    }
}
