//! Frozen selection and invocation boundaries over disposable local piles.
use super::*;
use crate::schemas::{config::config, workspace as attrs};
use triblespace::core::metadata;
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::prelude::*;

struct Fixture {
    _directory: tempfile::TempDir,
    storage: Storage,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("node.pile");
        let key = directory.path().join("node.key");
        std::fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        Self {
            _directory: directory,
            storage: Storage::new(pile, Some(key)),
        }
    }

    fn publish(&self, fragment: Fragment) {
        self.storage
            .with_pile(|pile, signer| {
                let collection = crate::collection_names::open(
                    pile,
                    crate::schemas::config::DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                )?;
                pile.commit(collection, signer, fragment)?;
                Ok(())
            })
            .unwrap();
    }

    fn pin(&self, revision: Id) -> Storage {
        let (workspace, signer) = self
            .storage
            .with_pile(|pile, signer| {
                Ok((
                    crate::workspace::resolve_in(
                        &pile.snapshot()?,
                        signer.verifying_key(),
                        revision,
                    )?
                    .expect("fixture workspace"),
                    signer.verifying_key(),
                ))
            })
            .unwrap();
        self.storage.clone().with_workspace(workspace, signer)
    }
}

fn binding(revision: &ExclusiveId, anchor: Id) -> Fragment {
    entity! { revision @ metadata::tag: attrs::WORKSPACE, config::anchor: anchor,
    attrs::enrolled: true, metadata::name: "Pinned workspace" }
}

#[test]
fn both_operation_entry_points_reject_a_superseded_pin_before_the_callback() {
    for retained in [false, true] {
        let fixture = Fixture::new();
        let original = genid();
        let anchor = genid().id;
        fixture.publish(binding(&original, anchor));
        let pin = fixture.pin(original.id);
        let pin = if retained { pin.retained() } else { pin };
        pin.with_store(|_, _, _| Ok(())).unwrap();
        pin.with_pile(|_, _| Ok(())).unwrap();
        if retained {
            assert_eq!(pin.try_with_store(|_, _, _| Ok(())).unwrap(), Some(()));
        }
        let next = genid();
        fixture.publish(
            binding(&next, anchor) + entity! { &next @ metadata::supersedes: original.id },
        );
        assert!(pin
            .with_store(|_, _, _| -> Result<()> { panic!("stale store invocation admitted") })
            .is_err());
        assert!(pin
            .with_pile(|_, _| -> Result<()> { panic!("stale pile invocation admitted") })
            .is_err());
        if retained {
            assert!(pin
                .try_with_store(|_, _, _| -> Result<()> {
                    panic!("stale nonblocking invocation admitted")
                })
                .is_err());
        }
        assert_eq!(pin.selected_workspace().unwrap().id, original.id);
        assert!(pin.collection_routes().unwrap().is_empty());
        pin.close().unwrap();
    }
}

#[test]
fn an_admitted_operation_finishes_but_the_next_borrowed_boundary_refuses_withdrawal() {
    let fixture = Fixture::new();
    let revision = genid();
    let anchor = genid().id;
    fixture.publish(binding(&revision, anchor));
    let pin = fixture.pin(revision.id).retained();
    pin.with_store(|store, signer, _| {
        let before = store.snapshot()?;
        pin.validate_workspace_in(&before, signer.verifying_key())?;
        let config_collection = crate::collection_names::open(
            store,
            crate::schemas::config::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
        )?;
        store.commit(
            config_collection,
            signer,
            entity! {
                metadata::tag: attrs::WORKSPACE, config::anchor: anchor,
                attrs::enrolled: false, metadata::supersedes: revision.id,
            },
        )?;
        // A frozen observation remains valid in its own frame. Validating a
        // later frame uses this borrowed owner, not a nested Storage lease.
        pin.validate_workspace_in(&before, signer.verifying_key())?;
        assert!(pin
            .validate_workspace_at(store, signer.verifying_key())
            .is_err());
        Ok(())
    })
    .unwrap();
    assert!(pin
        .with_store(|_, _, _| -> Result<()> { panic!("withdrawn invocation admitted") })
        .is_err());
    assert_eq!(pin.selected_workspace().unwrap().id, revision.id);
    pin.close().unwrap();
}

#[test]
fn annotations_that_change_the_binding_do_not_mutate_the_frozen_routes() {
    let fixture = Fixture::new();
    let revision = genid();
    fixture.publish(binding(&revision, genid().id));
    let pin = fixture.pin(revision.id);
    let route = entity! { config::anchor: crate::schemas::wiki::DEFAULT_SCOPE_ID,
    config::selects: CollectionHandle::new([7; 32]) };
    fixture.publish(entity! { &revision @ attrs::workspace_route*: route });
    let error = pin.with_pile(|_, _| Ok(())).unwrap_err();
    assert!(
        error.to_string().contains("changed or is unavailable"),
        "{error:#}"
    );
    assert!(pin.collection_routes().unwrap().is_empty());
    assert!(pin
        .collection_handle(crate::schemas::wiki::DEFAULT_SCOPE_ID)
        .is_err());
}

#[test]
fn concurrent_configuration_is_an_error_not_a_choice_of_another_revision() {
    let fixture = Fixture::new();
    let revision = genid();
    let anchor = genid().id;
    fixture.publish(binding(&revision, anchor));
    let pin = fixture.pin(revision.id);
    fixture.publish(binding(&genid(), anchor));
    let error = pin.with_pile(|_, _| Ok(())).unwrap_err();
    assert!(format!("{error:#}").contains("concurrent revisions"));
    assert_eq!(pin.selected_workspace().unwrap().id, revision.id);
}

#[test]
fn unchanged_binding_and_irrelevant_annotations_keep_the_pin_and_identity() {
    let fixture = Fixture::new();
    let revision = genid();
    fixture.publish(binding(&revision, genid().id));
    let pin = fixture.pin(revision.id);
    fixture.publish(entity! { &revision @ metadata::description: "not part of routing" });
    pin.with_pile(|pile, signer| {
        pin.validate_workspace_at(pile, signer.verifying_key())?;
        let wrong = SigningKey::from_bytes(&[86; 32]).verifying_key();
        assert!(pin.validate_workspace_in(&pile.snapshot()?, wrong).is_err());
        Ok(())
    })
    .unwrap();
    let retained = pin.clone().retained();
    assert_eq!(retained.selected_workspace(), pin.selected_workspace());
    assert_eq!(retained.collection_routes(), pin.collection_routes());
    retained.with_store(|_, _, _| Ok(())).unwrap();
    retained.close().unwrap();
}

#[test]
fn unpinned_storage_adds_no_snapshot_or_configuration_policy() {
    struct Unobserved;
    impl SnapshotSource for Unobserved {
        type Snapshot = PileSnapshot;
        type SnapshotError = std::io::Error;
        fn snapshot(&mut self) -> std::io::Result<Self::Snapshot> {
            panic!("unselected legacy operations must not pay for workspace snapshots")
        }
    }
    let storage = Storage::new(PathBuf::new(), None);
    let signer = SigningKey::from_bytes(&[87; 32]).verifying_key();
    storage
        .validate_workspace_at(&mut Unobserved, signer)
        .unwrap();
    storage
        .with_collections(BTreeMap::new())
        .validate_workspace_at(&mut Unobserved, signer)
        .unwrap();
}
