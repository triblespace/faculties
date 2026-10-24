use super::*;
use crate::encrypted::{ItemKey, ItemSealer};
use crate::resource::{self, DeliveryLimits, SecretTarget};
use crate::storage::{self, SecretsCollection};
use triblespace::core::blob::{Bytes, MemoryBlobStoreSnapshot};
use triblespace::core::capability::CapabilityResource;
use triblespace::core::collection::{
    grant_collection_write, AdmissionPolicy, CollectionPolicy, CollectionStoreExt,
};
use triblespace::core::repo::memoryrepo::MemoryRepo;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{
    BlobChildren, BlobStoreList, BlobStorePut, CapabilityProofRead, SnapshotSource,
};

const PRIVATE: &str = "SYNTHETIC-PRIVATE-password";
const USER: &str = "SYNTHETIC-PRIVATE-username";
const FILENAME: &str = "SYNTHETIC-PRIVATE-filename.bin";
const BINARY: &[u8] = b"SYNTHETIC-PRIVATE-binary\0\xff\r\n";

fn at(second: i64) -> IntervalValue {
    let epoch = Epoch::from_unix_seconds(second as f64);
    (epoch, epoch).try_to_inline().unwrap()
}

fn policy(signer: &SigningKey) -> CollectionPolicy {
    CollectionPolicy::new(
        AdmissionPolicy::Open,
        AdmissionPolicy::direct(signer.verifying_key()),
    )
}

fn fixture() -> (MemoryRepo, SecretsCollection, SigningKey) {
    let signer = SigningKey::from_bytes(&[51; 32]);
    let mut store = MemoryRepo::for_host(signer.verifying_key());
    let collection =
        SecretsCollection::register(&mut store, "structured synthetic fixture", policy(&signer))
            .unwrap();
    (store, collection, signer)
}

fn password_input() -> [FieldInput<'static>; 2] {
    [
        FieldInput {
            role: ROLE_USERNAME,
            label: None,
            value: FieldValue::Text(USER),
        },
        FieldInput {
            role: ROLE_PASSWORD,
            label: None,
            value: FieldValue::Text(PRIVATE),
        },
    ]
}

fn add(store: &mut MemoryRepo, collection: SecretsCollection, signer: &SigningKey) -> Id {
    storage::add_item(
        store,
        signer,
        collection,
        "explicit public label",
        ItemInput {
            fields: &password_input(),
            attachments: &[],
        },
        at(0),
    )
    .unwrap()
}

fn selected_text<R: BlobStoreGet>(item: &OpenedItem<R>, role: Id) -> PrivateText {
    find!(value: PrivateText, pattern!(item.facts(), [
        { metadata::tag: KIND_ITEM, item_field: _?field },
        { _?field @ field_role: role, field_text: ?value },
    ]))
    .next()
    .unwrap()
}

#[test]
fn carried_structured_version_uses_its_resource_policy_not_a_home_collection() {
    use crate::resource::retired::resource_collection;
    use triblespace::core::capability::policy::resource_handle;
    let (mut store, source, alice) = fixture();
    let bob = SigningKey::from_bytes(&[57; 32]);
    let target =
        SecretsCollection::register(&mut store, "carried structured fixture", policy(&alice))
            .unwrap();
    let sealed = prepare(
        &alice,
        "explicit public label",
        ItemInput {
            fields: &password_input(),
            attachments: &[],
        },
        at(0),
    )
    .unwrap();
    let id = sealed.secret;
    let resource = find!(resource: CollectionHandle,
        pattern!(sealed.fragment.facts(), [{ id @ resource_handle: ?resource }]))
        .next()
        .unwrap();
    store
        .commit(source.source(), &alice, sealed.fragment.clone())
        .unwrap();
    let unrelated = storage::snapshot(store.snapshot().unwrap(), target).unwrap();
    assert!(unrelated.open_item(id, &alice).is_err());
    store
        .commit(target.source(), &alice, sealed.fragment)
        .unwrap();
    let carried =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, target, &alice)).unwrap();
    let descriptor: TribleSet = carried.store_snapshot().get(resource).unwrap();
    assert!(!exists!(
        (home: CollectionHandle),
        pattern!(&descriptor, [{ resource_collection: ?home }])
    ));
    assert_eq!(carried.open_item(id, &alice).unwrap().len(), 1);
    resource::grant(
        &mut store,
        &alice,
        &carried,
        SecretTarget::Secret(id),
        bob.verifying_key(),
        DeliveryLimits::default(),
        false,
    )
    .unwrap();
    let granted =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, target, &alice)).unwrap();
    assert_eq!(
        storage::maintain_selected_recipient_envelopes(
            &mut store,
            &alice,
            &granted,
            target,
            &alice,
            &[SecretTarget::Secret(id)],
            Epoch::from_unix_seconds(1.0),
        )
        .unwrap(),
        1
    );
    let delivered =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, target, &alice)).unwrap();
    let opened = delivered.open_item(id, &bob).unwrap().remove(0);
    let password = selected_text(&opened, ROLE_PASSWORD);
    assert_eq!(opened.open_child(password).unwrap().bytes.as_ref(), PRIVATE.as_bytes());
    let original = storage::snapshot(store.snapshot().unwrap(), source).unwrap();
    assert!(original.open_item(id, &bob).is_err());
}

#[test]
fn totp_binds_exact_field_child_and_tolerates_open_world_relations() {
    let (mut store, collection, signer) = fixture();
    let key = ItemKey::generate(genid().id);
    let id = key.id();
    let mut sealer = ItemSealer::new(&key);
    let first = sealer.seal_blob::<blobencodings::UTF8String>(
        "otpauth://totp/Example:alice?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=Example&digits=8".to_owned().to_blob(), &[]).unwrap();
    let second = sealer
        .seal_blob::<blobencodings::UTF8String>("MY".to_owned().to_blob(), &[])
        .unwrap();
    let label = sealer
        .seal_blob::<blobencodings::UTF8String>("MY======".to_owned().to_blob(), &[])
        .unwrap();
    let detached = sealer
        .seal_blob::<blobencodings::UTF8String>("MZXQ".to_owned().to_blob(), &[])
        .unwrap();
    let field = genid();
    let root = genid();
    let unknown_role = genid().id;
    let mut private = entity! { &field @ field_role*: [ROLE_CUSTOM, unknown_role],
    field_text*: [first, second], private_name: label };
    private += entity! { &root @ metadata::tag*: [KIND_ITEM, unknown_role], item_field: &field };
    // An unrelated text relation and same-key blob are not item-field values.
    private += entity! { field_text: detached, metadata::tag: unknown_role };
    let bundle = sealer.finish_facts(private.into_facts()).unwrap();
    let body = bundle.root();
    let sealed = resource::seal_item_bundle(
        &signer,
        "synthetic TOTP",
        key,
        bundle,
        at(0),
    )
    .unwrap();
    let public = sealed.fragment.facts().clone();
    store
        .commit(collection.source(), &signer, sealed.fragment)
        .unwrap();
    let reader = store.snapshot().unwrap();
    let opened = open_exact(reader.clone(), &public, id, body, &signer).unwrap();
    assert_eq!(opened.totp_at(first, 59).unwrap().code.as_str(), "94287082");
    assert_eq!(
        opened.totp_at(second, 59).unwrap().code.as_str(),
        crate::totp::generate("MY", Some(59)).unwrap().code.as_str()
    );
    for child in [label, detached, Inline::new([0; 32])] {
        let error = opened
            .totp_at(child, 59)
            .err()
            .expect("non-field child accepted");
        assert_eq!(
            error.to_string(),
            "selected TOTP child is not an item text field"
        );
    }
    let missing = open_exact(without(&reader, first.raw), &public, id, body, &signer).unwrap();
    assert!(missing.totp_at(first, 59).is_err());
    assert!(missing.totp_at(second, 59).is_ok());
    assert!(open_exact(reader.clone(), &public, genid().id, body, &signer).is_err());
    assert!(open_exact(reader.clone(), &public, id, Inline::new([0; 32]), &signer).is_err());
    assert!(open_exact(reader, &public, id, body, &SigningKey::from_bytes(&[9; 32])).is_err());
}

fn assert_no_private<R: BlobStoreGet + BlobStoreList>(reader: &R) {
    for info in reader.blobs() {
        let blob: Bytes = reader.get(info.unwrap().handle).unwrap();
        assert!(!blob
            .windows(b"SYNTHETIC-PRIVATE-".len())
            .any(|window| window == b"SYNTHETIC-PRIVATE-"));
    }
}

#[test]
fn native_fields_binary_and_private_names_survive_commit_reopen_and_blind_traversal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic.pile");
    std::fs::File::create(&path).unwrap();
    let signer = SigningKey::from_bytes(&[52; 32]);
    let mut store = Pile::open_as(&path, signer.verifying_key()).unwrap();
    let collection =
        SecretsCollection::register(&mut store, "structured reopen fixture", policy(&signer))
            .unwrap();
    let fields = [
        FieldInput {
            role: ROLE_USERNAME,
            label: None,
            value: FieldValue::Text(USER),
        },
        FieldInput {
            role: ROLE_PASSWORD,
            label: None,
            value: FieldValue::Text(PRIVATE),
        },
        FieldInput {
            role: ROLE_URL,
            label: None,
            value: FieldValue::Text("SYNTHETIC-PRIVATE-service.example"),
        },
        FieldInput {
            role: ROLE_NOTES,
            label: None,
            value: FieldValue::Text("SYNTHETIC-PRIVATE-notes"),
        },
        FieldInput {
            role: ROLE_CUSTOM,
            label: Some("SYNTHETIC-PRIVATE-custom-label"),
            value: FieldValue::Text("SYNTHETIC-PRIVATE-custom-value"),
        },
        FieldInput {
            role: ROLE_CUSTOM,
            label: None,
            value: FieldValue::Integer(-42),
        },
        FieldInput {
            role: ROLE_CUSTOM,
            label: None,
            value: FieldValue::Flag(true),
        },
    ];
    let sealed = prepare(
        collection.handle(),
        &signer,
        "public fixture",
        ItemInput {
            fields: &fields,
            attachments: &[AttachmentInput {
                filename: FILENAME,
                bytes: BINARY,
            }],
        },
        at(0),
    )
    .unwrap();
    let id = sealed.secret;
    let public = sealed.fragment.facts().clone();
    let body = find!(body: ItemBody, pattern!(&public, [{ id @ structured_body: ?body }]))
        .next()
        .unwrap();
    assert!(!exists!(
        pattern!(sealed.fragment.metafacts(), [{ metadata::attribute: field_text.id() }])
    ));
    let receipt = store
        .commit(collection.source(), &signer, sealed.fragment)
        .unwrap();
    assert!(!format!("{receipt:?}").contains("SYNTHETIC-PRIVATE-"));
    store.close().unwrap();

    let mut reopened = Pile::open_as(&path, signer.verifying_key()).unwrap();
    let reader = reopened.snapshot().unwrap();
    assert_no_private(&reader);
    // Use the real schema-blind aligned-word walker, not Bundle carriage or an
    // encryption-specific reference parser. Every declared child must be found.
    let encrypted: Blob<Encrypted<SimpleArchive>> = reader.get(body).unwrap();
    let count = u64::from_le_bytes(encrypted.bytes[56..64].try_into().unwrap()) as usize;
    assert_eq!(count, 8);
    let direct = reader.children(body.transmute());
    let reachable =
        triblespace::core::repo::reachable(&reader, [body.transmute()]).collect::<BTreeSet<_>>();
    for word in encrypted.bytes[64..64 + 32 * count].chunks_exact(32) {
        let child = Inline::<inlineencodings::Handle<blobencodings::UnknownBlob>>::new(
            word.try_into().unwrap(),
        );
        assert!(direct.contains(&child));
        assert!(reachable.contains(&child));
        assert!(reader.get::<Bytes, _>(child).is_ok());
    }
    let item = open_exact(reader, &public, id, body, &signer).unwrap();
    assert_eq!(
        item.open_child(selected_text(&item, ROLE_PASSWORD))
            .unwrap()
            .bytes
            .as_ref(),
        PRIVATE.as_bytes()
    );
    assert_eq!(
        item.open_child(selected_text(&item, ROLE_USERNAME))
            .unwrap()
            .bytes
            .as_ref(),
        USER.as_bytes()
    );
    let custom = find!((value: PrivateText, label: PrivateText), pattern!(item.facts(), [
        { metadata::tag: KIND_ITEM, item_field: _?field },
        { _?field @ field_role: ROLE_CUSTOM, field_text: ?value, private_name: ?label },
    ]))
    .next()
    .unwrap();
    assert_eq!(
        item.open_child(custom.0).unwrap().bytes.as_ref(),
        b"SYNTHETIC-PRIVATE-custom-value"
    );
    assert_eq!(
        item.open_child(custom.1).unwrap().bytes.as_ref(),
        b"SYNTHETIC-PRIVATE-custom-label"
    );
    let (name, bytes) = find!((name: PrivateText,
        bytes: Inline<inlineencodings::Handle<Encrypted<blobencodings::RawBytes>>>),
        pattern!(item.facts(), [{ metadata::tag: KIND_ITEM, item_attachment: _?attachment },
            { _?attachment @ private_name: ?name, attachment_bytes: ?bytes }]))
    .next()
    .unwrap();
    assert_eq!(
        item.open_child(name).unwrap().bytes.as_ref(),
        FILENAME.as_bytes()
    );
    assert_eq!(item.open_child(bytes).unwrap().bytes.as_ref(), BINARY);
    let number = find!(number: Inline<inlineencodings::I256BE>, pattern!(item.facts(), [{field_integer: ?number}])).next().unwrap();
    assert_eq!(number.try_from_inline::<i128>().unwrap(), -42);
    let flag = find!(flag: Inline<inlineencodings::Boolean>, pattern!(item.facts(), [{field_flag: ?flag}])).next().unwrap();
    assert!(flag.try_from_inline::<bool>().unwrap());
    drop(item);
    reopened.close().unwrap();
}

fn without<R: BlobStoreGet + BlobStoreList>(
    reader: &R,
    missing: [u8; 32],
) -> MemoryBlobStoreSnapshot {
    let mut blobs = MemoryBlobStore::new();
    for info in reader.blobs() {
        let handle = info.unwrap().handle;
        if handle.raw != missing {
            blobs.insert(
                reader
                    .get::<Blob<blobencodings::UnknownBlob>, _>(handle)
                    .unwrap(),
            );
        }
    }
    blobs.snapshot().unwrap()
}

#[test]
fn missing_selected_child_fails_locally_and_unknown_repeated_opaque_facts_remain_valid() {
    let (mut store, collection, signer) = fixture();
    let key = ItemKey::generate(genid().id);
    let id = key.id();
    let mut sealer = ItemSealer::new(&key);
    let first = sealer
        .seal_blob::<blobencodings::UTF8String>(USER.to_owned().to_blob(), &[])
        .unwrap();
    let second = sealer
        .seal_blob::<blobencodings::UTF8String>(PRIVATE.to_owned().to_blob(), &[])
        .unwrap();
    let root = genid().id;
    let field = genid().id;
    let unknown = genid().id;
    let mut private = entity! { ExclusiveId::force_ref(&root) @ metadata::tag*: [KIND_ITEM, unknown], item_field: field };
    private += entity! { ExclusiveId::force_ref(&field) @ field_role*: [ROLE_PASSWORD, unknown],
    field_text*: [first, second], metadata::tag: unknown };
    let bundle = sealer.finish_facts(private.into_facts()).unwrap();
    let body = bundle.root();
    let sealed = resource::seal_item_bundle(
        &signer,
        "public opaque",
        key,
        bundle,
        at(0),
    )
    .unwrap();
    let public = sealed.fragment.facts().clone();
    store
        .commit(collection.source(), &signer, sealed.fragment)
        .unwrap();
    let resident = store.snapshot().unwrap();
    let filtered = without(&resident, first.raw);
    let opened = open_exact(filtered, &public, id, body, &signer).unwrap();
    assert!(exists!(
        pattern!(opened.facts(), [{ root @ metadata::tag: KIND_ITEM, item_field: field }])
    ));
    assert_eq!(
        find!(value: PrivateText, pattern!(opened.facts(), [{field @ field_text: ?value}])).count(),
        2
    );
    assert!(exists!(
        pattern!(opened.facts(), [{ field @ field_role: unknown }])
    ));
    assert_eq!(
        opened.open_child(second).unwrap().bytes.as_ref(),
        PRIVATE.as_bytes()
    );
    let error = opened.open_child(first).unwrap_err();
    assert!(!format!("{error:#}").contains("SYNTHETIC-PRIVATE-"));
    assert!(open_exact(
        resident.clone(),
        &public,
        id,
        body,
        &SigningKey::from_bytes(&[9; 32])
    )
    .is_err());
    assert_no_private(&resident);
}

#[test]
fn selected_grants_deadlines_no_delegation_and_cross_version_wraps_preserve_scope() {
    let (mut store, collection, alice) = fixture();
    let bob = SigningKey::from_bytes(&[53; 32]);
    let carol = SigningKey::from_bytes(&[54; 32]);
    let first = add(&mut store, collection, &alice);
    let second = add(&mut store, collection, &alice);
    let before =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    assert!(before.open_item(first, &bob).is_err());
    let before =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    resource::grant(
        &mut store,
        &alice,
        &before,
        SecretTarget::Secret(first),
        bob.verifying_key(),
        DeliveryLimits {
            not_before: Some(Epoch::from_unix_seconds(10.0)),
            expires_at: Some(Epoch::from_unix_seconds(20.0)),
        },
        false,
    )
    .unwrap();
    for (selection, now, expected) in [
        (first, 9.0, 0),
        (second, 12.0, 0),
        (first, 10.0, 1),
        (first, 20.0, 0),
    ] {
        let snapshot =
            pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice))
                .unwrap();
        assert_eq!(
            storage::maintain_selected_recipient_envelopes(
                &mut store,
                &alice,
                &snapshot,
                collection,
                &alice,
                &[SecretTarget::Secret(selection)],
                Epoch::from_unix_seconds(now)
            )
            .unwrap(),
            expected
        );
    }
    let snapshot =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    let public = snapshot.facts().unwrap().clone();
    let reader = snapshot.store_snapshot().clone();
    let opened = open(reader.clone(), &public, first, &bob).unwrap();
    assert_eq!(opened.len(), 1);
    assert_eq!(
        opened[0]
            .open_child(selected_text(&opened[0], ROLE_PASSWORD))
            .unwrap()
            .bytes
            .as_ref(),
        PRIVATE.as_bytes()
    );
    assert!(open(reader, &public, second, &bob).is_err());
    assert!(resource::grant(
        &mut store,
        &bob,
        &snapshot,
        SecretTarget::Secret(first),
        carol.verifying_key(),
        DeliveryLimits::default(),
        false
    )
    .is_err());
    // Copy a genuine delivered wrap into a different version's relation. Its
    // sealed S and authenticated binding must prevent cross-version recovery.
    let wrap = recipient_wraps(&public, first, bob.verifying_key().to_bytes())[0];
    let attack = wrap_record(
        genid().id,
        second,
        bob.verifying_key().to_bytes(),
        wrap.sealed_dek,
    );
    store.commit(collection.source(), &alice, attack).unwrap();
    let after =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    assert!(after.open_item(second, &bob).is_err());
}

#[test]
fn valid_competing_bodies_are_disclosed_and_missing_or_foreign_resources_do_not_poison_grants() {
    let (mut store, collection, alice) = fixture();
    let bob = SigningKey::from_bytes(&[55; 32]);
    let first = add(&mut store, collection, &alice);
    let second = add(&mut store, collection, &alice);
    let before =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    let genuine = super::super::envelope::recover_item(
        before.store_snapshot(),
        before.facts().unwrap(),
        first,
        &alice,
    )
    .unwrap()
    .remove(0);
    let foreign = super::super::envelope::recover_item(
        before.store_snapshot(),
        before.facts().unwrap(),
        second,
        &alice,
    )
    .unwrap()
    .remove(0)
    .resource;
    let genuine_body: ItemBody = genuine.body.transmute();
    // An admitted writer can create a genuinely decryptable alternative S/H/R,
    // not merely corrupt old ciphertext. It must not silently replace H1.
    let attacker = SigningKey::from_bytes(&[56; 32]);
    grant_collection_write(
        &mut store,
        collection.handle(),
        &alice,
        attacker.verifying_key(),
    )
    .unwrap();
    let attacker_dek = Key::gen();
    let competing_key = ItemKey::from_dek(first, attacker_dek.clone());
    let sealer = ItemSealer::new(&competing_key);
    let bundle = sealer
        .finish_facts(entity! { metadata::tag: KIND_ITEM }.into_facts())
        .unwrap();
    let competing_body = bundle.root();
    let mut competing = resource::seal_item_bundle(
        &attacker,
        "public competing",
        competing_key,
        bundle,
        at(0),
    )
    .unwrap();
    let competing_resource = find!(resource: CollectionHandle, pattern!(competing.fragment.facts(),
        [{ first @ triblespace::core::capability::policy::resource_handle: ?resource }]))
    .next()
    .unwrap();
    competing.fragment += super::super::envelope::fragment(
        &super::super::envelope::BoundKey {
            secret: first,
            body: competing_body.transmute(),
            resource: competing_resource,
            dek: attacker_dek,
        },
        alice.verifying_key().to_bytes(),
    )
    .unwrap();
    store
        .commit(collection.source(), &attacker, competing.fragment)
        .unwrap();
    // A valid same-S/body wrap naming missing R must remain nonmatching evidence
    // for Secret(S), while explicit Resource(R) reports the missing descriptor.
    let missing = CollectionHandle::new([199; 32]);
    let mut attack = super::super::envelope::fragment(
        &super::super::envelope::BoundKey {
            secret: first,
            body: genuine.body,
            resource: missing,
            dek: genuine.dek.clone(),
        },
        alice.verifying_key().to_bytes(),
    )
    .unwrap();
    // A retired collection annotation is inert. Exact S/H and R's own policy,
    // not an historical location, determine the resource's delivery authority.
    use crate::resource::retired::resource_collection;
    use triblespace::core::capability::policy::resource_policy;
    let capability = store
        .put::<SimpleArchive, _>(key_delivery_definition().facts().clone())
        .unwrap();
    let wrong_source = entity! { metadata::tag: KIND_SECRET_RESOURCE, wrap_secret: first,
    structured_body: genuine_body, resource_collection: CollectionHandle::new([198; 32]),
    resource_policy*: AdmissionPolicy::direct(alice.verifying_key()).binding(capability) };
    let wrong_source = store
        .put::<SimpleArchive, _>(wrong_source.into_facts())
        .unwrap();
    attack += super::super::envelope::fragment(
        &super::super::envelope::BoundKey {
            secret: first,
            body: genuine.body,
            resource: wrong_source,
            dek: genuine.dek.clone(),
        },
        alice.verifying_key().to_bytes(),
    )
    .unwrap();
    attack += super::super::envelope::fragment(
        &super::super::envelope::BoundKey {
            secret: first,
            body: genuine.body,
            resource: foreign,
            dek: genuine.dek.clone(),
        },
        alice.verifying_key().to_bytes(),
    )
    .unwrap();
    attack += entity! { ExclusiveId::force_ref(&first) @ triblespace::core::capability::policy::resource_handle: foreign };
    // A well-shaped envelope to Alice advertised as Bob's is not delivery proof.
    let original_wrap = recipient_wraps(
        before.facts().unwrap(),
        first,
        alice.verifying_key().to_bytes(),
    )[0];
    attack += wrap_record(
        genid().id,
        first,
        bob.verifying_key().to_bytes(),
        original_wrap.sealed_dek,
    );
    store.commit(collection.source(), &alice, attack).unwrap();
    let after =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    let alternatives = open(
        after.store_snapshot().clone(),
        after.facts().unwrap(),
        first,
        &alice,
    )
    .unwrap();
    assert_eq!(alternatives.len(), 2);
    assert!(resource::holder_has_binding(
        after.store_snapshot(),
        after.facts().unwrap(),
        first,
        genuine.body,
        genuine.resource,
        &alice
    )
    .unwrap());
    assert!(
        !resource::holder_has_binding(
            after.store_snapshot(),
            after.facts().unwrap(),
            first,
            genuine.body,
            competing_resource,
            &alice
        )
        .unwrap(),
        "a competing body/resource cannot authenticate the original body tuple"
    );
    assert!(!resource::holder_has_binding(
        after.store_snapshot(),
        after.facts().unwrap(),
        first,
        genuine.body,
        genuine.resource,
        &bob
    )
    .unwrap());
    assert_eq!(
        alternatives
            .iter()
            .map(|item| item.body())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([genuine_body, competing_body])
    );
    assert_eq!(
        open_exact(
            after.store_snapshot().clone(),
            after.facts().unwrap(),
            first,
            genuine_body,
            &alice
        )
        .unwrap()
        .facts(),
        alternatives
            .iter()
            .find(|item| item.body() == genuine_body)
            .unwrap()
            .facts()
    );
    assert!(resource::grant(
        &mut store,
        &alice,
        &after,
        SecretTarget::Resource(missing),
        bob.verifying_key(),
        DeliveryLimits::default(),
        false
    )
    .is_err());
    let issued = resource::grant(
        &mut store,
        &alice,
        &after,
        SecretTarget::Resource(genuine.resource),
        bob.verifying_key(),
        DeliveryLimits::default(),
        false,
    )
    .unwrap();
    assert_eq!(
        issued.len(),
        1,
        "only the matching resource Alice controls, not attacker/missing/foreign R"
    );
    let snapshot = store.snapshot().unwrap();
    for proof in snapshot.proofs().unwrap().map(Result::unwrap) {
        if issued.contains(&proof.id()) {
            assert_ne!(proof.resource(), CapabilityResource::from(foreign));
        }
    }
    let current =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    assert_eq!(
        storage::maintain_selected_recipient_envelopes(
            &mut store,
            &alice,
            &current,
            collection,
            &alice,
            &[SecretTarget::Resource(genuine.resource)],
            Epoch::from_unix_seconds(1.0)
        )
        .unwrap(),
        1
    );
    let delivered =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    assert_eq!(delivered.open_item(first, &bob).unwrap().len(), 1);
    let retired_home_grants = resource::grant(
        &mut store,
        &alice,
        &delivered,
        SecretTarget::Resource(wrong_source),
        bob.verifying_key(),
        DeliveryLimits::default(),
        false,
    )
    .unwrap();
    assert_eq!(retired_home_grants.len(), 1);
}
