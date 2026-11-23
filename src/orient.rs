//! Orient's grow-only presentation ledger.
//!
//! Source faculties define which events deserve attention. Orient records only
//! the irreducible observer state: whether one zooid has already been presented
//! one exact event. Every receipt names that zooid, its stable Relations
//! persona id, in its identity core: the signing key belongs to the node that
//! wrote the receipt, and one node serves many zooids (JP, 2026-11-23). So one
//! synced receipt collection can carry every zooid's seen state, and a receipt
//! for A never suppresses the event for B. A receipt without a persona (written
//! before 2026-11-23, when the key stood in for the observer) names nobody and
//! suppresses nobody: its owner is a key many zooids shared.
//! Store snapshots and collection covers are local
//! continuation tokens, not durable facts, and therefore do not live here.

use std::collections::BTreeSet;

use triblespace::core::metadata;
use triblespace::macros::{entity, find, pattern};
use triblespace::prelude::*;

use crate::schemas::orient::{presentation, KIND_PRESENTED};

pub mod cli;
pub mod delivery;
pub mod mcp;
mod operations;
#[cfg(all(test, unix))]
mod process_tests;
pub use operations::{
    BaselineReceipt, NewsForm, Next, NextKind, NextCompletion, Orient, PendingNext, PreparedReport, ShowOptions, WaitOptions,
    WakeOptions, WakePairsOptions, WakeTint,
};

/// Receipt facts saying `persona` was presented each event. The persona is
/// part of the receipt's identity, so the same event presented to two zooids
/// is two receipts. Event identity is stable across retries; observation
/// times annotate that identity, never qualify the derived ID set.
pub fn receipt_fragment(
    persona: Id,
    events: impl IntoIterator<Item = Id>,
    created_at: Inline<inlineencodings::NsTAIInterval>,
) -> Fragment {
    let mut fragment = Fragment::empty();
    for event in events {
        let receipt = entity! {
            metadata::tag: &KIND_PRESENTED,
            presentation::persona: &persona,
            presentation::event: &event,
        };
        let id = receipt.root().expect("one receipt entity");
        fragment += receipt;
        fragment += entity! { ExclusiveId::force_ref(&id) @ metadata::created_at: &created_at };
    }
    fragment
}

/// Receipts for due occurrences of standing intentions.
///
/// A habit's due event is an intention and an instant, not a record with an id
/// of its own, so the receipt cites both rather than a derived identity. The
/// pair is what a reader joins on, and joining is the point: an id that has to
/// be computed before it can be looked up is a hash-join, and only a set of ids
/// ever made one necessary here.
pub fn habit_receipt_fragment(
    persona: Id,
    due: impl IntoIterator<Item = (Id, Inline<inlineencodings::NsTAIInterval>)>,
    created_at: Inline<inlineencodings::NsTAIInterval>,
) -> Fragment {
    let mut fragment = Fragment::empty();
    for (habit, due_at) in due {
        let receipt = entity! {
            metadata::tag: &KIND_PRESENTED,
            presentation::persona: &persona,
            presentation::habit: &habit,
            presentation::due_at: &due_at,
        };
        let id = receipt.root().expect("one habit receipt entity");
        fragment += receipt;
        fragment += entity! { ExclusiveId::force_ref(&id) @ metadata::created_at: &created_at };
    }
    fragment
}

fn presented_record(persona: Id, event: Id) -> Fragment {
    entity! {
        metadata::tag: &KIND_PRESENTED,
        presentation::persona: &persona,
        presentation::event: &event,
    }
}

/// Legacy mixed-persona receipt facts, retained for explicit imports.
/// Build intrinsic grow-only facts saying that `persona` was presented each
/// supplied event.
///
/// Repeating a pair produces the same entity and is therefore idempotent.
/// Presentation is deliberately distinct from a source faculty's native
/// "read", "handled", or "acknowledged" state.
pub fn presented_fragment(persona: Id, events: impl IntoIterator<Item = Id>) -> Fragment {
    let mut fragment = Fragment::empty();
    for event in events {
        fragment += presented_record(persona, event);
    }
    fragment
}

/// Exact event identities in a legacy mixed-persona receipt collection.
pub fn presented_events(facts: &TribleSet, persona: Id) -> BTreeSet<Id> {
    find!(
        event: Id,
        pattern!(facts, [{
            _?presentation @
            metadata::tag: &KIND_PRESENTED,
            presentation::persona: &persona,
            presentation::event: ?event,
        }])
    )
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_time_annotates_identity_without_qualifying_membership() {
        let persona = Id::new([7; 16]).unwrap();
        let event = Id::new([8; 16]).unwrap();
        let first_time = crate::clock::point(hifitime::Epoch::from_unix_seconds(10.0)).unwrap();
        let next_time = crate::clock::point(hifitime::Epoch::from_unix_seconds(20.0)).unwrap();
        let first = receipt_fragment(persona, [event], first_time);
        let next = receipt_fragment(persona, [event, event], next_time);
        let subjects = |facts: &TribleSet| {
            find!(id: Id, pattern!(facts, [{ ?id @ presentation::event: &event }]))
                .collect::<BTreeSet<_>>()
        };
        let identities = subjects(first.facts());
        assert_eq!(identities.len(), 1);
        assert_eq!(identities, subjects(next.facts()));
        let mut both = first;
        both += next;
        let receipt = *identities.first().unwrap();
        let times: BTreeSet<Inline<inlineencodings::NsTAIInterval>> = find!(
            at: Inline<inlineencodings::NsTAIInterval>,
            pattern!(both.facts(), [{ receipt @ metadata::created_at: ?at }])
        )
        .collect();
        assert_eq!(times, BTreeSet::from([first_time, next_time]));
        assert_eq!(presented_events(both.facts(), persona), BTreeSet::from([event]));
    }

    #[test]
    fn the_receiving_persona_is_part_of_each_receipt_identity() {
        let first = Id::new([1; 16]).unwrap();
        let second = Id::new([2; 16]).unwrap();
        let event = Id::new([3; 16]).unwrap();
        let habit = Id::new([4; 16]).unwrap();
        let at = crate::clock::point(hifitime::Epoch::from_unix_seconds(10.0)).unwrap();
        let due = crate::clock::point(hifitime::Epoch::from_unix_seconds(5.0)).unwrap();
        let roots = |fragment: Fragment| {
            find!(id: Id, pattern!(fragment.facts(), [{ ?id @ metadata::tag: &KIND_PRESENTED }]))
                .collect::<BTreeSet<_>>()
        };
        let a = roots(receipt_fragment(first, [event], at));
        let b = roots(receipt_fragment(second, [event], at));
        assert_eq!(a.len(), 1);
        assert!(a.is_disjoint(&b));
        let mut shared = receipt_fragment(first, [event], at);
        shared += receipt_fragment(second, [event], at);
        assert_eq!(presented_events(shared.facts(), first), BTreeSet::from([event]));
        assert_eq!(presented_events(shared.facts(), second), BTreeSet::from([event]));
        let ha = roots(habit_receipt_fragment(first, [(habit, due)], at));
        let hb = roots(habit_receipt_fragment(second, [(habit, due)], at));
        assert_eq!(ha.len(), 1);
        assert!(ha.is_disjoint(&hb));
        // The current constructor and the legacy mixed-ledger record agree on
        // identity, so an imported legacy receipt and a new one coincide.
        assert_eq!(a, roots(presented_fragment(first, [event])));
    }

    #[test]
    fn presentation_atoms_are_intrinsic_and_idempotent() {
        let persona = Id::new([1; 16]).unwrap();
        let first = Id::new([2; 16]).unwrap();
        let second = Id::new([3; 16]).unwrap();

        let once = presented_fragment(persona, [first, second]);
        let mut repeated = presented_fragment(persona, [second, first]);
        repeated += presented_fragment(persona, [first, first]);

        assert_eq!(once.facts(), repeated.facts());
        assert_eq!(
            presented_events(once.facts(), persona),
            BTreeSet::from([first, second]),
        );
    }

    #[test]
    fn presentation_is_scoped_to_the_exact_persona() {
        let first_persona = Id::new([1; 16]).unwrap();
        let second_persona = Id::new([2; 16]).unwrap();
        let shared_event = Id::new([3; 16]).unwrap();
        let private_event = Id::new([4; 16]).unwrap();

        let mut facts = presented_fragment(first_persona, [shared_event, private_event]);
        facts += presented_fragment(second_persona, [shared_event]);

        assert_eq!(
            presented_events(facts.facts(), first_persona),
            BTreeSet::from([shared_event, private_event]),
        );
        assert_eq!(
            presented_events(facts.facts(), second_persona),
            BTreeSet::from([shared_event]),
        );
    }

    #[test]
    fn unrelated_open_world_facts_are_ignored() {
        let persona = Id::new([1; 16]).unwrap();
        let event = Id::new([2; 16]).unwrap();
        let unrelated = Id::new([3; 16]).unwrap();
        let mut facts = presented_fragment(persona, [event]);
        facts += entity! {
            metadata::tag: &unrelated,
        };

        assert_eq!(
            presented_events(facts.facts(), persona),
            BTreeSet::from([event]),
        );
    }
}
