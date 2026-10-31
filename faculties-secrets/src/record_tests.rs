use super::*;
use crate::resource::{self, DeliveryLimits, SecretTarget};
use crate::storage::{self, SecretsCollection};
use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, CollectionStoreExt};
use triblespace::core::repo::memoryrepo::MemoryRepo;

const NOTE: &str = "SYNTHETIC-PRIVATE-note";
const PHOTO: &[u8] = b"SYNTHETIC-PRIVATE-photo\0\xff";

type NoteHandle =
    Inline<inlineencodings::Handle<crate::encrypted::Encrypted<blobencodings::UTF8String>>>;
type PhotoHandle =
    Inline<inlineencodings::Handle<crate::encrypted::Encrypted<blobencodings::RawBytes>>>;

/// Test-only attributes; anchors minted with `trible genid` on 2026-10-31.
mod schema {
    use triblespace::prelude::*;
    attributes! {
        "4C155897FF480B17955D6D0FFF4C3890" as pub value: inlineencodings::GenId;
        "20456ADCE999C139236901BF0CF57574" as pub note:
            inlineencodings::Handle<crate::encrypted::Encrypted<blobencodings::UTF8String>>;
        "35E072FF0F715550C4C655E6295C213F" as pub photo:
            inlineencodings::Handle<crate::encrypted::Encrypted<blobencodings::RawBytes>>;
    }
}

fn at(second: i64) -> IntervalValue {
    let epoch = Epoch::from_unix_seconds(second as f64);
    (epoch, epoch).try_to_inline().unwrap()
}

fn fixture(seed: u8) -> (MemoryRepo, SecretsCollection, SigningKey) {
    let signer = SigningKey::from_bytes(&[seed; 32]);
    let mut store = MemoryRepo::for_host(signer.verifying_key());
    let collection = SecretsCollection::register(
        &mut store,
        "record fixture",
        CollectionPolicy::new(
            AdmissionPolicy::Open,
            AdmissionPolicy::direct(signer.verifying_key()),
        ),
    )
    .unwrap();
    (store, collection, signer)
}

/// Seal one record with a note and photo inside it.
fn record(signer: &SigningKey, marker: Id, delivery: Delivery) -> SealedVersion {
    seal(signer, "synthetic record", at(1), delivery, |sealer| {
        let note = sealer.seal_blob::<blobencodings::UTF8String>(NOTE.to_owned().to_blob(), &[])?;
        let photo = sealer.seal_blob::<blobencodings::RawBytes>(PHOTO.to_vec().to_blob(), &[])?;
        Ok(
            entity! { schema::value: marker, schema::note: note, schema::photo: photo }
                .into_facts(),
        )
    })
    .unwrap()
}

#[test]
fn a_record_and_its_attachments_seal_as_one_version_and_open_together() {
    let (mut store, collection, alice) = fixture(61);
    let marker = genid().id;
    let sealed = record(&alice, marker, Delivery::Own);
    let version = sealed.secret;
    store
        .commit(collection.source(), &alice, sealed.fragment)
        .unwrap();
    let snapshot =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    let public = snapshot.facts().unwrap().clone();
    let reader = snapshot.store_snapshot().clone();
    let opened = open(reader.clone(), &public, version, &alice).unwrap();
    assert_eq!(opened.len(), 1);
    let record = &opened[0];
    assert!(record.resource().is_some());
    let (note, photo) = find!((note: NoteHandle, photo: PhotoHandle),
        pattern!(record.facts(), [{ schema::value: marker, schema::note: ?note, schema::photo: ?photo }]))
    .next()
    .unwrap();
    assert_eq!(
        record.open_child(note).unwrap().bytes.as_ref(),
        NOTE.as_bytes()
    );
    assert_eq!(record.open_child(photo).unwrap().bytes.as_ref(), PHOTO);
    // Not an item: the item path refuses it, the record path does not.
    assert!(crate::item::open(reader.clone(), &public, version, &alice).is_err());
    // Another key holds no envelope.
    assert!(open(reader, &public, version, &SigningKey::from_bytes(&[62; 32])).is_err());
    // No plaintext anywhere in the store.
    let mut blobs = store.blobs.clone();
    for (_, blob) in triblespace::core::repo::SnapshotSource::snapshot(&mut blobs)
        .unwrap()
        .iter()
    {
        assert!(!blob.bytes.windows(10).any(|w| w == b"SYNTHETIC-"));
    }
}

#[test]
fn one_group_grant_covers_every_member_including_later_ones_and_nothing_else() {
    let (mut store, collection, alice) = fixture(63);
    let bob = SigningKey::from_bytes(&[64; 32]);
    let carol = SigningKey::from_bytes(&[65; 32]);
    let group = resource::create_group(&alice, "synthetic group", at(0)).unwrap();
    let resource = group.resource;
    store
        .commit(collection.source(), &alice, group.fragment)
        .unwrap();
    let member = |store: &mut MemoryRepo| {
        let sealed = record(&alice, genid().id, Delivery::Group(resource));
        let id = sealed.secret;
        store
            .commit(collection.source(), &alice, sealed.fragment)
            .unwrap();
        id
    };
    let first = member(&mut store);
    let second = member(&mut store);
    let outside = record(&alice, genid().id, Delivery::Own);
    let outside_id = outside.secret;
    store
        .commit(collection.source(), &alice, outside.fragment)
        .unwrap();

    let snapshot =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    assert!(resource::is_group(snapshot.store_snapshot(), resource));
    // A grant naming one member never widens to its group.
    assert!(resource::grant(
        &mut store,
        &alice,
        &snapshot,
        SecretTarget::Secret(first),
        carol.verifying_key(),
        DeliveryLimits::default(),
        false,
    )
    .is_err());
    resource::grant(
        &mut store,
        &alice,
        &snapshot,
        SecretTarget::Resource(resource),
        bob.verifying_key(),
        DeliveryLimits::default(),
        false,
    )
    .unwrap();
    let maintain = |store: &mut MemoryRepo| {
        let snapshot =
            pollster::block_on(storage::ensure_and_snapshot(store, collection, &alice)).unwrap();
        storage::maintain_recipient_envelopes(
            store,
            &alice,
            &snapshot,
            collection,
            &alice,
            Epoch::from_unix_seconds(5.0),
        )
        .unwrap()
    };
    assert_eq!(maintain(&mut store), 2, "one envelope per member for bob");
    // A member added after the grant reaches bob at the next maintenance:
    // the group's policy decides future delivery.
    let later = member(&mut store);
    assert_eq!(maintain(&mut store), 1);
    assert_eq!(
        maintain(&mut store),
        0,
        "maintenance is additive and idempotent"
    );

    let snapshot =
        pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice)).unwrap();
    let public = snapshot.facts().unwrap().clone();
    let reader = snapshot.store_snapshot().clone();
    for id in [first, second, later] {
        let opened = open(reader.clone(), &public, id, &bob).unwrap();
        assert_eq!(opened[0].resource(), Some(resource));
        let note = find!(note: NoteHandle, pattern!(opened[0].facts(), [{ schema::note: ?note }]))
            .next()
            .unwrap();
        assert_eq!(
            opened[0].open_child(note).unwrap().bytes.as_ref(),
            NOTE.as_bytes()
        );
    }
    assert!(open(reader.clone(), &public, outside_id, &bob).is_err());
    assert!(open(reader, &public, first, &carol).is_err());
}
