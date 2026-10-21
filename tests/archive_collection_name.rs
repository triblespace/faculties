//! The Archive collection is named `archive` and selected by
//! `TRIBLESPACE_COLLECTION_ARCHIVE`, end to end through the `archive` binary
//! on a fixture pile that holds both a fresh `archive` descriptor and the
//! retired `blockdag` one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ed25519_dalek::SigningKey;
use tempfile::TempDir;
use triblespace::core::collection::{
    AdmissionPolicy, CollectionCommit, CollectionPolicy, CollectionRead, CollectionRecord,
    CollectionStore, CollectionStoreExt,
};
use triblespace::core::inline::Inline;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::SnapshotSource;

const CODE: &str = r#"{"type":"user","sessionId":"renamed","uuid":"first","timestamp":"2026-03-01T15:34:01Z","message":{"role":"user","content":"archive rename needle"}}"#;

struct Fixture {
    directory: TempDir,
    pile: PathBuf,
    key: PathBuf,
    session: PathBuf,
    archive: String,
    blockdag: String,
}

impl Fixture {
    /// A pile holding two named roots with the production shape of policy (a
    /// 1-of-3 quorum that admits this host's signer): the fresh `archive`
    /// collection and the retired `blockdag` one.
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("archive-rename.pile");
        let key = directory.path().join("host.key");
        fs::File::create(&pile).unwrap();
        faculties::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let host = faculties::storage::load_signer(&pile, Some(&key)).unwrap();
        let roots = [
            host.verifying_key(),
            SigningKey::from_bytes(&[0x51; 32]).verifying_key(),
            SigningKey::from_bytes(&[0x52; 32]).verifying_key(),
        ];
        let quorum = AdmissionPolicy::quorum(roots, 1, None).unwrap();
        let policy = CollectionPolicy::new(quorum.clone(), quorum);
        let mut store = Pile::open(&pile).unwrap();
        let archive = store.collection("archive", policy.clone()).unwrap();
        let blockdag = store.collection("blockdag", policy).unwrap();
        store.close().unwrap();
        let session = directory.path().join("renamed.jsonl");
        fs::write(&session, CODE).unwrap();
        Self {
            archive: hex::encode(archive.handle().raw),
            blockdag: hex::encode(blockdag.handle().raw),
            directory,
            pile,
            key,
            session,
        }
    }

    /// `archive import` with a scrubbed environment plus exactly `env`.
    fn import(&self, env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_archive"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("TRIBLESPACE_") {
                command.env_remove(name);
            }
        }
        command.env_remove("PILE").env_remove("PERSONA");
        for (name, value) in env {
            command.env(name, value);
        }
        command
            .current_dir(self.directory.path())
            .arg("--pile")
            .arg(&self.pile)
            .arg("--key")
            .arg(&self.key)
            .args(["import", "--source", "claude-code"])
            .arg(&self.session)
            .output()
            .unwrap()
    }

    /// COMMIT records per collection handle, read from the pile itself.
    fn commits(&self) -> std::collections::BTreeMap<String, usize> {
        commits_in(&self.pile)
    }

    /// One COMMIT signed by this host into the retired `blockdag` root, as
    /// the old build's imports left there.
    fn seed_blockdag(&self) {
        let host = faculties::storage::load_signer(&self.pile, Some(&self.key)).unwrap();
        let mut handle = [0_u8; 32];
        hex::decode_to_slice(&self.blockdag, &mut handle).unwrap();
        let mut store = Pile::open(&self.pile).unwrap();
        store
            .insert(CollectionRecord::Commit(CollectionCommit::sign(
                &host,
                Inline::new(handle),
                Inline::new([0x61; 32]),
                Inline::new([0x62; 32]),
            )))
            .unwrap();
        store.close().unwrap();
    }

    fn length(&self) -> u64 {
        fs::metadata(&self.pile).unwrap().len()
    }
}

fn commits_in(pile: &Path) -> std::collections::BTreeMap<String, usize> {
    let mut store = Pile::open(pile).unwrap();
    let snapshot = store.snapshot().unwrap();
    let mut counts = std::collections::BTreeMap::new();
    for record in snapshot.records().unwrap() {
        if let CollectionRecord::Commit(commit) = record.unwrap() {
            *counts
                .entry(hex::encode(commit.collection().raw))
                .or_insert(0) += 1;
        }
    }
    drop(snapshot);
    store.close().unwrap();
    counts
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The ARCHIVE variable routes `archive import` into the fresh `archive`
/// collection, and nothing lands in the retired one.
#[test]
fn import_writes_into_the_fresh_archive_collection_under_the_archive_variable() {
    let fixture = Fixture::new();
    let output = fixture.import(&[("TRIBLESPACE_COLLECTION_ARCHIVE", &fixture.archive)]);
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Archive collection: 1 piece(s)"),
        "{stdout}"
    );
    let commits = fixture.commits();
    eprintln!("commits by collection: {commits:?}");
    assert!(commits.get(&fixture.archive).copied().unwrap_or(0) > 0);
    assert_eq!(commits.get(&fixture.blockdag), None);
    assert_eq!(commits.len(), 1, "only the configured archive was written");

    // The switch window: the retired variable beside the current one is
    // ignored, and the import is idempotent into the same collection.
    let again = fixture.import(&[
        ("TRIBLESPACE_COLLECTION_ARCHIVE", &fixture.archive),
        ("TRIBLESPACE_COLLECTION_BLOCKDAG", &fixture.blockdag),
    ]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(fixture.commits(), commits);
}

/// A stale environment carrying only the BLOCKDAG variable is refused before
/// anything is appended, whichever collection its value names.
#[test]
fn a_stale_blockdag_variable_alone_appends_nothing() {
    let fixture = Fixture::new();
    let before = fixture.length();
    for value in [&fixture.blockdag, &fixture.archive] {
        let output = fixture.import(&[("TRIBLESPACE_COLLECTION_BLOCKDAG", value)]);
        let message = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{message}");
        assert!(
            message.contains("TRIBLESPACE_COLLECTION_BLOCKDAG"),
            "{message}"
        );
        assert!(
            message.contains("TRIBLESPACE_COLLECTION_ARCHIVE"),
            "{message}"
        );
        assert_eq!(fixture.length(), before, "a refused import appended");
    }
    assert!(fixture.commits().is_empty());
}

/// Pointing the new variable at the retired descriptor is refused by the
/// descriptor's own name, before anything is appended.
#[test]
fn the_archive_variable_cannot_select_the_retired_blockdag_descriptor() {
    let fixture = Fixture::new();
    let before = fixture.length();
    let output = fixture.import(&[("TRIBLESPACE_COLLECTION_ARCHIVE", &fixture.blockdag)]);
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(message.contains("\"blockdag\""), "{message}");
    assert!(
        message.contains("not expected faculty collection"),
        "{message}"
    );
    assert_eq!(fixture.length(), before);
    assert!(fixture.commits().is_empty());
}

/// With no collection variable at all, a pile whose retired `blockdag` holds
/// content refuses the import, naming both the old name and the variable to
/// set, and writes no COMMIT. Before the rename the same refusal came from the
/// same-name generation check; the rename must not turn it into a silent
/// private `archive`. With an empty `blockdag` the import proceeds as on any
/// fresh pile.
#[test]
fn no_variable_beside_a_blockdag_with_content_is_refused() {
    let fixture = Fixture::new();
    fixture.seed_blockdag();
    let before = fixture.commits();
    assert_eq!(before.get(&fixture.blockdag), Some(&1));
    let output = fixture.import(&[]);
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(message.contains("\"blockdag\""), "{message}");
    assert!(
        message.contains("TRIBLESPACE_COLLECTION_ARCHIVE"),
        "{message}"
    );
    assert!(message.contains(&fixture.blockdag), "{message}");
    assert_eq!(fixture.commits(), before, "a refused import committed");

    let empty = Fixture::new();
    let output = empty.import(&[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(empty.commits().get(&empty.blockdag), None);
}
