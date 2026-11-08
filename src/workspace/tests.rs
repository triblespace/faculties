use super::*;
use triblespace::core::repo::{BlobStoreGet, BlobStorePut, SnapshotSource};
use triblespace::macros::id_hex;

type Label = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;

// The app's published definitions, retained only as a fixture oracle. Tests
// publish with these bindings and read through the extracted shared schema.
mod published {
    use triblespace::prelude::*;

    attributes! {
        "C39028980C8DCF8219B07396CCC3B1C3" as managed_pile: inlineencodings::GenId;
        "6029BC6E30BF94DB16A5D4C96FF6B4E8" as enrolled: inlineencodings::Boolean;
        "57DCA7F297BF848DA19CFD2730D6293E" as workspace_route: inlineencodings::GenId;
    }
}

fn binding(
    revision: &ExclusiveId,
    anchor: Id,
    label: Label,
    roots: &[(Id, CollectionHandle)],
) -> Fragment {
    let mut routes = Fragment::empty();
    for (role, root) in roots {
        routes += entity! { config::anchor: *role, config::selects: *root };
    }
    entity! { revision @
        metadata::tag: id_hex!("F4D15E6C2AA3263C1B6DBDD0D5880539"),
        config::anchor: anchor, published::enrolled: true,
        metadata::name: label, published::workspace_route*: routes,
    }
}

fn resolve(facts: &TribleSet, revision: Id, blobs: &mut MemoryRepo) -> Result<Option<Workspace>> {
    let snapshot = blobs.snapshot()?;
    resolve_revision(facts, revision, |label| {
        let text: View<str> = snapshot.get(label)?;
        Ok(text.to_string())
    })
}

#[test]
fn extraction_preserves_published_attribute_and_kind_identities() {
    assert_eq!(
        attrs::WORKSPACE,
        id_hex!("F4D15E6C2AA3263C1B6DBDD0D5880539")
    );
    assert_eq!(attrs::enrolled.id(), published::enrolled.id());
    assert_eq!(attrs::managed_pile.id(), published::managed_pile.id());
    assert_eq!(attrs::workspace_route.id(), published::workspace_route.id());
}

#[test]
fn empty_configuration_and_unknown_revision_never_select_or_read_text() {
    let facts = TribleSet::new();
    assert!(revision_ids(&facts).is_empty());
    assert_eq!(
        resolve_revision(&facts, genid().id, |_| panic!("no label selected")).unwrap(),
        None
    );
}

#[test]
fn opaque_exact_ids_keep_equal_labels_and_unrecognized_roles_separate() {
    let mut blobs = MemoryRepo::default();
    let label = blobs
        .put::<blobencodings::UTF8String, _>("Research")
        .unwrap();
    let first = genid();
    let second = genid();
    let first_anchor = genid().id;
    let second_anchor = genid().id;
    let role = genid().id;
    let first_root = CollectionHandle::new([1; 32]);
    let second_root = CollectionHandle::new([2; 32]);
    let mut facts = binding(&first, first_anchor, label, &[(role, first_root)])
        .facts()
        .clone();
    facts += binding(&second, second_anchor, label, &[(role, second_root)]);
    facts += entity! { &first @ metadata::description: "unrelated annotation" };
    let mut ids = vec![first.id, second.id];
    ids.sort_unstable();
    assert_eq!(revision_ids(&facts), ids);
    assert_eq!(
        resolve(&facts, first.id, &mut blobs).unwrap(),
        Some(Workspace {
            id: first.id,
            anchor: first_anchor,
            label: "Research".into(),
            routes: BTreeMap::from([(role, first_root)]),
        })
    );
    assert_eq!(
        resolve(&facts, second.id, &mut blobs)
            .unwrap()
            .unwrap()
            .routes,
        BTreeMap::from([(role, second_root)])
    );
    assert_eq!(resolve(&facts, genid().id, &mut blobs).unwrap(), None);
}

#[test]
fn repeated_route_witnesses_agree_and_absent_roles_stay_absent() {
    let mut blobs = MemoryRepo::default();
    let label = blobs.put::<blobencodings::UTF8String, _>("Chosen").unwrap();
    let revision = genid();
    let role = genid().id;
    let root = CollectionHandle::new([3; 32]);
    let mut facts = binding(&revision, genid().id, label, &[(role, root)])
        .facts()
        .clone();
    let other_route = entity! { &genid() @ config::anchor: role, config::selects: root };
    facts += entity! { &revision @ published::workspace_route*: other_route };
    assert_eq!(
        resolve(&facts, revision.id, &mut blobs)
            .unwrap()
            .unwrap()
            .routes,
        BTreeMap::from([(role, root)])
    );

    let empty = genid();
    facts += binding(&empty, genid().id, label, &[]);
    assert!(resolve(&facts, empty.id, &mut blobs)
        .unwrap()
        .unwrap()
        .routes
        .is_empty());
}

#[test]
fn superseded_and_withdrawn_revisions_never_retarget() {
    let mut blobs = MemoryRepo::default();
    let label = blobs.put::<blobencodings::UTF8String, _>("Chosen").unwrap();
    let anchor = genid().id;
    let old = genid();
    let next = genid();
    let mut facts = binding(&old, anchor, label, &[]).facts().clone();
    facts += binding(&next, anchor, label, &[]);
    facts += entity! { &next @ metadata::supersedes: old.id };
    assert_eq!(revision_ids(&facts), [next.id]);
    assert_eq!(resolve(&facts, old.id, &mut blobs).unwrap(), None);
    assert_eq!(
        resolve(&facts, next.id, &mut blobs).unwrap().unwrap().id,
        next.id
    );

    facts += entity! { metadata::tag: attrs::WORKSPACE, config::anchor: anchor,
    published::enrolled: false, metadata::supersedes: next.id };
    assert!(revision_ids(&facts).is_empty());
    assert_eq!(resolve(&facts, next.id, &mut blobs).unwrap(), None);
}

#[test]
fn concurrent_revisions_remain_discoverable_but_neither_is_selected() {
    let mut blobs = MemoryRepo::default();
    let label = blobs.put::<blobencodings::UTF8String, _>("Chosen").unwrap();
    let anchor = genid().id;
    let first = genid();
    let second = genid();
    let mut facts = binding(&first, anchor, label, &[]).facts().clone();
    facts += binding(&second, anchor, label, &[]);
    assert_eq!(revision_ids(&facts).len(), 2);
    for id in [first.id, second.id] {
        let error =
            resolve_revision(&facts, id, |_| panic!("conflict precedes label read")).unwrap_err();
        assert!(error.to_string().contains("concurrent revisions"));
    }
}

#[test]
fn conflicting_role_roots_are_not_overwritten_or_filled_from_defaults() {
    let mut blobs = MemoryRepo::default();
    let label = blobs.put::<blobencodings::UTF8String, _>("Chosen").unwrap();
    let revision = genid();
    let role = genid().id;
    let facts = binding(
        &revision,
        genid().id,
        label,
        &[
            (role, CollectionHandle::new([4; 32])),
            (role, CollectionHandle::new([5; 32])),
        ],
    )
    .facts()
    .clone();
    let error = resolve_revision(&facts, revision.id, |_| {
        panic!("conflict precedes label read")
    })
    .unwrap_err();
    assert!(error.to_string().contains("conflicting exact roots"));
}

#[test]
fn incomplete_and_legacy_bindings_remain_unavailable_without_fallback() {
    let incomplete = genid();
    let mut facts = entity! { &incomplete @ metadata::tag: attrs::WORKSPACE,
    published::enrolled: true }
    .facts()
    .clone();
    assert_eq!(revision_ids(&facts), [incomplete.id]);
    assert!(
        resolve_revision(&facts, incomplete.id, |_| panic!("no label"))
            .unwrap_err()
            .to_string()
            .contains("no unambiguous understood binding")
    );
    facts += entity! { &incomplete @ published::managed_pile: genid().id };
    assert!(
        resolve_revision(&facts, incomplete.id, |_| panic!("legacy binding"))
            .unwrap_err()
            .to_string()
            .contains("explicit single-store migration")
    );
}

#[test]
fn contradictory_enrollment_and_ambiguous_labels_preserve_app_refusals() {
    let mut blobs = MemoryRepo::default();
    let label = blobs.put::<blobencodings::UTF8String, _>("First").unwrap();
    let other_label = blobs.put::<blobencodings::UTF8String, _>("Second").unwrap();
    let revision = genid();
    let original = binding(&revision, genid().id, label, &[]).facts().clone();
    let mut facts = original.clone();
    facts += entity! { &revision @ published::enrolled: false };
    assert!(resolve(&facts, revision.id, &mut blobs)
        .unwrap_err()
        .to_string()
        .contains("concurrent revisions"));
    let mut facts = original;
    facts += entity! { &revision @ metadata::name: other_label };
    assert!(resolve(&facts, revision.id, &mut blobs)
        .unwrap_err()
        .to_string()
        .contains("no unambiguous understood binding"));
}

#[test]
fn missing_label_is_delivered_to_the_callers_dependency_tracker() {
    let mut blobs = MemoryRepo::default();
    let label = blobs
        .put::<blobencodings::UTF8String, _>("Arrived later")
        .unwrap();
    let revision = genid();
    let facts = binding(&revision, genid().id, label, &[]).facts().clone();
    let mut requested = Vec::new();
    let error = resolve_revision(&facts, revision.id, |name| {
        requested.push(name);
        bail!("label unavailable in this observation")
    })
    .unwrap_err();
    assert_eq!(requested, [label]);
    assert_eq!(error.to_string(), "label unavailable in this observation");
    assert_eq!(
        resolve(&facts, revision.id, &mut blobs)
            .unwrap()
            .unwrap()
            .label,
        "Arrived later"
    );
}
