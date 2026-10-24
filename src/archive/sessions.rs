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
use crate::archive_collection::ArchiveImportWriter;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum SessionSource {
    ClaudeCode,
    Codex,
}
impl From<SessionSource> for ImportSource {
    fn from(source: SessionSource) -> Self {
        match source {
            SessionSource::ClaudeCode => Self::ClaudeCode,
            SessionSource::Codex => Self::Codex,
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

/// Conventional local roots, including archived Codex rollouts. Missing optional
/// directories are absent from the selection; a newly appearing root therefore
/// requires an explicit baseline rather than inheriting another selection's progress.
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
        if let Some(parent) = unique.iter().find(|p| root.path.starts_with(&p.path)) {
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
#[derive(Serialize, Deserialize)]
struct Checkpoint {
    binding: Binding,
    through: SystemTime,
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
    if source != SessionSource::ClaudeCode {
        return Ok(None);
    }
    let path = path.with_extension("meta.json");
    match fs::symlink_metadata(&path) {
        Ok(_) => observe_file(&path).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("inspect sidecar {:?}", path)),
    }
}
struct Candidate {
    source: SessionSource,
    path: PathBuf,
    observed: Observation,
    sidecar: Option<Observation>,
}
impl Candidate {
    fn modified(&self) -> SystemTime {
        self.sidecar.as_ref().map_or(self.observed.modified, |s| {
            s.modified.max(self.observed.modified)
        })
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
        Ok(())
    }
}
fn discover(roots: &[SessionRoot]) -> Result<(Vec<Candidate>, usize)> {
    // This is a transient file list for one observation, never a retained catalogue.
    let mut files = Vec::new();
    let mut other = 0;
    for root in roots {
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
                    let observed = observe_file(&path)?;
                    let annotations = sidecar(&path, root.source)?;
                    files.push(Candidate {
                        source: root.source,
                        path,
                        observed,
                        sidecar: annotations,
                    });
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
            let mut writer = ArchiveImportWriter::from_store_with_storage(&mut *store, signer, runtime.clone(), &self.storage)?;
            let collection = hex::encode(writer.collection_handle().raw);
            let binding = Binding { pile: pile_binding, collection: collection.clone(), roots: root_binding };
            let since = match options.start {
                SessionStart::All => None,
                SessionStart::Since(time) => Some(time),
                SessionStart::Incremental => {
                    let previous = previous.as_ref().expect("checked checkpoint");
                    if previous.binding != binding { bail!("session destination or roots changed; choose --all or --since explicitly"); }
                    Some(previous.through)
                }
            };
            if since.is_some_and(|time| time > cutoff) { bail!("session lower bound is after eligibility cutoff (clock moved backwards or --since is too recent)"); }
            let mut report = SessionReport { imported: 0, live: 0, unchanged: 0, other_files,
                pieces: 0, commits: 0, cutoff, collection };
            for file in &files {
                // Inclusive lower edge intentionally rechecks the prior boundary.
                // A file live in the previous pass is at or after it by definition.
                if since.is_some_and(|time| file.modified() < time) { report.unchanged += 1; continue; }
                let live = file.modified() >= cutoff;
                let result = (|| {
                    if live { event(SessionEvent::Live(&file.path))?; }
                    event(SessionEvent::Importing(&file.path))?;
                    file.check()?;
                    let summary = super::local::project_path(&file.path, file.source.into(), Some(cutoff), |fragment, identity, _| writer.stage(fragment, identity))?;
                    file.check()?;
                    let publication = writer.commit_unit()?;
                    report.pieces += publication.pieces;
                    report.commits += publication.commits.len();
                    report.imported += 1;
                    report.live += usize::from(live);
                    event(SessionEvent::Imported { path: &file.path, receipt: &ImportReceipt { summary, publication } })
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
            Ok((report, Checkpoint { binding, through: cutoff }))
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
