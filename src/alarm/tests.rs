use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::UnknownBlob;
use triblespace::core::collection::{CollectionRead, CollectionStore, CollectionStoreExt};
use triblespace::core::repo::memoryrepo::MemoryRepo;
use triblespace::core::repo::{BlobStoreGet, BlobStoreList, BlobStorePut, SnapshotSource};
use triblespace::macros::id_hex;

use super::*;
use crate::schemas::alarm::*;

const JP: Id = id_hex!("10101010101010101010101010101010");
const CC: Id = id_hex!("11111111111111111111111111111111");
/// 2026-11-11 in TAI nanoseconds, roughly; only differences matter.
const T0: i128 = 1_978_000_000 * SECOND;

fn id(byte: u8) -> Id {
    Id::new([byte; 16]).unwrap()
}

fn raise(what: &str) -> Raise {
    Raise {
        raiser: CC,
        for_person: JP,
        class: CLASS_DATA_LOSS,
        host: "sky".to_owned(),
        raised_at: T0,
        what: what.to_owned(),
        decays: "the APFS snapshot cycles out at 04:00".to_owned(),
        evidence: "self.pile is 19 KB, was 6.1 GB at 01:00".to_owned(),
        incident: None,
        test: false,
    }
}

fn ack(alarm: Id, at: i128) -> (Fragment, Id) {
    ack_fragment(&Ack {
        alarm,
        by: JP,
        via: VIA_CLI,
        at,
        discord_message: None,
        note: None,
    })
    .unwrap()
}

fn resolve(alarm: Id, at: i128, false_alarm: bool) -> (Fragment, Id) {
    resolve_fragment(&Resolve {
        alarm,
        by: CC,
        at,
        reason: "restored from the snapshot".to_owned(),
        false_alarm,
    })
    .unwrap()
}

fn step(alarm: Id, rung: Id, started: i128) -> (Fragment, Id) {
    step_fragment(&Step {
        alarm,
        rung,
        host: "sky".to_owned(),
        started,
        finished: started + SECOND,
        outcome: Outcome::Delivered,
        discord_message: None,
        say_request: None,
    })
    .unwrap()
}

// ---- step 2: constructors and readers ---------------------------------------

#[test]
fn an_exact_alarm_retry_converges_on_one_entity() {
    let (first, first_id) = raise_fragment(&raise("pile truncated")).unwrap();
    let (second, second_id) = raise_fragment(&raise("pile truncated")).unwrap();
    assert_eq!(first, second);
    assert_eq!(first_id, second_id);
    let mut union = first.facts().clone();
    union += second.facts().clone();
    let rows = list(&union);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].alarm, first_id);
    assert_eq!(rows[0].raised, T0);
    assert_eq!(rows[0].classes, vec![CLASS_DATA_LOSS]);
    assert_eq!(rows[0].raisers, vec![CC]);
    assert_eq!(rows[0].what, vec![text_handle("pile truncated")]);
    assert!(rows[0].open && !rows[0].test);
    // A different text is a different alarm.
    assert_ne!(raise_fragment(&raise("other")).unwrap().1, first_id);
}

#[test]
fn an_alarm_ack_closes_it_and_so_does_a_resolve() {
    let (raised, alarm) = raise_fragment(&raise("pile truncated")).unwrap();
    assert!(is_open(raised.facts(), alarm));
    let mut acked = raised.facts().clone();
    acked += ack(alarm, T0 + MINUTE).0.facts().clone();
    assert!(!is_open(&acked, alarm));
    assert!(!list(&acked)[0].open);
    assert_eq!(closed_at(&acked, alarm), Some(T0 + MINUTE));
    let mut resolved = raised.facts().clone();
    resolved += resolve(alarm, T0 + 2 * MINUTE, false).0.facts().clone();
    assert!(!is_open(&resolved, alarm));
    // An ack about another alarm closes nothing here.
    let mut elsewhere = raised.facts().clone();
    elsewhere += ack(id(0x77), T0).0.facts().clone();
    assert!(is_open(&elsewhere, alarm));
}

#[test]
fn the_union_of_an_acked_alarm_pile_and_a_stepped_one_is_closed_with_the_step() {
    let (raised, alarm) = raise_fragment(&raise("pile truncated")).unwrap();
    let mut a = raised.facts().clone();
    a += ack(alarm, T0 + 4 * MINUTE).0.facts().clone();
    let mut b = raised.facts().clone();
    let (stepped, step_id) = step(alarm, RUNG_MAC_SOUND, T0 + 6 * MINUTE);
    b += stepped.facts().clone();
    assert!(is_open(&b, alarm));
    let mut union = a.clone();
    union += b.clone();
    assert!(!is_open(&union, alarm));
    let rows = steps(&union, alarm);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step, step_id);
    assert_eq!(rows[0].rung, RUNG_MAC_SOUND);
    assert_eq!(rows[0].started, T0 + 6 * MINUTE);
    assert!(rows[0].delivered && !rows[0].failed);
    // The other order gives the same answer.
    let mut other = b;
    other += a;
    assert!(!is_open(&other, alarm));
}

#[test]
fn an_unknown_fact_about_an_alarm_is_ignored() {
    let (raised, alarm) = raise_fragment(&raise("pile truncated")).unwrap();
    let before = list(raised.facts());
    let future = Attribute::<inlineencodings::GenId>::named("alarm_test_future_attribute");
    let unknown_kind = id(0x99);
    let mut facts = raised.facts().clone();
    // An extra fact on the raise itself, of an attribute this build lacks.
    facts += entity! { ExclusiveId::force_ref(&alarm) @ future: id(0x42) }.into_facts();
    // A record of a kind this build does not know, about the alarm.
    facts += entity! {
        metadata::tag: &unknown_kind,
        alarm::of: alarm,
        metadata::created_at: point_at(T0).unwrap(),
    }
    .into_facts();
    // A step carrying an extra tag and an extra fact.
    let (stepped, step_id) = step(alarm, RUNG_COLONY, T0);
    facts += stepped.facts().clone();
    facts += entity! { ExclusiveId::force_ref(&step_id) @
        metadata::tag: &unknown_kind,
        future: id(0x43),
    }
    .into_facts();
    assert!(is_open(&facts, alarm));
    let after = list(&facts);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].what, before[0].what);
    assert_eq!(after[0].raised, before[0].raised);
    assert_eq!(after[0].open, before[0].open);
    let rows = steps(&facts, alarm);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step, step_id);
}

#[test]
fn an_alarm_raise_missing_optional_fields_still_lists() {
    let (raised, alarm) = raise_fragment(&raise("pile truncated")).unwrap();
    let rows = list(raised.facts());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].alarm, alarm);
    assert!(rows[0].incident.is_empty());
    let facts = raised.facts();
    assert!(!exists!(
        pattern!(facts, [{ alarm @ alarm::incident: _?incident }])
    ));
    // A bare record with only its kind and time, as an older or foreign
    // writer might leave it, still lists.
    let bare = entity! {
        metadata::tag: &KIND_ALARM,
        metadata::created_at: point_at(T0 + SECOND).unwrap(),
    };
    let bare_id = bare.root().unwrap();
    let rows = list(bare.facts());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].alarm, bare_id);
    assert!(rows[0].classes.is_empty() && rows[0].what.is_empty() && rows[0].open);
}

/// A commit can arrive before its member blob (wiki db1046cc, instance 4):
/// it is left out and counted, never an error, and read once it arrives.
#[test]
fn an_alarm_commit_whose_member_blob_has_not_arrived_is_skipped_then_read() {
    let signer = SigningKey::from_bytes(&[7; 32]);
    let mut source = MemoryRepo::for_host(signer.verifying_key());
    let root = crate::collection_names::open(&mut source, DEFAULT_SCOPE_ID, signer.verifying_key())
        .unwrap();
    let (_, rank9) = crate::storage::fact_pair(&mut source, root).unwrap();
    let (first, first_id) = raise_fragment(&raise("pile truncated")).unwrap();
    let mut late = raise("host hung");
    late.class = CLASS_HUNG_HOST;
    let (second, second_id) = raise_fragment(&late).unwrap();
    source.commit(root, &signer, first).unwrap();
    let in_flight = source.commit(root, &signer, second).unwrap().data();
    let full = source.snapshot().unwrap();

    let mut local = MemoryRepo::for_host(signer.verifying_key());
    let mut withheld = None;
    for blob in full.blobs() {
        let handle = blob.unwrap().handle;
        if handle.raw == in_flight.raw {
            withheld = Some(handle);
        } else {
            local
                .put::<UnknownBlob, _>(full.get::<anybytes::Bytes, _>(handle).unwrap())
                .unwrap();
        }
    }
    let withheld = withheld.expect("the in-flight payload is a blob of the source");
    for record in full.records().unwrap() {
        local.insert(record.unwrap()).unwrap();
    }
    let observed = observe(&local.snapshot().unwrap(), rank9)
        .expect("a commit whose bytes are in flight must not fail the read");
    assert_eq!(observed.unread, 1);
    let rows = list(&observed.facts);
    assert_eq!(
        rows.iter().map(|row| row.alarm).collect::<Vec<_>>(),
        vec![first_id]
    );

    // The bytes arrive; the next read sees both.
    local
        .put::<UnknownBlob, _>(full.get::<anybytes::Bytes, _>(withheld).unwrap())
        .unwrap();
    let observed = observe(&local.snapshot().unwrap(), rank9).unwrap();
    assert_eq!(observed.unread, 0);
    let mut seen: Vec<Id> = list(&observed.facts).iter().map(|row| row.alarm).collect();
    seen.sort();
    let mut expected = vec![first_id, second_id];
    expected.sort();
    assert_eq!(seen, expected);
}

#[test]
fn the_alarm_writers_refuse_what_is_not_a_record() {
    let mut unknown = raise("x");
    unknown.class = id(0x55);
    assert!(raise_fragment(&unknown).is_err());
    let mut blank = raise("x");
    blank.decays = "  ".to_owned();
    assert!(raise_fragment(&blank).is_err());
    let mut empty_incident = raise("x");
    empty_incident.incident = Some(String::new());
    assert!(raise_fragment(&empty_incident).is_err());
    assert!(ack_fragment(&Ack {
        alarm: id(1),
        by: JP,
        via: id(0x56),
        at: T0,
        discord_message: None,
        note: None,
    })
    .is_err());
    assert!(step_fragment(&Step {
        alarm: id(1),
        rung: id(0x57),
        host: "sky".to_owned(),
        started: T0,
        finished: T0,
        outcome: Outcome::Delivered,
        discord_message: None,
        say_request: None,
    })
    .is_err());
    assert!(class_id("Data-Loss").is_ok());
    assert!(class_id("unsure").is_err());
}

#[test]
fn a_failed_alarm_step_reads_back_as_failed() {
    let (raised, alarm) = raise_fragment(&raise("pile truncated")).unwrap();
    let (failed, _) = step_fragment(&Step {
        alarm,
        rung: RUNG_HUE_ALERT,
        host: "stars".to_owned(),
        started: T0,
        finished: T0 + SECOND,
        outcome: Outcome::Failed("unreachable from stars".to_owned()),
        discord_message: None,
        say_request: None,
    })
    .unwrap();
    let mut facts = raised.facts().clone();
    facts += failed.facts().clone();
    let rows = steps(&facts, alarm);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].failed && !rows[0].delivered);
}
