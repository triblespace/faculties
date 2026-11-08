//! Selected frontend contracts with scratch storage only: no device, model,
//! account, service or live pile is involved.
use crate::out::Out;
use crate::schemas::{config::config, workspace as attrs};
use crate::storage::Storage;
use clap::Parser;
use std::collections::BTreeMap;
use triblespace::core::collection::{Collection, CollectionHandle, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::*;

struct Fixture {
    directory: tempfile::TempDir,
    storage: Storage,
    revision: Id,
    roots: BTreeMap<Id, CollectionHandle>,
    siblings: BTreeMap<Id, CollectionHandle>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("node.pile");
        let key = directory.path().join("node.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let storage = Storage::new(pile, Some(key));
        let revision = genid();
        let mut roots = BTreeMap::new();
        let mut siblings = BTreeMap::new();
        storage
            .with_pile(|pile, signer| {
                let policy = crate::collection_names::private_policy(signer.verifying_key());
                let mut routes = Fragment::empty();
                for role in [
                    crate::schemas::body::DEFAULT_SCOPE_ID,
                    crate::schemas::relations::DEFAULT_SCOPE_ID,
                ] {
                    let selected = pile
                        .collection(&format!("selected {role:X}"), policy.clone())?
                        .handle();
                    let sibling = pile
                        .collection(&format!("sibling {role:X}"), policy.clone())?
                        .handle();
                    roots.insert(role, selected);
                    siblings.insert(role, sibling);
                    routes += entity! { config::anchor: role, config::selects: selected };
                }
                let config = pile.collection(crate::config::COLLECTION_NAME, policy)?;
                pile.commit(
                    config,
                    signer,
                    entity! { &revision @
                        metadata::tag: attrs::WORKSPACE, config::anchor: genid().id,
                        attrs::enrolled: true, metadata::name: "Selected data",
                        attrs::workspace_route*: routes,
                    },
                )?;
                Ok(())
            })
            .unwrap();
        Self {
            directory,
            storage,
            revision: revision.id,
            roots,
            siblings,
        }
    }

    fn args(&self, binary: &str, command: &[&str]) -> Vec<String> {
        let mut args = vec![
            binary.to_owned(),
            "--pile".into(),
            self.storage.path().display().to_string(),
            "--key".into(),
            self.storage.key_path().unwrap().display().to_string(),
            "--workspace".into(),
            format!("{:X}", self.revision),
        ];
        args.extend(command.iter().map(|arg| (*arg).to_owned()));
        args
    }

    fn facts(&self, root: CollectionHandle) -> TribleSet {
        self.storage
            .with_pile(|pile, _| {
                let snapshot = pile.snapshot()?;
                Ok(
                    Collection::<blobencodings::SimpleArchive>::open(&snapshot, root)?
                        .read(&snapshot)?,
                )
            })
            .unwrap()
    }
}

#[test]
fn body_intent_publication_uses_only_the_requested_root() {
    let fixture = Fixture::new();
    let cli = crate::body::cli::Cli::try_parse_from(
        fixture.args("body", &["intent", "look toward the light"]),
    )
    .unwrap();
    crate::body::cli::execute(cli, &mut Out::new(&mut |_| Ok(()))).unwrap();
    let role = crate::schemas::body::DEFAULT_SCOPE_ID;
    assert!(!fixture.facts(fixture.roots[&role]).is_empty());
    assert!(fixture.facts(fixture.siblings[&role]).is_empty());
}

#[test]
fn linkedin_local_import_routes_relations_without_an_account_or_network() {
    let fixture = Fixture::new();
    let source = fixture.directory.path().join("connections.json");
    std::fs::write(&source, br#"[{"First Name":"Ada","Last Name":"Example","URL":"https://www.linkedin.com/in/synthetic-fixture"}]"#).unwrap();
    let cli = crate::linkedin::cli::Cli::try_parse_from(
        fixture.args("linkedin", &["import", source.to_str().unwrap()]),
    )
    .unwrap();
    crate::linkedin::cli::execute(cli, &mut Out::new(&mut |_| Ok(()))).unwrap();
    let role = crate::schemas::relations::DEFAULT_SCOPE_ID;
    assert!(!fixture.facts(fixture.roots[&role]).is_empty());
    assert!(fixture.facts(fixture.siblings[&role]).is_empty());
}
