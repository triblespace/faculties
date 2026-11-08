use super::*;
use clap::Parser;
use triblespace::core::collection::CollectionStoreExt;
use triblespace::core::metadata;

use crate::schemas::{config::config, workspace as attrs};

#[derive(Parser, Debug)]
struct Arguments {
    #[command(flatten)]
    selection: WorkspaceArgs,
}

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
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        Self {
            _directory: directory,
            storage: Storage::new(pile, Some(key)),
        }
    }

    fn publish(&self, fragment: Fragment) {
        self.storage
            .with_pile(|pile, signer| {
                let config = pile.collection(
                    crate::config::COLLECTION_NAME,
                    crate::collection_names::private_policy(signer.verifying_key()),
                )?;
                pile.commit(config, signer, fragment)?;
                Ok(())
            })
            .unwrap();
    }

    fn root(&self, name: &str) -> triblespace::core::collection::CollectionHandle {
        self.storage
            .with_pile(|pile, signer| {
                Ok(pile
                    .collection(
                        name,
                        crate::collection_names::private_policy(signer.verifying_key()),
                    )?
                    .handle())
            })
            .unwrap()
    }

    fn selected(&self, revision: Id) -> Result<Storage> {
        WorkspaceArgs {
            workspace: Some(revision),
        }
        .storage(
            self.storage.path().to_owned(),
            self.storage.key_path().map(Path::to_owned),
        )
    }

    fn listing(&self) -> String {
        let mut text = String::new();
        list(
            self.storage.path(),
            self.storage.key_path(),
            &mut crate::out::Out::new(&mut |part| {
                match part {
                    crate::out::Part::Text { text: next } => text.push_str(&next),
                    _ => panic!("workspace discovery is text"),
                }
                Ok(())
            }),
        )
        .unwrap();
        text
    }
}

fn binding(
    revision: &ExclusiveId,
    anchor: Id,
    root: triblespace::core::collection::CollectionHandle,
) -> Fragment {
    let route = entity! {
        config::anchor: crate::schemas::wiki::DEFAULT_SCOPE_ID,
        config::selects: root,
    };
    entity! { revision @
        metadata::tag: attrs::WORKSPACE, config::anchor: anchor,
        attrs::enrolled: true, metadata::name: "Same label",
        attrs::workspace_route*: route,
    }
}

#[test]
fn exact_revision_grammar_rejects_labels_prefixes_and_whitespace() {
    let id = genid().id;
    assert_eq!(parse_revision(&format!("{id:x}")).unwrap(), id);
    assert_eq!(parse_revision(&format!("{id:X}")).unwrap(), id);
    for invalid in ["", "Research", "abcd", "0000000000000000000000000000000g"] {
        assert!(parse_revision(invalid).is_err());
    }
    assert!(parse_revision(&format!(" {id:x}")).is_err());
    assert!(parse_revision(&format!("{id:x} ")).is_err());
}

#[test]
fn absent_selector_is_lazy_legacy_storage_and_missing_bootstrap_is_not_created() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("missing.pile");
    let key = directory.path().join("missing.key");
    let legacy = WorkspaceArgs::default()
        .storage(pile.clone(), Some(key.clone()))
        .unwrap();
    assert!(legacy.collection_routes().is_none());
    assert!(!pile.exists());
    assert!(!key.exists());
    assert!(WorkspaceArgs {
        workspace: Some(genid().id)
    }
    .storage(pile.clone(), Some(key.clone()))
    .is_err());
    assert!(!pile.exists());
    assert!(!key.exists());
}

#[test]
fn selected_cli_help_does_not_resolve_the_destination() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("missing.pile");
    let key = directory.path().join("missing.key");
    let revision = format!("{:X}", genid().id);
    let arguments = [
        "compass",
        "--pile",
        pile.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "--workspace",
        &revision,
    ];
    let cli = crate::compass::cli::Cli::try_parse_from(arguments).unwrap();
    let mut text = String::new();
    crate::compass::cli::execute(
        cli,
        &mut crate::out::Out::new(&mut |part| {
            if let crate::out::Part::Text { text: next } = part {
                text.push_str(&next);
            }
            Ok(())
        }),
    )
    .unwrap();
    assert!(text.contains("--workspace"));
    crate::wiki::cli::execute_from(arguments, &mut crate::out::Out::new(&mut |_| Ok(()))).unwrap();
    assert!(!pile.exists());
    assert!(!key.exists());
}

#[test]
fn missing_config_lists_empty_without_creating_roots_or_selecting_a_default() {
    let fixture = Fixture::new();
    let before = std::fs::read(fixture.storage.path()).unwrap();
    assert!(fixture.listing().contains("No configured workspaces"));
    assert!(fixture
        .selected(genid().id)
        .unwrap_err()
        .to_string()
        .contains("unavailable"));
    assert_eq!(before, std::fs::read(fixture.storage.path()).unwrap());
}

#[test]
fn exact_selection_keeps_equal_labels_separate_and_missing_roles_unavailable() {
    let fixture = Fixture::new();
    let first_root = fixture.root("first data");
    let second_root = fixture.root("second data");
    let first = genid();
    let second = genid();
    fixture.publish(binding(&first, genid().id, first_root));
    fixture.publish(binding(&second, genid().id, second_root));
    let before = std::fs::read(fixture.storage.path()).unwrap();
    let chosen = fixture.selected(first.id).unwrap();
    assert_eq!(
        chosen
            .collection_handle(crate::schemas::wiki::DEFAULT_SCOPE_ID)
            .unwrap(),
        Some(first_root)
    );
    assert_eq!(
        fixture
            .selected(second.id)
            .unwrap()
            .collection_handle(crate::schemas::wiki::DEFAULT_SCOPE_ID)
            .unwrap(),
        Some(second_root)
    );
    assert!(chosen
        .collection_handle(crate::schemas::files::DEFAULT_SCOPE_ID)
        .unwrap_err()
        .to_string()
        .contains("unconfigured"));
    let listing = fixture.listing();
    assert!(listing.contains(&format!("{:X}", first.id)));
    assert!(listing.contains(&format!("{:X}", second.id)));
    assert!(listing.contains(&hex::encode(first_root.raw)));
    assert_eq!(before, std::fs::read(fixture.storage.path()).unwrap());
}

#[test]
fn stale_withdrawn_unknown_and_conflicted_revisions_never_retarget() {
    let fixture = Fixture::new();
    let root = fixture.root("data");
    let anchor = genid().id;
    let old = genid();
    let next = genid();
    fixture.publish(binding(&old, anchor, root));
    let mut successor = binding(&next, anchor, root);
    successor += entity! { &next @ metadata::supersedes: old.id };
    fixture.publish(successor);
    assert!(fixture.selected(old.id).is_err());
    assert!(fixture.selected(next.id).is_ok());
    assert!(fixture.selected(genid().id).is_err());
    let concurrent = genid();
    fixture.publish(binding(&concurrent, anchor, root));
    assert!(fixture
        .selected(next.id)
        .unwrap_err()
        .to_string()
        .contains("concurrent revisions"));
    let listing = fixture.listing();
    assert!(listing.contains(&format!("{:X}  unavailable:", next.id)));
    assert!(listing.contains(&format!("{:X}  unavailable:", concurrent.id)));
    fixture.publish(entity! {
        metadata::tag: attrs::WORKSPACE, config::anchor: anchor,
        attrs::enrolled: false, metadata::supersedes*: [next.id, concurrent.id],
    });
    assert!(fixture.selected(next.id).is_err());
    assert!(fixture.selected(concurrent.id).is_err());
}

#[test]
fn selected_signer_is_pinned_before_ordinary_storage_opens() {
    let fixture = Fixture::new();
    let revision = genid();
    fixture.publish(binding(&revision, genid().id, fixture.root("data")));
    let selected = fixture.selected(revision.id).unwrap();
    let replacement = fixture._directory.path().join("replacement.key");
    crate::storage::initialize_signer(fixture.storage.path(), Some(&replacement)).unwrap();
    std::fs::rename(replacement, fixture.storage.key_path().unwrap()).unwrap();
    let error = selected.with_pile(|_, _| Ok(())).unwrap_err();
    assert!(error.to_string().contains("workspace signer changed"));
}

// Each environment-sensitive case runs in a child test process: no unsafe
// process-global environment mutation and no inherited live pile/key access.
#[test]
fn environment_precedence_and_role_overrides_are_process_local() {
    let first = format!("{:X}", genid().id);
    let second = format!("{:X}", genid().id);
    for case in [
        "absent",
        "environment",
        "explicit",
        "invalid",
        "empty",
        "routes",
        "reset",
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "cli_workspace::tests::environment_child",
                "--nocapture",
            ])
            .env("FACULTIES_WORKSPACE_TEST_CASE", case)
            .env("FACULTIES_WORKSPACE_TEST_FIRST", &first)
            .env("FACULTIES_WORKSPACE_TEST_SECOND", &second)
            .env_remove("FACULTIES_WORKSPACE")
            .env_remove("PILE")
            .env_remove("TRIBLESPACE_KEY");
        match case {
            "environment" | "routes" | "reset" => {
                child.env("FACULTIES_WORKSPACE", &first);
            }
            "explicit" | "invalid" => {
                child.env("FACULTIES_WORKSPACE", "not-an-id");
            }
            "empty" => {
                child.env("FACULTIES_WORKSPACE", "");
            }
            _ => {}
        }
        child
            .env("TRIBLESPACE_COLLECTION_WIKI", "invalid-override")
            .env("TRIBLESPACE_COLLECTION_CONFIG", "invalid-override");
        if case == "reset" {
            // Model the collection environment reset for this child invocation
            // only. It must not erase the separately named workspace selector.
            for (name, _) in std::env::vars_os() {
                if name.to_string_lossy().starts_with("TRIBLESPACE_") {
                    child.env_remove(name);
                }
            }
            for name in [
                "TRIBLESPACE_KEY",
                "TRIBLESPACE_COLLECTION_WIKI",
                "TRIBLESPACE_COLLECTION_CONFIG",
            ] {
                child.env_remove(name);
            }
        }
        let result = child.output().unwrap();
        assert!(
            result.status.success(),
            "{case}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn environment_child() {
    let Ok(case) = std::env::var("FACULTIES_WORKSPACE_TEST_CASE") else {
        return;
    };
    let first = std::env::var("FACULTIES_WORKSPACE_TEST_FIRST").unwrap();
    let second = std::env::var("FACULTIES_WORKSPACE_TEST_SECOND").unwrap();
    match case.as_str() {
        "absent" => assert!(Arguments::try_parse_from(["test"])
            .unwrap()
            .selection
            .workspace
            .is_none()),
        "environment" => assert_eq!(
            Arguments::try_parse_from(["test"])
                .unwrap()
                .selection
                .workspace,
            Some(parse_revision(&first).unwrap())
        ),
        "explicit" => assert_eq!(
            Arguments::try_parse_from(["test", "--workspace", &second])
                .unwrap()
                .selection
                .workspace,
            Some(parse_revision(&second).unwrap())
        ),
        "invalid" | "empty" => assert!(Arguments::try_parse_from(["test"]).is_err()),
        "routes" => exact_selection_keeps_equal_labels_separate_and_missing_roles_unavailable(),
        "reset" => {
            assert!(!std::env::vars_os()
                .any(|(name, _)| name.to_string_lossy().starts_with("TRIBLESPACE_")));
            let arguments = Arguments::try_parse_from(["test"]).unwrap();
            let revision = parse_revision(&first).unwrap();
            assert_eq!(arguments.selection.workspace, Some(revision));
            let fixture = Fixture::new();
            let root = fixture.root("data");
            fixture.publish(binding(ExclusiveId::force_ref(&revision), genid().id, root));
            let selected = arguments
                .selection
                .storage(
                    fixture.storage.path().to_owned(),
                    fixture.storage.key_path().map(Path::to_owned),
                )
                .unwrap();
            assert_eq!(selected.selected_workspace().unwrap().id, revision);
            assert_eq!(
                selected
                    .collection_handle(crate::schemas::wiki::DEFAULT_SCOPE_ID)
                    .unwrap(),
                Some(root)
            );
            assert!(selected
                .collection_handle(crate::schemas::files::DEFAULT_SCOPE_ID)
                .unwrap_err()
                .to_string()
                .contains("unconfigured"));
        }
        _ => panic!("unknown environment case"),
    }
}
