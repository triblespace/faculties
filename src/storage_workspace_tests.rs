//! Workspace routing laws, using scratch piles and no process-global mutation.
use super::*;
use anybytes::Bytes;
use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
use triblespace::core::inline::Inline;
use triblespace::core::metadata;
use triblespace::core::repo::StoreSnapshot;
use triblespace::macros::entity;

struct Fixture {
    _directory: tempfile::TempDir,
    storage: Storage,
    signer: SigningKey,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("workspace.pile");
        let key = directory.path().join("workspace.key");
        std::fs::File::create(&pile).unwrap();
        let signer = initialize_signer(&pile, Some(&key)).unwrap();
        let storage =
            Storage::shared_local(pile, Some(key)).with_expected_signer(signer.verifying_key());
        Self {
            _directory: directory,
            storage,
            signer,
        }
    }

    fn root(&self, scope: Id, read: AdmissionPolicy, write: AdmissionPolicy) -> CollectionHandle {
        self.storage
            .with_store(|store, _, _| {
                Ok(store
                    .collection(
                        crate::collection_names::require_name(scope),
                        CollectionPolicy::new(read, write),
                    )?
                    .handle())
            })
            .unwrap()
    }

    fn private(&self, scope: Id) -> CollectionHandle {
        let authority = self.signer.verifying_key();
        self.root(
            scope,
            AdmissionPolicy::direct(authority),
            AdmissionPolicy::direct(authority),
        )
    }

    fn selected(&self, routes: CollectionRoutes) -> Storage {
        self.storage.clone().with_collections(routes)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.storage.close().unwrap();
    }
}

#[test]
fn every_catalog_role_selects_exactly_one_same_named_root() {
    let fixture = Fixture::new();
    for (scope, _) in crate::collection_names::table() {
        let selected = fixture.private(scope);
        let sibling = fixture.root(
            scope,
            AdmissionPolicy::Open,
            AdmissionPolicy::direct(fixture.signer.verifying_key()),
        );
        assert_ne!(selected, sibling);
        let storage = fixture.selected(BTreeMap::from([(scope, selected)]));
        storage
            .with_store(|store, signer, runtime| {
                let a =
                    storage.open_collection_write(store, scope, signer.verifying_key(), runtime)?;
                let b = crate::collection_names::open_exact_in(&store.snapshot()?, scope, sibling)?;
                let first = entity! { metadata::description: "selected-root".to_owned() };
                let expected = first.facts().clone();
                store.commit(a, signer, first)?;
                store.commit(
                    b,
                    signer,
                    entity! { metadata::description: "sibling-root".to_owned() },
                )?;
                let read =
                    storage.open_collection_read(store, scope, signer.verifying_key(), runtime)?;
                assert_eq!(read.handle(), selected);
                let facts: TribleSet = read.read(&store.snapshot()?)?;
                assert_eq!(facts, expected, "a same-name sibling must never be unioned");
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn explicit_missing_primary_or_auxiliary_never_registers_a_private_root() {
    let fixture = Fixture::new();
    let scope = crate::schemas::wiki::DEFAULT_SCOPE_ID;
    let selected = fixture.private(scope);
    let storage = fixture.selected(BTreeMap::from([(scope, selected)]));
    storage
        .with_store(|store, signer, runtime| {
            let before = store.snapshot()?;
            for missing in [
                crate::schemas::relations::DEFAULT_SCOPE_ID,
                crate::schemas::embeddings::DEFAULT_SCOPE_ID,
                crate::schemas::memory::DEFAULT_COMB_SCOPE_ID,
                crate::secrets::DEFAULT_SCOPE_ID,
            ] {
                assert!(storage
                    .open_collection_read(store, missing, signer.verifying_key(), runtime)
                    .unwrap_err()
                    .to_string()
                    .contains("unconfigured"));
                assert!(storage
                    .open_collection_write(store, missing, signer.verifying_key(), runtime)
                    .unwrap_err()
                    .to_string()
                    .contains("unconfigured"));
                assert!(storage
                    .open_collection_local(store, missing, signer.verifying_key())
                    .unwrap_err()
                    .to_string()
                    .contains("unconfigured"));
            }
            assert!(store.snapshot()?.changes_since(&before).is_empty());
            Ok(())
        })
        .unwrap();
}

#[test]
fn read_and_write_admission_are_independent_action_boundaries() {
    let fixture = Fixture::new();
    let scope = crate::schemas::files::DEFAULT_SCOPE_ID;
    let foreign = SigningKey::from_bytes(&[17; 32]).verifying_key();
    let read_only = fixture.root(
        scope,
        AdmissionPolicy::direct(fixture.signer.verifying_key()),
        AdmissionPolicy::direct(foreign),
    );
    let write_only = fixture.root(
        scope,
        AdmissionPolicy::direct(foreign),
        AdmissionPolicy::direct(fixture.signer.verifying_key()),
    );
    for (handle, can_read) in [(read_only, true), (write_only, false)] {
        let storage = fixture.selected(BTreeMap::from([(scope, handle)]));
        storage
            .with_store(|store, signer, runtime| {
                let read =
                    storage.open_collection_read(store, scope, signer.verifying_key(), runtime);
                let write =
                    storage.open_collection_write(store, scope, signer.verifying_key(), runtime);
                assert_eq!(read.is_ok(), can_read);
                assert_eq!(write.is_ok(), !can_read);
                Ok(())
            })
            .unwrap();
    }
    let read_storage = fixture.selected(BTreeMap::from([(scope, read_only)]));
    let files = crate::files::Files::with_storage(read_storage);
    files.list(&[], None).unwrap();
    let error = files
        .add_bytes(
            Bytes::from(b"forbidden".to_vec()),
            "no.txt",
            "text/plain",
            &[],
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("WRITE"));
}

#[test]
fn clones_retained_scopes_and_different_bindings_keep_selection_and_owner_separate() {
    let fixture = Fixture::new();
    let scope = crate::schemas::wiki::DEFAULT_SCOPE_ID;
    let a = fixture.private(scope);
    let b = fixture.root(
        scope,
        AdmissionPolicy::Open,
        AdmissionPolicy::direct(fixture.signer.verifying_key()),
    );
    let original = fixture.selected(BTreeMap::from([(scope, a)]));
    let other = original
        .clone()
        .with_collections(BTreeMap::from([(scope, b)]));
    assert_eq!(original.collection_handle(scope).unwrap(), Some(a));
    assert_eq!(other.collection_handle(scope).unwrap(), Some(b));
    assert!(Arc::ptr_eq(
        original.shared.as_ref().unwrap(),
        other.shared.as_ref().unwrap()
    ));
    let mut once = original.clone();
    once.shared = None;
    let retained = once.retained();
    assert!(retained.local_only);
    assert_eq!(
        retained.expected_signer,
        Some(fixture.signer.verifying_key())
    );
    assert_eq!(retained.collection_routes(), once.collection_routes());
    once.scope(|scope_storage| {
        assert_eq!(scope_storage.collection_handle(scope)?, Some(a));
        assert!(scope_storage.local_only);
        Ok(())
    })
    .unwrap();
    retained.close().unwrap();
}

#[test]
fn resident_only_owner_refuses_missing_bytes_without_network_startup() {
    let fixture = Fixture::new();
    fixture
        .storage
        .with_store(|store, _, runtime| {
            let missing = Inline::<Handle<UnknownBlob>>::new([234; 32]);
            let outcome = runtime
                .block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(2), store.acquire(missing))
                        .await
                })
                .expect("offline missing bytes must fail promptly");
            assert!(format!("{}", outcome.unwrap_err()).contains("closed"));
            // Closing acquisition must not close resident snapshots or publication.
            let handle = store
                .collection(
                    "offline-test",
                    CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
                )?
                .handle();
            assert!(store.snapshot()?.contains_blob(handle)?);
            Ok(())
        })
        .unwrap();
}

#[test]
fn saved_signer_is_checked_before_first_open_and_after_replacement() {
    let fixture = Fixture::new();
    let wrong = SigningKey::from_bytes(&[18; 32]).verifying_key();
    let unopened = Storage::shared_local(
        fixture.storage.path().to_owned(),
        fixture.storage.key_path().map(Path::to_owned),
    )
    .with_expected_signer(wrong);
    assert!(unopened
        .with_store(|_, _, _| Ok(()))
        .unwrap_err()
        .to_string()
        .contains("signer changed"));
    fixture.storage.with_store(|_, _, _| Ok(())).unwrap();
    std::fs::remove_file(fixture.storage.key_path().unwrap()).unwrap();
    initialize_signer(fixture.storage.path(), fixture.storage.key_path()).unwrap();
    assert!(fixture
        .storage
        .with_pile(|_, _| Ok(()))
        .unwrap_err()
        .to_string()
        .contains("signer changed"));
}

#[test]
fn explicit_routes_ignore_contradictory_ambient_overrides() {
    const CHILD: &str = "FACULTIES_WORKSPACE_AMBIENT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "storage::workspace_tests::explicit_routes_ignore_contradictory_ambient_overrides",
            "--nocapture",
        ]);
        command
            .env(CHILD, "1")
            .env("TRIBLESPACE_PEERS", "not an endpoint");
        for (scope, _) in crate::collection_names::table() {
            command.env(
                crate::collection_names::override_env_name(scope),
                "invalid ambient descriptor",
            );
        }
        assert!(command.status().unwrap().success());
        return;
    }
    let fixture = Fixture::new();
    let scope = crate::schemas::files::DEFAULT_SCOPE_ID;
    let selected = fixture.private(scope);
    let storage = fixture.selected(BTreeMap::from([(scope, selected)]));
    crate::files::Files::with_storage(storage)
        .list(&[], None)
        .unwrap();
    assert!(
        crate::collection_names::configured_handle(scope).is_err(),
        "CLI still reads ambient overrides"
    );
    let mut routes = BTreeMap::new();
    for scope in [
        crate::schemas::wiki::DEFAULT_SCOPE_ID,
        crate::schemas::compass::DEFAULT_SCOPE_ID,
    ] {
        routes.insert(
            scope,
            fixture.root(
                scope,
                AdmissionPolicy::Open,
                AdmissionPolicy::direct(fixture.signer.verifying_key()),
            ),
        );
    }
    // Bootstrap's derived indexes must follow the selected roots too; an
    // ambient reopen here would fail after the primary publications.
    crate::bootstrap::import_with_storage(&fixture.selected(routes)).unwrap();
}

#[test]
fn bootstrap_requires_read_before_any_write_or_view_maintenance() {
    let fixture = Fixture::new();
    let other = SigningKey::from_bytes(&[19; 32]).verifying_key();
    for denied_scope in [
        crate::schemas::wiki::DEFAULT_SCOPE_ID,
        crate::schemas::compass::DEFAULT_SCOPE_ID,
    ] {
        let routes = [
            crate::schemas::wiki::DEFAULT_SCOPE_ID,
            crate::schemas::compass::DEFAULT_SCOPE_ID,
        ]
        .into_iter()
        .map(|scope| {
            (
                scope,
                fixture.root(
                    scope,
                    AdmissionPolicy::direct(if scope == denied_scope {
                        other
                    } else {
                        fixture.signer.verifying_key()
                    }),
                    AdmissionPolicy::direct(fixture.signer.verifying_key()),
                ),
            )
        })
        .collect();
        let storage = fixture.selected(routes);
        let before = storage
            .with_store(|store, _, _| Ok(store.snapshot()?))
            .unwrap();
        let error = crate::bootstrap::import_with_storage(&storage).unwrap_err();
        assert!(format!("{error:#}").contains("READ"));
        storage
            .with_store(|store, _, _| {
                assert!(store.snapshot()?.changes_since(&before).is_empty());
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn workspace_model_operations_refuse_ambient_assets_before_opening_them() {
    use crate::mcp::Faculty;
    let fixture = Fixture::new();
    let storage = fixture.selected(BTreeMap::new());
    assert!(storage
        .require_ambient_models("embedding")
        .unwrap_err()
        .to_string()
        .contains("model-asset routing is unconfigured"));
    fixture.storage.require_ambient_models("embedding").unwrap();
    let voice = crate::voice::mcp::Voice::with_storage(storage);
    let error = voice
        .call(
            "voice_synthesize",
            Bytes::from(br#"{"text":"fixture"}"#.to_vec()),
            &mut crate::out::Out::new(&mut |_| Ok(())),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("model-asset routing is unconfigured"));
}

#[test]
fn aggregate_native_primary_and_auxiliary_openers_fail_closed_without_routes() {
    let fixture = Fixture::new();
    let storage = fixture.selected(BTreeMap::new());
    let catalog = crate::mcp::catalog::Catalog::with_storage(
        crate::mcp::catalog::Config::new(storage.path()),
        storage.clone(),
    );
    // Only resident finite reads, plus source-only writes which must fail
    // before publication. No hardware, networks, daemons or callbacks.
    let mut cases = vec![
        ("archive_list", "{}"),
        ("atlas_list", "{}"),
        ("body_list", "{}"),
        ("bootstrap_import", "{}"),
        ("code_find", r#"{"name":"fixture"}"#),
        ("cognition_check", "{}"),
        ("compass_list", "{}"),
        ("decide_list", "{}"),
        ("discord_read", "{}"),
        ("files_list", "{}"),
        ("gauge_health", "{}"),
        ("habit_list", "{}"),
        ("headspace_list", "{}"),
        ("linkedin_review", "{}"),
        ("mail_account_list", "{}"),
        ("memory_list", "{}"),
        ("message_list", r#"{"reader":"fixture"}"#),
        ("planner_list", r#"{"from":"2026-10-08","to":"2026-10-09"}"#),
        ("posture_list", "{}"),
        ("posture_vocab_list", "{}"),
        ("relations_list", "{}"),
        ("secrets_list", "{}"),
        ("status_list", "{}"),
        ("teams_auth_status", "{}"),
        ("triage_scan", "{}"),
        ("voice_route", "{}"),
        (
            "web_search",
            r#"{"query":"must fail before any provider request"}"#,
        ),
        ("reason_record", r#"{"text":"must not publish"}"#),
        (
            "patience_extend",
            r#"{"turn_id":"01010101010101010101010101010101","worker_id":"02020202020202020202020202020202","duration_ms":1}"#,
        ),
    ];
    cases.sort_unstable();
    let registrations = catalog.registrations();
    let before = storage
        .with_store(|store, _, _| Ok(store.snapshot()?))
        .unwrap();
    for (name, arguments) in cases {
        let faculty = registrations
            .iter()
            .find(|faculty| faculty.tools().iter().any(|tool| tool.name == name))
            .unwrap_or_else(|| panic!("missing catalog tool {name}"));
        let error = faculty
            .call(
                name,
                Bytes::from(arguments.as_bytes().to_vec()),
                &mut crate::out::Out::new(&mut |_| Ok(())),
            )
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("unconfigured"),
            "{name}: {error:#}"
        );
    }
    storage
        .with_store(|store, _, _| {
            assert!(store.snapshot()?.changes_since(&before).is_empty());
            Ok(())
        })
        .unwrap();
    drop(registrations);
    catalog.close().unwrap();
}
