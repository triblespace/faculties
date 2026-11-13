//! The colony vault: default sealing reaches every READ root, the descriptor
//! converges across hosts, adoption keeps version ids, and protected versions
//! and other vaults are never touched.

use super::*;
use crate::item::{FieldInput, FieldValue, ItemInput};
use crate::protected::{self, Protection};
use crate::record::{self, Delivery};
use crate::resource::{self, DeliveryLimits, SecretTarget};
use crate::schema::ROLE_PASSWORD;
use crate::{envelope, wrap_recipients, IntervalValue};
use ed25519_dalek::VerifyingKey;
use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, CollectionStoreExt};
use triblespace::core::repo::memoryrepo::{MemoryRepo, MemoryRepoSnapshot};
use triblespace::core::repo::{CapabilityProofRead, SnapshotSource};
use triblespace::prelude::*;

const RAW: &[u8] = b"SYNTHETIC-PRIVATE-raw-token";
const PASSWORD: &str = "SYNTHETIC-PRIVATE-item-password";

fn at(second: i64) -> IntervalValue {
    let epoch = Epoch::from_unix_seconds(second as f64);
    (epoch, epoch).try_to_inline().unwrap()
}

fn now() -> Epoch {
    Epoch::from_unix_seconds(100.0)
}

struct Colony {
    store: MemoryRepo,
    collection: SecretsCollection,
    a: SigningKey,
    b: SigningKey,
    c: SigningKey,
    outsider: SigningKey,
}

/// A Secrets collection whose READ policy is 1-of-3 over hosts A, B and C,
/// as on sky, written by A.
fn colony() -> Colony {
    let a = SigningKey::from_bytes(&[101; 32]);
    let b = SigningKey::from_bytes(&[102; 32]);
    let c = SigningKey::from_bytes(&[103; 32]);
    let outsider = SigningKey::from_bytes(&[104; 32]);
    let mut store = MemoryRepo::for_host(a.verifying_key());
    let collection = SecretsCollection::register(
        &mut store,
        "colony fixture",
        read_policy(
            AdmissionPolicy::quorum(
                [a.verifying_key(), b.verifying_key(), c.verifying_key()],
                1,
                None,
            )
            .unwrap(),
            a.verifying_key(),
        ),
    )
    .unwrap();
    Colony {
        store,
        collection,
        a,
        b,
        c,
        outsider,
    }
}

fn read_policy(read: AdmissionPolicy, writer: VerifyingKey) -> CollectionPolicy {
    CollectionPolicy::new(read, AdmissionPolicy::direct(writer))
}

fn observe(
    collection: SecretsCollection,
) -> impl FnMut(&mut MemoryRepo) -> Result<SecretsSnapshot<MemoryRepoSnapshot>> {
    move |store| snapshot(store.snapshot().unwrap(), collection)
}

fn view(colony: &mut Colony) -> SecretsSnapshot<MemoryRepoSnapshot> {
    snapshot(colony.store.snapshot().unwrap(), colony.collection).unwrap()
}

fn login() -> [FieldInput<'static>; 1] {
    [FieldInput {
        role: ROLE_PASSWORD,
        label: None,
        value: FieldValue::Text(PASSWORD),
    }]
}

fn item_password(
    view: &SecretsSnapshot<MemoryRepoSnapshot>,
    id: Id,
    holder: &SigningKey,
) -> Result<String> {
    let reader = view.store_snapshot().clone();
    let items = crate::item::open(reader, view.facts().unwrap(), id, holder)?;
    let child = find!(value: crate::item::PrivateText, pattern!(items[0].facts(), [
        { crate::schema::field_role: ROLE_PASSWORD, crate::schema::field_text: ?value }
    ]))
    .next()
    .ok_or_else(|| anyhow!("no password field"))?;
    Ok(String::from_utf8(
        items[0].open_child(child)?.bytes.to_vec(),
    )?)
}

fn proof_count(view: &SecretsSnapshot<MemoryRepoSnapshot>) -> usize {
    view.store_snapshot().proofs().unwrap().count()
}

fn add_raw(colony: &mut Colony, name: &str) -> Result<ColonyAdd> {
    let collection = colony.collection;
    add_secret_to_colony(
        &mut colony.store,
        &colony.a,
        collection,
        observe(collection),
        name,
        RAW,
        at(1),
        now(),
    )
}

fn add_login(colony: &mut Colony) -> Result<ColonyAdd> {
    let collection = colony.collection;
    let fields = login();
    add_item_to_colony(
        &mut colony.store,
        &colony.a,
        collection,
        observe(collection),
        "synthetic login",
        ItemInput {
            fields: &fields,
            attachments: &[],
        },
        at(2),
        now(),
    )
}

fn adopt(colony: &mut Colony, commit: bool) -> Result<ColonyAdoption> {
    let collection = colony.collection;
    adopt_into_colony(
        &mut colony.store,
        &colony.a,
        collection,
        observe(collection),
        now(),
        commit,
    )
}

fn protected_secret(colony: &mut Colony) -> Id {
    let sealed = protected::prepare_raw(
        "synthetic protected",
        b"SYNTHETIC-PRIVATE-protected",
        at(3),
        Protection {
            password: b"synthetic passphrase",
            device: None,
        },
    )
    .unwrap();
    let id = sealed.secret;
    colony
        .store
        .commit(colony.collection.source(), &colony.a, sealed.fragment)
        .unwrap();
    id
}

#[test]
fn colony_sealing_reaches_every_read_root_without_a_grant() {
    let mut colony = colony();
    let protected = protected_secret(&mut colony);
    let before = view(&mut colony);
    let protected_facts = wrap_recipients(before.facts().unwrap(), protected);
    drop(before);

    let raw = add_raw(&mut colony, "synthetic raw").unwrap();
    let item = add_login(&mut colony).unwrap();
    assert_eq!(raw.vault, item.vault, "one colony vault for both");
    assert_eq!(raw.delivered, 2, "B and C, A already holds its envelope");
    assert_eq!(item.delivered, 2);

    let view = view(&mut colony);
    assert_eq!(proof_count(&view), 0, "no grant was issued");
    for host in [&colony.a, &colony.b, &colony.c] {
        assert_eq!(view.open(raw.secret, host).unwrap(), RAW);
        assert_eq!(item_password(&view, item.secret, host).unwrap(), PASSWORD);
    }
    assert!(view.open(raw.secret, &colony.outsider).is_err());
    assert!(item_password(&view, item.secret, &colony.outsider).is_err());
    assert!(resource::is_vault(view.store_snapshot(), raw.vault));
    // Default sealing never touches a protected version.
    assert_eq!(
        wrap_recipients(view.facts().unwrap(), protected),
        protected_facts
    );
    assert!(protected_facts.is_empty());
}

#[test]
fn two_hosts_create_the_same_colony_vault_descriptor() {
    let a = SigningKey::from_bytes(&[111; 32]);
    let b = SigningKey::from_bytes(&[112; 32]);
    let c = SigningKey::from_bytes(&[113; 32]);
    let read = AdmissionPolicy::quorum(
        [a.verifying_key(), b.verifying_key(), c.verifying_key()],
        1,
        None,
    )
    .unwrap();
    let prepared = [&a, &b].map(|host| {
        let mut store = MemoryRepo::for_host(host.verifying_key());
        let collection = SecretsCollection::register(
            &mut store,
            "colony fixture",
            read_policy(read.clone(), a.verifying_key()),
        )
        .unwrap();
        let observed = snapshot(store.snapshot().unwrap(), collection).unwrap();
        colony_vault(&observed, collection).unwrap()
    });
    let [first, second] = prepared;
    assert_eq!(first.resource, second.resource);
    let first = first.created.expect("absent, so prepared");
    let second = second.created.expect("absent, so prepared");
    assert_eq!(first.facts(), second.facts(), "identical public facts");

    // Concurrent creation converges: both commits land, one vault is found.
    let mut colony = colony();
    let collection = colony.collection;
    let observed = view(&mut colony);
    let one = colony_vault(&observed, collection).unwrap();
    let two = colony_vault(&observed, collection).unwrap();
    drop(observed);
    // A care-style vault under the same policy is not the colony vault.
    let other = resource::create_vault(
        &resource::colony_policy(
            &AdmissionPolicy::quorum(
                [
                    colony.a.verifying_key(),
                    colony.b.verifying_key(),
                    colony.c.verifying_key(),
                ],
                1,
                None,
            )
            .unwrap(),
        )
        .unwrap(),
        "synthetic other vault",
        at(0),
    )
    .unwrap();
    for fragment in [one.created.unwrap(), two.created.unwrap(), other.fragment] {
        colony
            .store
            .commit(collection.source(), &colony.a, fragment)
            .unwrap();
    }
    let observed = view(&mut colony);
    let found = colony_vault(&observed, collection).unwrap();
    assert_eq!(found.resource, one.resource);
    assert!(found.created.is_none());
    assert_ne!(found.resource, other.resource);
}

#[test]
fn adoption_keeps_version_ids_reaches_the_colony_and_is_idempotent() {
    let mut colony = colony();
    let collection = colony.collection;
    // Sealed the old way: own descriptors, direct(A).
    let raw = add_secret(
        &mut colony.store,
        &colony.a,
        collection,
        "old raw",
        RAW,
        at(1),
    )
    .unwrap();
    let fields = login();
    let item = add_item(
        &mut colony.store,
        &colony.a,
        collection,
        "old login",
        ItemInput {
            fields: &fields,
            attachments: &[],
        },
        at(2),
    )
    .unwrap();
    let protected = protected_secret(&mut colony);
    // A member of another vault keeps that vault's audience.
    let care = resource::create_signer_vault(&colony.a, "synthetic care vault", at(0)).unwrap();
    let care_member = record::seal(
        &colony.a,
        "care record",
        at(4),
        Delivery::Vault(care.resource),
        |sealer| {
            let note = sealer.seal_blob::<blobencodings::UTF8String>(
                "SYNTHETIC-PRIVATE-care".to_owned().to_blob(),
                &[],
            )?;
            Ok(entity! { crate::schema::field_text: note }.into_facts())
        },
    )
    .unwrap();
    let care_id = care_member.secret;
    let mut fragment = care.fragment;
    fragment += care_member.fragment;
    colony
        .store
        .commit(collection.source(), &colony.a, fragment)
        .unwrap();

    let old = view(&mut colony);
    let original = envelope::recover(old.store_snapshot(), old.facts().unwrap(), raw, &colony.a)
        .unwrap()
        .remove(0);
    let original_audience = resource::recipients(old.store_snapshot(), &original, now()).unwrap();
    assert_eq!(original_audience, vec![colony.a.verifying_key()]);
    assert!(old.open(raw, &colony.b).is_err());
    let before = old.support().len();
    drop(old);

    // Dry run: lists exactly the two own-descriptor versions, writes nothing.
    let plan = adopt(&mut colony, false).unwrap();
    assert!(!plan.committed);
    let mut listed = plan.adopted.iter().map(|a| a.secret).collect::<Vec<_>>();
    listed.sort();
    let mut expected = vec![raw, item];
    expected.sort();
    assert_eq!(listed, expected);
    assert!(plan
        .adopted
        .iter()
        .any(|a| a.secret == raw && !a.structured && a.name.as_deref() == Some("old raw")));
    assert!(plan
        .adopted
        .iter()
        .any(|a| a.secret == item && a.structured && a.name.as_deref() == Some("old login")));
    assert_eq!(
        view(&mut colony).support().len(),
        before,
        "a dry run commits nothing"
    );

    let done = adopt(&mut colony, true).unwrap();
    assert!(done.committed);
    assert_eq!(done.adopted.len(), 2);
    assert_eq!(done.delivered, 4, "B and C for each adopted version");

    let after = view(&mut colony);
    for host in [&colony.a, &colony.b, &colony.c] {
        assert_eq!(after.open(raw, host).unwrap(), RAW, "same version id");
        assert_eq!(item_password(&after, item, host).unwrap(), PASSWORD);
    }
    assert!(after.open(raw, &colony.outsider).is_err());
    // The original binding is unchanged for old readers.
    assert_eq!(
        &*resource::open_bound_raw(
            after.store_snapshot(),
            after.facts().unwrap(),
            raw,
            original.body,
            original.resource,
            &colony.a,
        )
        .unwrap(),
        RAW
    );
    assert_eq!(
        resource::recipients(after.store_snapshot(), &original, now()).unwrap(),
        original_audience
    );
    // Two bindings for A: own and colony, same body and DEK.
    let bindings = envelope::recover(
        after.store_snapshot(),
        after.facts().unwrap(),
        raw,
        &colony.a,
    )
    .unwrap();
    assert_eq!(bindings.len(), 2);
    assert!(bindings.iter().all(|binding| binding.body == original.body));
    assert!(bindings
        .iter()
        .any(|binding| binding.resource == done.vault));
    // Protected and other-vault versions are untouched.
    assert!(wrap_recipients(after.facts().unwrap(), protected).is_empty());
    assert!(record::open(
        after.store_snapshot().clone(),
        after.facts().unwrap(),
        care_id,
        &colony.b
    )
    .is_err());
    drop(after);

    // Grant by secret still selects the own binding only.
    let current = view(&mut colony);
    let issued = resource::grant(
        &mut colony.store,
        &colony.a,
        &current,
        SecretTarget::Secret(raw),
        colony.outsider.verifying_key(),
        DeliveryLimits::default(),
        false,
    )
    .unwrap();
    assert_eq!(issued.len(), 1);
    drop(current);
    let current = view(&mut colony);
    assert_eq!(
        maintain_recipient_envelopes(
            &mut colony.store,
            &colony.a,
            &current,
            collection,
            &colony.a,
            now()
        )
        .unwrap(),
        1
    );
    drop(current);
    assert_eq!(view(&mut colony).open(raw, &colony.outsider).unwrap(), RAW);
    let settled = view(&mut colony).support().len();

    // A second adoption adds nothing.
    let again = adopt(&mut colony, true).unwrap();
    assert!(again.adopted.is_empty());
    assert_eq!(again.delivered, 0);
    assert_eq!(
        view(&mut colony).support().len(),
        settled,
        "nothing committed"
    );
}

#[test]
fn an_open_read_policy_refuses_colony_sealing() {
    let a = SigningKey::from_bytes(&[121; 32]);
    let mut store = MemoryRepo::for_host(a.verifying_key());
    let collection = SecretsCollection::register(
        &mut store,
        "open fixture",
        read_policy(AdmissionPolicy::Open, a.verifying_key()),
    )
    .unwrap();
    let error = add_secret_to_colony(
        &mut store,
        &a,
        collection,
        observe(collection),
        "refused",
        RAW,
        at(1),
        now(),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("READ policy is Open"),
        "{error:#}"
    );
    let error = adopt_into_colony(
        &mut store,
        &a,
        collection,
        observe(collection),
        now(),
        false,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("READ policy is Open"),
        "{error:#}"
    );
    let observed = snapshot(store.snapshot().unwrap(), collection).unwrap();
    assert!(observed
        .facts()
        .is_none_or(|facts| crate::secret_rows(facts).is_empty()));
}

#[test]
fn a_planted_vault_that_widens_delivery_is_not_the_colony_vault() {
    use crate::schema::{resource_vault, COLONY_VAULT, KIND_SECRET_VAULT};
    use triblespace::core::capability::policy::{resource_handle, resource_policy};
    use triblespace::core::metadata;
    let mut colony = colony();
    let collection = colony.collection;
    let observed = view(&mut colony);
    let genuine = colony_vault(&observed, collection).unwrap();
    drop(observed);
    let policy = resource::colony_policy(
        &AdmissionPolicy::quorum(
            [
                colony.a.verifying_key(),
                colony.b.verifying_key(),
                colony.c.verifying_key(),
            ],
            1,
            None,
        )
        .unwrap(),
    )
    .unwrap();
    // A writer plants a descriptor with the colony marker and the READ
    // policy, plus a second key-delivery binding rooted at an outsider.
    for capability_definition in [
        crate::key_delivery_definition(),
        entity! { triblespace::core::capability::capability_action: crate::schema::ACTION_KEY_DELIVERY },
    ] {
        let mut planted = triblespace::core::trible::Fragment::empty();
        let canonical =
            planted.put::<SimpleArchive, _>(crate::key_delivery_definition().into_facts());
        let other = planted.put::<SimpleArchive, _>(capability_definition.into_facts());
        let descriptor = entity! {
            metadata::tag: crate::schema::KIND_SECRET_RESOURCE,
            resource_vault: COLONY_VAULT,
            resource_policy*: policy.binding(canonical),
            resource_policy*: AdmissionPolicy::direct(colony.outsider.verifying_key()).binding(other),
        };
        let resource = planted.put::<SimpleArchive, _>(descriptor.into_facts());
        planted += entity! { metadata::tag: KIND_SECRET_VAULT, resource_handle: resource };
        colony
            .store
            .commit(collection.source(), &colony.a, planted)
            .unwrap();
        let observed = view(&mut colony);
        let found = colony_vault(&observed, collection).unwrap();
        assert_ne!(
            found.resource, resource,
            "a widened descriptor is never chosen"
        );
        assert_eq!(found.resource, genuine.resource);
        assert!(found.created.is_some(), "the genuine one is still absent");
    }
}

/// Review F1: a definition that is not here at selection may arrive later.
/// A candidate is certified only when every typed delivery binding on it is
/// readable and equal to the colony policy.
#[test]
fn a_candidate_with_an_unreadable_delivery_definition_is_never_chosen() {
    use crate::schema::{resource_vault, COLONY_VAULT, KIND_SECRET_VAULT};
    use triblespace::core::blob::{Blob, IntoBlob};
    use triblespace::core::capability::policy::{resource_handle, resource_policy};
    use triblespace::core::metadata;
    let mut colony = colony();
    let collection = colony.collection;
    let policy = resource::colony_policy(
        &AdmissionPolicy::quorum(
            [
                colony.a.verifying_key(),
                colony.b.verifying_key(),
                colony.c.verifying_key(),
            ],
            1,
            None,
        )
        .unwrap(),
    )
    .unwrap();
    let genuine = resource::colony_vault(&policy).resource;
    // C1: the readable canonical definition. C2: a valid key-delivery
    // definition with distinct bytes, absent when the holder selects.
    let delayed = entity! {
        triblespace::core::capability::capability_action: crate::schema::ACTION_KEY_DELIVERY,
        triblespace::core::capability::capability_delegate_action: crate::schema::ACTION_KEY_DELIVERY,
    };
    let mut planted = triblespace::core::trible::Fragment::empty();
    let c1 = planted.put::<SimpleArchive, _>(crate::key_delivery_definition().into_facts());
    let c2_blob: Blob<SimpleArchive> = delayed.into_facts().to_blob();
    let c2 = c2_blob.get_handle();
    let descriptor = entity! {
        metadata::tag: crate::schema::KIND_SECRET_RESOURCE,
        resource_vault: COLONY_VAULT,
        resource_policy*: policy.binding(c1),
        resource_policy*: AdmissionPolicy::direct(colony.outsider.verifying_key()).binding(c2),
    };
    let planted_resource = planted.put::<SimpleArchive, _>(descriptor.into_facts());
    planted += entity! { metadata::tag: KIND_SECRET_VAULT, resource_handle: planted_resource };
    colony
        .store
        .commit(collection.source(), &colony.a, planted)
        .unwrap();

    let observed = view(&mut colony);
    let found = colony_vault(&observed, collection).unwrap();
    assert_ne!(
        found.resource, planted_resource,
        "a partial view certifies nothing"
    );
    assert_eq!(found.resource, genuine);
    assert!(
        found.created.is_some(),
        "no candidate is eligible, so prepare the intrinsic one"
    );
    drop(observed);
    let added = add_raw(&mut colony, "synthetic while C2 is absent").unwrap();
    assert_eq!(added.vault, genuine);

    // C2 arrives.
    colony.store.put::<SimpleArchive, _>(c2_blob).unwrap();
    let observed = view(&mut colony);
    let found = colony_vault(&observed, collection).unwrap();
    assert_eq!(found.resource, genuine);
    assert!(found.created.is_none());
    // The planted R now would deliver to the outsider; nothing is bound to it.
    assert_eq!(
        maintain_recipient_envelopes(
            &mut colony.store,
            &colony.a,
            &observed,
            collection,
            &colony.a,
            now()
        )
        .unwrap(),
        0
    );
    drop(observed);
    assert!(view(&mut colony)
        .open(added.secret, &colony.outsider)
        .is_err());
}

/// Review F2: an unreadable vault descriptor is not evidence of "no vault".
#[test]
fn adoption_skips_a_version_whose_resource_descriptor_is_missing() {
    let mut colony = colony();
    let collection = colony.collection;
    // The Care member is here; its Care vault descriptor is not.
    let care = resource::create_signer_vault(&colony.a, "synthetic care vault", at(0)).unwrap();
    let member = record::seal(
        &colony.a,
        "care record",
        at(4),
        Delivery::Vault(care.resource),
        |sealer| {
            let note = sealer.seal_blob::<blobencodings::UTF8String>(
                "SYNTHETIC-PRIVATE-care".to_owned().to_blob(),
                &[],
            )?;
            Ok(entity! { crate::schema::field_text: note }.into_facts())
        },
    )
    .unwrap();
    let member_id = member.secret;
    colony
        .store
        .commit(collection.source(), &colony.a, member.fragment)
        .unwrap();
    for commit in [false, true] {
        let report = adopt(&mut colony, commit).unwrap();
        assert!(report.adopted.is_empty(), "commit={commit}");
        assert!(!report.committed);
        assert!(
            report
                .skipped
                .iter()
                .any(|skip| skip.secret == member_id && skip.reason.contains("unavailable")),
            "commit={commit}: {:?}",
            report.skipped
        );
    }
    assert!(record::open(
        view(&mut colony).store_snapshot().clone(),
        view(&mut colony).facts().unwrap(),
        member_id,
        &colony.b
    )
    .is_err());
    // Once the descriptor arrives the member is known to be in a vault.
    colony
        .store
        .commit(collection.source(), &colony.a, care.fragment)
        .unwrap();
    let report = adopt(&mut colony, true).unwrap();
    assert!(report.adopted.is_empty());
    assert!(report.skipped.is_empty());
}

/// When the version is published but delivery cannot follow, the error
/// carries the new version id as a typed receipt.
#[test]
fn a_failed_delivery_after_the_commit_reports_the_version_id() {
    let mut colony = colony();
    let collection = colony.collection;
    let mut calls = 0;
    let error = add_secret_to_colony(
        &mut colony.store,
        &colony.a,
        collection,
        |store: &mut MemoryRepo| {
            calls += 1;
            if calls > 1 {
                bail!("synthetic observation failure");
            }
            snapshot(store.snapshot().unwrap(), collection)
        },
        "synthetic receipt",
        RAW,
        at(1),
        now(),
    )
    .unwrap_err();
    let receipt = error
        .downcast_ref::<CommittedUndelivered>()
        .unwrap_or_else(|| panic!("no receipt in {error:#}"));
    let message = format!("{error:#}");
    assert!(
        message.contains(&format!("{:x}", receipt.secret)),
        "{message}"
    );
    assert!(message.contains("maintain"), "{message}");
    assert_eq!(
        view(&mut colony).open(receipt.secret, &colony.a).unwrap(),
        RAW
    );
}
