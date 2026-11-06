//! The reference freezer for the v3 cutover: what today's hand-written
//! importer does with every file of a real corpus, recorded once, so the
//! table-driven engine can later be checked against that record instead of
//! a live importer.
//!
//! A generation is one directory. It holds the corpus `manifest.jsonl`
//! (path, size, mtime, sorted by path), a `header.json` naming the build,
//! the format, the manifest's sha256 and the fact-set hash method, an
//! append-only `records.jsonl` (one line per examined file, written and
//! synced before the next file starts), `runs.jsonl` (one line per run),
//! a fresh `reference.pile` signed by its own fresh `reference.key` that
//! holds every accepted fragment, and a `lock` taken with `flock` for the
//! whole run.
//!
//! Each file is read once into memory; its sha256 and the importer see
//! exactly those bytes. A file whose size or mtime differs from its
//! manifest entry is recorded as changed and never imported. A run resumes
//! a generation only under the identity in its header, and skips a file
//! only when a record holds its path and the sha256 of bytes read afresh.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::prelude::*;

use crate::archive_v3::record::Failure;
use crate::collection_names::private_policy;
use crate::storage::{initialize_signer, load_signer, open_pile_strict};

#[cfg(test)]
mod tests;

/// How an accepted fragment's facts are hashed. Stated in every header.
pub const FACT_HASH: &str = "sha256 over the fragment's facts (Fragment::facts, metafacts \
    excluded) as raw 64-byte tribles (Trible::as_raw: entity 16, attribute 16, value 32 bytes) \
    concatenated without separators in ascending byte-lexicographic order \
    (TribleSet::iter_ordered); blobs are covered only through the handles inside those \
    tribles; the fact count is recorded beside the hash";

/// The collection every accepted fragment of a generation is committed to.
const COLLECTION: &str = "archive-v3-reference";
const MANIFEST: &str = "manifest.jsonl";
const HEADER: &str = "header.json";
const RECORDS: &str = "records.jsonl";
const RUNS: &str = "runs.jsonl";
const PILE: &str = "reference.pile";
const KEY: &str = "reference.key";
const LOCK: &str = "lock";

/// Lowercase hexadecimal sha256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The fact-set hash ([`FACT_HASH`]).
pub fn fact_sha256(facts: &TribleSet) -> String {
    let mut context = Sha256::new();
    for trible in facts.iter_ordered() {
        context.update(trible.as_raw());
    }
    hex::encode(context.finalize())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Codex,
    ClaudeCode,
}

impl Format {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "codex" => Ok(Format::Codex),
            "claude_code" => Ok(Format::ClaudeCode),
            other => bail!("unknown format {other:?}: codex or claude_code"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Codex => "codex",
            Format::ClaudeCode => "claude_code",
        }
    }

    fn import(self, source: &[u8]) -> Result<Fragment, Failure> {
        match self {
            Format::Codex => crate::archive_v3::codex::import(source),
            Format::ClaudeCode => crate::archive_v3::claude_code::import(source),
        }
    }
}

/// One manifest line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub mtime_ns: u64,
}

fn mtime_ns(metadata: &fs::Metadata) -> Result<u64> {
    let since = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .context("an mtime before 1970")?;
    u64::try_from(since.as_nanos()).context("an mtime past 2554")
}

/// Every regular `*.jsonl` file under `root`, symbolic links not followed,
/// sorted by path.
pub fn enumerate(root: &Path) -> Result<Vec<Entry>> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolve corpus root {}", root.display()))?;
    let mut entries = Vec::new();
    let mut pending = vec![root];
    while let Some(directory) = pending.pop() {
        let listing =
            fs::read_dir(&directory).with_context(|| format!("list {}", directory.display()))?;
        for item in listing {
            let item = item?;
            let kind = item.file_type()?;
            let path = item.path();
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            if !kind.is_file() || !path.extension().is_some_and(|ext| ext == "jsonl") {
                continue;
            }
            let metadata = item.metadata()?;
            let path = path
                .into_os_string()
                .into_string()
                .map_err(|path| anyhow!("a corpus path is not UTF-8: {path:?}"))?;
            entries.push(Entry {
                path,
                size: metadata.len(),
                mtime_ns: mtime_ns(&metadata)?,
            });
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

pub fn manifest_bytes(entries: &[Entry]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in entries {
        bytes.extend(serde_json::to_vec(entry).expect("an entry serializes"));
        bytes.push(b'\n');
    }
    bytes
}

/// A manifest's entries; refused unless strictly sorted by path.
pub fn parse_manifest(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let entry: Entry = serde_json::from_slice(line).context("a manifest line")?;
        if let Some(last) = entries.last() {
            if last.path >= entry.path {
                bail!(
                    "the manifest is not strictly sorted by path at {}",
                    entry.path
                );
            }
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// One file as read: exactly these bytes are hashed and imported.
pub struct Snapshot {
    pub bytes: Vec<u8>,
    /// The mtime after the read.
    pub mtime_ns: u64,
    pub sha256: String,
}

pub fn read_once(path: &Path) -> Result<Snapshot> {
    let mut file = File::open(path)?;
    let expected = file.metadata()?.len();
    let mut bytes = Vec::with_capacity(usize::try_from(expected).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    let mtime_ns = mtime_ns(&file.metadata()?)?;
    let sha256 = sha256(&bytes);
    Ok(Snapshot {
        bytes,
        mtime_ns,
        sha256,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted {
        facts: usize,
        fact_sha256: String,
        metafacts: usize,
        blobs: usize,
        blob_bytes: u64,
        /// The blake3 handle of the facts' SimpleArchive the commit names,
        /// once committed.
        commit_data: Option<String>,
    },
    /// The importer's own refusal.
    Refused {
        failure_path: String,
        reason: String,
    },
    /// Size or mtime differ from the manifest: not imported, not covered.
    Changed {
        manifest_mtime_ns: u64,
        read_mtime_ns: u64,
    },
    /// `cause` is `read` (no bytes) or `panic` (caught). Running out of
    /// memory aborts the process and leaves no record.
    Operational {
        cause: &'static str,
        message: String,
    },
}

/// One `records.jsonl` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub path: String,
    pub manifest_size: u64,
    /// `None` only when the file could not be read.
    pub read_size: Option<u64>,
    pub sha256: Option<String>,
    pub outcome: Outcome,
}

impl Record {
    pub fn to_json(&self) -> Value {
        let mut line = json!({
            "path": self.path,
            "manifest_size": self.manifest_size,
            "read_size": self.read_size,
            "sha256": self.sha256,
        });
        let fields = match &self.outcome {
            Outcome::Accepted {
                facts,
                fact_sha256,
                metafacts,
                blobs,
                blob_bytes,
                commit_data,
            } => json!({"outcome": "accepted", "facts": facts, "fact_sha256": fact_sha256,
                "metafacts": metafacts, "blobs": blobs, "blob_bytes": blob_bytes,
                "commit_data": commit_data}),
            Outcome::Refused {
                failure_path,
                reason,
            } => json!({"outcome": "refused", "failure_path": failure_path, "reason": reason}),
            Outcome::Changed {
                manifest_mtime_ns,
                read_mtime_ns,
            } => json!({"outcome": "changed", "manifest_mtime_ns": manifest_mtime_ns,
                "read_mtime_ns": read_mtime_ns}),
            Outcome::Operational { cause, message } => {
                json!({"outcome": "operational", "cause": cause, "message": message})
            }
        };
        let (Value::Object(line_fields), Value::Object(more)) = (&mut line, fields) else {
            unreachable!("both are objects");
        };
        line_fields.extend(more);
        line
    }
}

pub fn unreadable(entry: &Entry, error: &anyhow::Error) -> Record {
    Record {
        path: entry.path.clone(),
        manifest_size: entry.size,
        read_size: None,
        sha256: None,
        outcome: Outcome::Operational {
            cause: "read",
            message: format!("{error:#}"),
        },
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic without a message".to_owned()
    }
}

/// What the importer does with one snapshot. The bytes are dropped here,
/// before the fragment is committed.
pub fn classify(format: Format, entry: &Entry, snapshot: Snapshot) -> (Record, Option<Fragment>) {
    let read_size = snapshot.bytes.len() as u64;
    let record = |outcome: Outcome| Record {
        path: entry.path.clone(),
        manifest_size: entry.size,
        read_size: Some(read_size),
        sha256: Some(snapshot.sha256.clone()),
        outcome,
    };
    if read_size != entry.size || snapshot.mtime_ns != entry.mtime_ns {
        let changed = Outcome::Changed {
            manifest_mtime_ns: entry.mtime_ns,
            read_mtime_ns: snapshot.mtime_ns,
        };
        return (record(changed), None);
    }
    match catch_unwind(AssertUnwindSafe(|| format.import(&snapshot.bytes))) {
        Ok(Ok(mut fragment)) => {
            let facts = fragment.facts().len();
            let fact_sha256 = fact_sha256(fragment.facts());
            let metafacts = fragment.metafacts().len();
            let blobs = fragment
                .blobs_mut()
                .snapshot()
                .expect("a memory blob snapshot is infallible");
            let accepted = Outcome::Accepted {
                facts,
                fact_sha256,
                metafacts,
                blobs: blobs.len(),
                blob_bytes: blobs.iter().map(|(_, blob)| blob.bytes.len() as u64).sum(),
                commit_data: None,
            };
            (record(accepted), Some(fragment))
        }
        Ok(Err(failure)) => {
            let refused = Outcome::Refused {
                failure_path: failure.path,
                reason: failure.reason.to_owned(),
            };
            (record(refused), None)
        }
        Err(payload) => {
            let panicked = Outcome::Operational {
                cause: "panic",
                message: panic_message(payload.as_ref()),
            };
            (record(panicked), None)
        }
    }
}

/// What built the run. With the format and the manifest's sha256 it is the
/// generation's identity: a resume refuses any other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub faculties_revision: String,
    pub faculties_dirty: bool,
    pub binary_sha256: String,
    pub cargo_lock_sha256: String,
}

impl Build {
    /// This process: the HEAD and status of the source tree it was compiled
    /// from (read now, at run start), the sha256 of its own executable, and
    /// that tree's Cargo.lock.
    pub fn current() -> Result<Self> {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repository = git2::Repository::discover(source)
            .with_context(|| format!("open the git repository of {}", source.display()))?;
        let faculties_revision = repository.head()?.peel_to_commit()?.id().to_string();
        let mut options = git2::StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false);
        let faculties_dirty = repository
            .statuses(Some(&mut options))?
            .iter()
            .any(|entry| entry.status() != git2::Status::CURRENT);
        let binary = std::env::current_exe().context("locate this executable")?;
        let binary_sha256 = sha256(&fs::read(&binary).context("read this executable")?);
        let cargo_lock_sha256 =
            sha256(&fs::read(source.join("Cargo.lock")).context("read Cargo.lock")?);
        Ok(Build {
            faculties_revision,
            faculties_dirty,
            binary_sha256,
            cargo_lock_sha256,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub format: String,
    pub build: Build,
    pub manifest_sha256: String,
    pub manifest_entries: usize,
    pub fact_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    pub identity: Identity,
    pub started: String,
    pub pile: String,
    /// The reference key's verifying key, hexadecimal.
    pub signer: String,
    /// The reference collection's descriptor handle, hexadecimal.
    pub collection: String,
}

/// Where a fresh generation's manifest comes from.
pub enum Source {
    /// Enumerate `*.jsonl` under this root.
    Corpus(PathBuf),
    /// Use this manifest file's exact bytes.
    Manifest(PathBuf),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub accepted: usize,
    pub refused: usize,
    pub changed: usize,
    pub operational: usize,
    /// Done before, by path and fresh sha256.
    pub skipped: usize,
    pub bytes_read: u64,
}

pub struct Generation {
    format: Format,
    manifest: Vec<Entry>,
    records: File,
    done: HashSet<(String, String)>,
    pile: Pile,
    signer: SigningKey,
    collection: Collection<SimpleArchive>,
    /// Held, locked, for the life of the run.
    _lock: File,
}

fn lock(directory: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join(LOCK))
        .context("open the generation lock")?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another run holds {}", directory.join(LOCK).display())
        }
        Err(std::fs::TryLockError::Error(error)) => Err(error).context("lock the generation"),
    }
}

/// Write `name` in `directory` whole or not at all.
fn write_durably(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let partial = directory.join(format!("{name}.partial"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .with_context(|| format!("create {}", partial.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&partial, directory.join(name))?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn append_line(file: &mut File, line: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(line)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_data()?;
    Ok(())
}

fn append_to(directory: &Path, name: &str, line: &Value) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(name))?;
    append_line(&mut file, line)
}

/// Open `records.jsonl`, cut a torn last line (an interrupted append), and
/// return what its records cover: (path, sha256) of every record holding
/// bytes that were read.
fn open_records(directory: &Path) -> Result<(File, HashSet<(String, String)>)> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(directory.join(RECORDS))
        .context("open the records")?;
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    if complete < bytes.len() {
        file.set_len(complete as u64)?;
        file.sync_data()?;
    }
    let mut done = HashSet::new();
    for line in bytes[..complete].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let record: Value = serde_json::from_slice(line).context("a complete record line")?;
        let path = record["path"].as_str().context("a record without a path")?;
        // Only an importer verdict is reusable: a changed file was never
        // imported, and an operational record says nothing about the bytes.
        let verdict = matches!(record["outcome"].as_str(), Some("accepted" | "refused"));
        if let (true, Some(sha256)) = (verdict, record["sha256"].as_str()) {
            done.insert((path.to_owned(), sha256.to_owned()));
        }
    }
    Ok((file, done))
}

fn open_reference(
    directory: &Path,
    signer: &SigningKey,
) -> Result<(Pile, Collection<SimpleArchive>)> {
    let mut pile = open_pile_strict(&directory.join(PILE))?;
    // Not `open_configured`: no TRIBLESPACE_COLLECTION_* variable can
    // redirect a reference generation.
    let policy = private_policy(signer.verifying_key());
    let collection = CollectionStoreExt::collection(&mut pile, COLLECTION, policy)
        .map_err(|error| anyhow!("register the reference collection: {error}"))?;
    Ok((pile, collection))
}

impl Generation {
    /// Start `directory` as a new generation when it does not exist, or
    /// resume it. A resume takes no corpus root (a manifest is fixed per
    /// generation) and accepts a manifest only if its sha256 is the
    /// generation's.
    pub fn open(
        directory: &Path,
        format: Format,
        build: Build,
        source: Option<Source>,
    ) -> Result<Self> {
        if directory.exists() {
            return Self::resume(directory, format, build, source);
        }
        let source = source.context("a new generation needs a corpus root or a manifest")?;
        Self::create(directory, format, build, source)
    }

    fn create(directory: &Path, format: Format, build: Build, source: Source) -> Result<Self> {
        fs::create_dir(directory)
            .with_context(|| format!("create generation {}", directory.display()))?;
        let lock = lock(directory)?;
        let manifest = match source {
            Source::Corpus(root) => manifest_bytes(&enumerate(&root)?),
            Source::Manifest(path) => {
                fs::read(&path).with_context(|| format!("read manifest {}", path.display()))?
            }
        };
        let entries = parse_manifest(&manifest)?;
        write_durably(directory, MANIFEST, &manifest)?;
        let pile_path = directory.join(PILE);
        File::create_new(&pile_path).with_context(|| format!("create {}", pile_path.display()))?;
        let signer = initialize_signer(&pile_path, Some(directory.join(KEY).as_path()))?;
        let (pile, collection) = open_reference(directory, &signer)?;
        let header = Header {
            identity: Identity {
                format: format.name().to_owned(),
                build,
                manifest_sha256: sha256(&manifest),
                manifest_entries: entries.len(),
                fact_hash: FACT_HASH.to_owned(),
            },
            started: chrono::Utc::now().to_rfc3339(),
            pile: PILE.to_owned(),
            signer: hex::encode(signer.verifying_key().to_bytes()),
            collection: hex::encode(collection.handle().raw),
        };
        write_durably(directory, HEADER, &serde_json::to_vec_pretty(&header)?)?;
        append_to(
            directory,
            RUNS,
            &json!({"started": header.started, "resumed": false}),
        )?;
        let (records, done) = open_records(directory)?;
        Ok(Generation {
            format,
            manifest: entries,
            records,
            done,
            pile,
            signer,
            collection,
            _lock: lock,
        })
    }

    fn resume(
        directory: &Path,
        format: Format,
        build: Build,
        source: Option<Source>,
    ) -> Result<Self> {
        // Nothing, not even the lock file, is written into a directory
        // that is not a generation.
        let header_path = directory.join(HEADER);
        if !header_path.is_file() {
            bail!(
                "{} exists and has no {HEADER}: an incomplete start, or not a generation; \
                 remove it to start afresh, or name a new directory",
                directory.display()
            );
        }
        let lock = lock(directory)?;
        let header: Header =
            serde_json::from_slice(&fs::read(&header_path)?).context("read the header")?;
        let manifest = fs::read(directory.join(MANIFEST)).context("read the manifest")?;
        match source {
            None => {}
            Some(Source::Corpus(_)) => bail!(
                "{} exists: a resume uses its own manifest, unset AV3_CORPUS",
                directory.display()
            ),
            Some(Source::Manifest(path)) => {
                if sha256(&fs::read(&path)?) != sha256(&manifest) {
                    bail!("{} is not this generation's manifest", path.display());
                }
            }
        }
        let entries = parse_manifest(&manifest)?;
        let identity = Identity {
            format: format.name().to_owned(),
            build,
            manifest_sha256: sha256(&manifest),
            manifest_entries: entries.len(),
            fact_hash: FACT_HASH.to_owned(),
        };
        if identity != header.identity {
            bail!(
                "this run is not the generation's identity:\n  generation {:?}\n  this run   {:?}",
                header.identity,
                identity
            );
        }
        let signer = load_signer(&directory.join(PILE), Some(directory.join(KEY).as_path()))?;
        if hex::encode(signer.verifying_key().to_bytes()) != header.signer {
            bail!("the reference key is not the one the header names");
        }
        let (pile, collection) = open_reference(directory, &signer)?;
        if hex::encode(collection.handle().raw) != header.collection {
            bail!("the reference collection is not the one the header names");
        }
        append_to(
            directory,
            RUNS,
            &json!({"started": chrono::Utc::now().to_rfc3339(), "resumed": true}),
        )?;
        let (records, done) = open_records(directory)?;
        Ok(Generation {
            format,
            manifest: entries,
            records,
            done,
            pile,
            signer,
            collection,
            _lock: lock,
        })
    }

    /// Examine every manifest entry not already done, in manifest order.
    /// An accepted fragment is committed and the pile synced before its
    /// record is appended and synced.
    pub fn run(&mut self) -> Result<Tally> {
        let mut tally = Tally::default();
        let entries = std::mem::take(&mut self.manifest);
        for (at, entry) in entries.iter().enumerate() {
            if at % 100 == 0 {
                eprintln!("freeze: {at}/{} files, {tally:?}", entries.len());
            }
            let snapshot = match read_once(Path::new(&entry.path)) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    append_line(&mut self.records, &unreadable(entry, &error).to_json())?;
                    tally.operational += 1;
                    continue;
                }
            };
            tally.bytes_read += snapshot.bytes.len() as u64;
            let key = (entry.path.clone(), snapshot.sha256.clone());
            if self.done.contains(&key) {
                tally.skipped += 1;
                continue;
            }
            let (mut record, fragment) = classify(self.format, entry, snapshot);
            if let Some(fragment) = fragment {
                let commit = self
                    .pile
                    .commit(self.collection, &self.signer, fragment)
                    .map_err(|error| anyhow!("commit {}: {error}", entry.path))?;
                self.pile
                    .flush()
                    .map_err(|error| anyhow!("sync the reference pile: {error}"))?;
                if let Outcome::Accepted { commit_data, .. } = &mut record.outcome {
                    *commit_data = Some(hex::encode(commit.data().raw));
                }
            }
            match record.outcome {
                Outcome::Accepted { .. } => tally.accepted += 1,
                Outcome::Refused { .. } => tally.refused += 1,
                Outcome::Changed { .. } => tally.changed += 1,
                Outcome::Operational { .. } => tally.operational += 1,
            }
            append_line(&mut self.records, &record.to_json())?;
            self.done.insert(key);
        }
        self.manifest = entries;
        Ok(tally)
    }

    pub fn close(self) -> Result<()> {
        self.pile
            .close()
            .map_err(|error| anyhow!("close the reference pile: {error}"))
    }
}

/// Every complete record line of a generation, in file order.
pub fn records(directory: &Path) -> Result<Vec<Value>> {
    let bytes = fs::read(directory.join(RECORDS))?;
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).context("a record line"))
        .collect()
}
