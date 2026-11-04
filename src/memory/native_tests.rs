//! Synthetic native capability laws; no personal pile or process environment.
use super::*;
use crate::out::Part;
use crate::storage::{CollectionRoutes, Storage};
use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, CollectionStoreExt};
use triblespace::core::repo::{SnapshotSource, StoreSnapshot};

fn capture(call: impl FnOnce(&mut Out<'_>) -> Result<()>) -> Result<String> {
    let mut text = String::new();
    call(&mut Out::new(&mut |part| {
        let Part::Text { text: part } = part else {
            panic!("expected text")
        };
        text.push_str(&part);
        Ok(())
    }))?;
    Ok(text)
}

#[test]
fn finite_memory_route_preserves_receipt_and_does_not_union_siblings() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let pile = directory.path().join("scratch.pile");
    let key = directory.path().join("scratch.key");
    std::fs::File::create(&pile)?;
    let signer = crate::storage::initialize_signer(&pile, Some(&key))?;
    let storage =
        Storage::shared_local(pile, Some(key)).with_expected_signer(signer.verifying_key());
    let scope = crate::schemas::memory::DEFAULT_SCOPE_ID;
    let (selected, sibling) = storage.with_store(|store, _, _| {
        let name = crate::collection_names::require_name(scope);
        Ok((
            store
                .collection(
                    name,
                    CollectionPolicy::new(
                        AdmissionPolicy::direct(signer.verifying_key()),
                        AdmissionPolicy::direct(signer.verifying_key()),
                    ),
                )?
                .handle(),
            store
                .collection(
                    name,
                    CollectionPolicy::new(
                        AdmissionPolicy::Open,
                        AdmissionPolicy::direct(signer.verifying_key()),
                    ),
                )?
                .handle(),
        ))
    })?;
    let selected = storage
        .clone()
        .with_collections(CollectionRoutes::from([(scope, selected)]));
    let sibling = storage
        .clone()
        .with_collections(CollectionRoutes::from([(scope, sibling)]));
    let native = Memory::resident(
        selected.clone(),
        Arc::new(|_| Ok(())),
        Arc::new(|| Ok(())),
        Arc::new(AtomicBool::new(false)),
    )?;
    assert_eq!(
        native
            .tools()
            .iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        ["memory_show", "memory_create"]
    );
    let range = "2026-09-01T00:00:00..2026-09-02T00:00:00";
    let summary = "literal @/host/private/file\nsecond line";
    let args = serde_json::json!({"summary": summary, "range":range, "tags":["native"]});
    let receipt =
        capture(|out| native.call("memory_create", serde_json::to_vec(&args)?.into(), out))?;
    let operations = super::super::Memory::with_storage(selected.clone());
    let expected =
        operations.create(summary, Some(parse_time_range(range)?), &["native".into()])?;
    assert_eq!(receipt, capture(|out| expected.emit(out))?);
    assert_eq!(receipt.lines().count(), 2);
    let selector = format!("{:x}", expected.id);
    let show = capture(|out| {
        native.call(
            "memory_show",
            serde_json::to_vec(&serde_json::json!({"selector":selector}))?.into(),
            out,
        )
    })?;
    assert_eq!(show, capture(|out| operations.show(&selector, out))?);
    assert!(show.contains(summary));
    assert!(
        capture(|out| super::super::Memory::with_storage(sibling).show(&selector, out)).is_err()
    );
    for name in [
        "memory_meta",
        "memory_context",
        "memory_provenance",
        "wiki_create",
    ] {
        let error = capture(|out| native.call(name, b"{}".as_slice().into(), out)).unwrap_err();
        assert!(error.to_string().contains("provisioned Memory capability"));
    }
    assert!(selected
        .collection_handle(crate::schemas::blockdag::DEFAULT_SCOPE_ID)
        .is_err());
    storage.finish(Ok(()))
}

#[test]
fn missing_route_and_pressure_refuse_before_publication() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let pile = directory.path().join("scratch.pile");
    let key = directory.path().join("scratch.key");
    std::fs::File::create(&pile)?;
    let signer = crate::storage::initialize_signer(&pile, Some(&key))?;
    let storage = Storage::shared_local(pile, Some(key));
    assert!(Memory::resident(
        storage.clone(),
        Arc::new(|_| Ok(())),
        Arc::new(|| Ok(())),
        Arc::new(AtomicBool::new(false))
    )
    .is_err());
    let scope = crate::schemas::memory::DEFAULT_SCOPE_ID;
    let handle = storage.with_store(|store, _, _| {
        Ok(store
            .collection(
                crate::collection_names::require_name(scope),
                CollectionPolicy::new(
                    AdmissionPolicy::direct(signer.verifying_key()),
                    AdmissionPolicy::direct(signer.verifying_key()),
                ),
            )?
            .handle())
    })?;
    let selected = storage
        .clone()
        .with_collections(CollectionRoutes::from([(scope, handle)]));
    let native = Memory::resident(
        selected,
        Arc::new(|_| Ok(())),
        Arc::new(|| anyhow::bail!("synthetic disk pressure")),
        Arc::new(AtomicBool::new(false)),
    )?;
    let before = storage.with_store(|store, _, _| Ok(store.snapshot()?))?;
    let error = capture(|out| {
        native.call(
            "memory_create",
            b"{\"summary\":\"must not publish\"}".as_slice().into(),
            out,
        )
    })
    .unwrap_err();
    assert!(error.to_string().contains("disk pressure"));
    storage.with_store(|store, _, _| {
        assert!(store.snapshot()?.changes_since(&before).is_empty());
        Ok(())
    })?;
    storage.finish(Ok(()))
}
