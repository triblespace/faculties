use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ed25519_dalek::SigningKey;
use tempfile::TempDir;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::collection::{
    grant_collection_write, CollectionRead, CollectionRecord, CollectionSnapshotExt,
    CollectionStoreExt,
};
use triblespace::core::inline::encodings::hash::Handle;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{BlobStoreList, SnapshotSource};

struct TestPile {
    _directory: TempDir,
    pile: PathBuf,
    tenant_key: PathBuf,
}

impl TestPile {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create collection target fixture");
        let pile = directory.path().join("shared.pile");
        let tenant_key = directory.path().join("tenant.key");
        fs::File::create(&pile).expect("create pile");
        faculties::storage::initialize_signer(&pile, Some(&tenant_key))
            .expect("initialize tenant signer");
        Self {
            _directory: directory,
            pile,
            tenant_key,
        }
    }

    fn key(&self, name: &str) -> PathBuf {
        let key = self._directory.path().join(name);
        faculties::storage::initialize_signer(&self.pile, Some(&key)).expect("initialize signer");
        key
    }
}

fn relations(fixture: &TestPile, key: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_relations"));
    command
        .arg("--pile")
        .arg(&fixture.pile)
        .arg("--key")
        .arg(key)
        .env_remove("TRIBLESPACE_PEERS");
    command
}

fn succeeded(output: Output) -> Output {
    assert!(
        output.status.success(),
        "relations failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

/// The collections the commits in `pile` went to, one entry per commit, in
/// byte order.
fn committed_to(pile: &Path) -> Vec<[u8; 32]> {
    let mut pile = Pile::open(pile).expect("open fixture pile");
    let mut collections: Vec<_> = pile
        .snapshot()
        .expect("freeze fixture pile")
        .records()
        .expect("read records")
        .map(|record| record.expect("decode record"))
        .filter_map(|record| match record {
            CollectionRecord::Commit(commit) => Some(commit.collection().raw),
            _ => None,
        })
        .collect();
    pile.close().expect("close fixture pile");
    collections.sort_unstable();
    collections
}

#[test]
fn a_named_target_retains_an_offline_cli_commit_until_write_is_granted() {
    let fixture = TestPile::new();
    let root = SigningKey::from_bytes(&[0x41; 32]);
    let tenant = faculties::storage::load_signer(&fixture.pile, Some(&fixture.tenant_key))
        .expect("load tenant signer");

    let mut pile = Pile::open(&fixture.pile).expect("open fixture pile");
    let collection = pile
        .collection(
            "relations",
            faculties::collection_names::private_policy(root.verifying_key()),
        )
        .expect("register shared relations collection");
    pile.close().expect("close initialized fixture pile");
    let handle = hex::encode(collection.handle().raw);

    // Local publication retains a signed claim even without present WRITE
    // admission. Admission is a property of the reader's frozen evidence.
    succeeded(
        relations(&fixture, &fixture.tenant_key)
            .args(["--target", &handle, "add", "Ada"])
            .output()
            .expect("run relations"),
    );

    let mut pile = Pile::open(&fixture.pile).expect("reopen fixture pile");
    let frozen = pile.snapshot().expect("freeze pre-grant evidence");
    let original = {
        let mut commits = frozen
            .records()
            .expect("read retained collection records")
            .map(|record| record.expect("decode retained record"))
            .filter_map(|record| match record {
                CollectionRecord::Commit(commit) => Some(commit),
                _ => None,
            });
        let commit = commits.next().expect("the CLI retained one signed claim");
        assert!(commits.next().is_none(), "one invocation emits one COMMIT");
        commit
    };
    assert_eq!(original.collection(), collection.handle());
    assert_eq!(original.public_key().raw, tenant.verifying_key().to_bytes());
    let member = Handle::<SimpleArchive>::from_hash(original.data());
    assert!(frozen.contains_blob(member).unwrap());
    let unadmitted = frozen
        .collection(collection)
        .expect("observe collection before the grant");
    assert!(unadmitted.support().unwrap().is_empty());
    assert!(unadmitted.cover().is_empty());

    grant_collection_write(
        &mut pile,
        collection.handle(),
        &root,
        tenant.verifying_key(),
    )
    .expect("grant tenant WRITE");
    // There is deliberately no second CLI call: the new evidence admits the
    // original member, without another emission, signature, or entity id.
    let after = pile.snapshot().expect("freeze post-grant evidence");
    let admitted = after
        .collection(collection)
        .expect("observe collection after the grant");
    assert_eq!(admitted.support().unwrap().len(), 1);
    assert!(admitted.support().unwrap().contains(member));
    assert_eq!(admitted.cover().len(), 1);
    assert!(admitted.cover().contains(member));
    assert!(frozen
        .collection(collection)
        .expect("reobserve the frozen pre-grant evidence")
        .support()
        .unwrap()
        .is_empty());
    pile.close().expect("close granted fixture pile");
}

/// The variables that used to select a faculty's collection are ignored: a
/// shell carrying one reads and writes exactly what a clean shell does.
#[test]
fn collection_environment_variables_change_nothing() {
    let fixture = TestPile::new();
    let other_key = fixture.key("other.key");
    let tenant = faculties::storage::load_signer(&fixture.pile, Some(&fixture.tenant_key))
        .expect("load tenant signer");
    let other = faculties::storage::load_signer(&fixture.pile, Some(&other_key))
        .expect("load other signer");

    succeeded(
        relations(&fixture, &fixture.tenant_key)
            .args(["add", "Ada"])
            .output()
            .expect("run relations"),
    );
    succeeded(
        relations(&fixture, &other_key)
            .args(["add", "Bob"])
            .output()
            .expect("run relations"),
    );
    let mut pile = Pile::open(&fixture.pile).expect("open fixture pile");
    let mine = faculties::collection_names::open(
        &mut pile,
        faculties::schemas::relations::DEFAULT_SCOPE_ID,
        tenant.verifying_key(),
    )
    .expect("the tenant's relations");
    let theirs = faculties::collection_names::open(
        &mut pile,
        faculties::schemas::relations::DEFAULT_SCOPE_ID,
        other.verifying_key(),
    )
    .expect("the other key's relations");
    pile.close().expect("close fixture pile");
    let mut expected = vec![mine.handle().raw, theirs.handle().raw];
    expected.sort_unstable();
    assert_eq!(committed_to(&fixture.pile), expected);

    let stale = |command: &mut Command| {
        command
            .env(
                "TRIBLESPACE_COLLECTION_RELATIONS",
                hex::encode(theirs.handle().raw),
            )
            .env("TRIBLESPACE_COLLECTION_WIKI", "not even a handle");
    };
    let clean = succeeded(
        relations(&fixture, &fixture.tenant_key)
            .arg("list")
            .output()
            .expect("run relations"),
    );
    let mut with_environment = relations(&fixture, &fixture.tenant_key);
    stale(&mut with_environment);
    let with_environment = succeeded(with_environment.arg("list").output().expect("run"));
    assert_eq!(with_environment.stdout, clean.stdout);
    assert!(String::from_utf8_lossy(&clean.stdout).contains("Ada"));
    assert!(!String::from_utf8_lossy(&clean.stdout).contains("Bob"));

    let mut write = relations(&fixture, &fixture.tenant_key);
    stale(&mut write);
    succeeded(write.args(["add", "Cy"]).output().expect("run relations"));
    expected.push(mine.handle().raw);
    expected.sort_unstable();
    assert_eq!(
        committed_to(&fixture.pile),
        expected,
        "the write goes to the default target, not the collection the variable names"
    );
}
