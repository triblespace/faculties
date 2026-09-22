//! This pile's own configuration: which exact collection each name resolves to.
//!
//! See [`crate::schemas::config`] for why the mapping lives in the pile rather
//! than in the process environment or beside it in a file. The short version is
//! that a process environment is a copy taken at start which cannot be
//! invalidated, and a file is a copy a process reads once; a collection is
//! neither, because opening the pile is the thing every faculty call already
//! does.
//!
//! The environment still WINS when it is set, because pinning one collection
//! for one command is ordinary and has to keep working. What changes is that an
//! environment value which DISAGREES with the pile now says so.

use ed25519_dalek::VerifyingKey;

use triblespace::core::collection::descriptor;
use triblespace::core::collection::records::CollectionHandle;
use triblespace::core::id::Id;
use triblespace::core::inline::Inline;
use triblespace::core::metadata;
use triblespace::core::query::register::{maximal, ObservationOrder};
use triblespace::core::query::TriblePattern;
use triblespace::prelude::*;

pub use crate::schemas::config::{COLLECTION_NAME, DEFAULT_SCOPE_ID};

/// The configuration collection of the pile this key belongs to.
///
/// Derived, never configured. The handle is a pure function of the fixed name
/// and the pile's own signing key, so a process that holds the key already
/// knows where to look and nothing external has to remember a handle. Deriving
/// rather than resolving is also what keeps the mechanism non-circular: the
/// root of a resolution cannot be resolved by the thing it roots.
///
/// A host that has never been configured derives the same handle and finds no
/// descriptor there, which reads as an absent collection. That is the honest
/// open-world answer and the correct fallback -- it is why deriving a handle is
/// safe for a reader even though it would not be for a writer.
pub fn handle(authority: VerifyingKey) -> CollectionHandle {
    descriptor::root_handle_to_read(
        COLLECTION_NAME,
        crate::collection_names::private_policy(authority),
    )
}

/// The collection this pile configures faculty `scope` to use.
///
/// Configuration is a REGISTER, not a table keyed by name. The faculty brings
/// its own stable scope id; that id anchors the register, and the states of
/// that register are ordered by the ordinary `metadata::supersedes` DAG. The
/// current configuration is the state nothing supersedes.
///
/// Keying on identity rather than on a name is the point. A name would have to
/// be hashed to be looked up, two faculties could claim one name, and a name
/// says nothing about which thing it configures. A scope id is already each
/// schema's stable identifier and every faculty already holds its own.
///
/// Ordering by supersession is the other half. Reconfiguring writes a state
/// that supersedes the last one, so an ordinary change is unambiguous and
/// needs no clock, no counter and no retraction -- the store stays append-only
/// and the history of what this host was pointed at stays readable.
///
/// The pattern proposes and the register filters: `maximal` never proposes
/// candidates, it only kills dominated ones, so the planner orders around the
/// anchor lookup exactly as it would any other relation.
///
/// # What is refused
///
/// Genuinely CONCURRENT states -- two configurations neither of which
/// supersedes the other -- are an error rather than a guess. Manufacturing an
/// order between them is the one thing this substrate refuses to do on a
/// reader's behalf, and picking one would silently point a host at a
/// collection nobody chose, which is the exact failure this mechanism exists
/// to end. The fix is one write that supersedes both.
///
/// Concurrent states that select the SAME collection are not a conflict. They
/// agree, and a duplicate is not a fault in a store that cannot reach
/// consensus at write time; erroring on one would assume a coordination we do
/// not have.
pub fn configured_in<P>(facts: &P, scope: Id) -> anyhow::Result<Option<CollectionHandle>>
where
    P: TriblePattern + Sync,
{
    let order = ObservationOrder::new(facts, metadata::supersedes.id());
    let mut heads: Vec<CollectionHandle> = find!(
        (
            state: Id,
            selected: Inline<inlineencodings::Handle<blobencodings::SimpleArchive>>
        ),
        and!(
            pattern!(facts, [{ ?state @
                crate::schemas::config::config::anchor: scope,
                crate::schemas::config::config::selects: ?selected,
            }]),
            maximal(state, &order),
        )
    )
    .map(|(_, selected)| selected)
    .collect();

    heads.sort_unstable();
    heads.dedup();

    match heads.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(*only)),
        several => {
            let listed = several
                .iter()
                .map(|handle| format!("blake3:{}", hex::encode(handle.raw)))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "the {COLLECTION_NAME} collection holds {} concurrent configurations for scope \
                 {scope:X} and none supersedes the others: {listed}. Write one state that \
                 supersedes them rather than letting a reader pick.",
                several.len()
            )
        }
    }
}

/// Which source a resolved handle came from, and whether they disagreed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// Only the environment supplied one.
    Environment,
    /// Only the pile's configuration supplied one.
    Stored,
    /// Both did, and they agree. Nothing to say.
    Agreed,
    /// Both did and they DISAGREE. The environment is used and this is worth
    /// a word, because it is the shape a stale process takes.
    EnvironmentOverridesStored,
}

/// Decide between an environment value and the stored one.
///
/// Pure, and separate from both the reading and the printing, so the
/// precedence can be tested without a pile or a process environment. The
/// caller compares PARSED handles rather than text and passes the verdict in as
/// `equal`, so that `blake3:AB…` and `ab…` count as agreement rather than as
/// drift -- a warning that fires on every call is noise, and noise is what
/// stops anyone reading the one that matters.
pub fn decide(from_env: bool, from_stored: bool, equal: bool) -> Option<Source> {
    match (from_env, from_stored) {
        (true, true) if equal => Some(Source::Agreed),
        (true, true) => Some(Source::EnvironmentOverridesStored),
        (true, false) => Some(Source::Environment),
        (false, true) => Some(Source::Stored),
        (false, false) => None,
    }
}

/// Say, on stderr, that an environment override disagrees with the pile.
///
/// Deliberately not an error. An operator pinning one collection for one
/// command is ordinary and must keep working; what must not happen is that a
/// process carrying a whole retired generation looks exactly like a correct
/// one. Both values are printed in full, because a truncated handle is not
/// enough to tell which generation you are on.
pub fn report_override_divergence(variable: &str, from_env: &str, from_stored: &str) {
    eprintln!(
        "{}",
        override_divergence_note(variable, from_env, from_stored)
    );
}

/// The text of that note, separated so it can be asserted on.
pub fn override_divergence_note(variable: &str, from_env: &str, from_stored: &str) -> String {
    format!(
        "note: {variable} is set to {from_env} but this pile's {COLLECTION_NAME} collection says \
         {from_stored}; using the environment. A process environment is a copy taken when the \
         process started and does not update when the configuration changes -- if this is not \
         deliberate, start a fresh shell."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> VerifyingKey {
        ed25519_dalek::SigningKey::from_bytes(&[byte; 32]).verifying_key()
    }

    /// The property the whole mechanism rests on: a process holding the key
    /// can find its configuration without being told where it is.
    #[test]
    fn the_handle_is_a_pure_function_of_the_key() {
        assert_eq!(handle(key(1)), handle(key(1)));
        assert_ne!(
            handle(key(1)),
            handle(key(2)),
            "one pile's configuration must not be another's"
        );
    }

    fn descriptor(byte: u8) -> CollectionHandle {
        Inline::new([byte; 32])
    }

    /// One configuration state for `scope`, optionally superseding others.
    fn state(id: &Id, scope: Id, selected: CollectionHandle, supersedes: &[Id]) -> TribleSet {
        let mut facts = entity! { ExclusiveId::force_ref(id) @
            crate::schemas::config::config::anchor: scope,
            crate::schemas::config::config::selects: selected,
        }
        .facts()
        .clone();
        for earlier in supersedes {
            facts.union(
                entity! { ExclusiveId::force_ref(id) @
                    metadata::supersedes: ExclusiveId::force_ref(earlier),
                }
                .facts()
                .clone(),
            );
        }
        facts
    }

    #[test]
    fn an_unconfigured_faculty_resolves_to_nothing_rather_than_failing() {
        let facts = TribleSet::new();
        assert_eq!(configured_in(&facts, genid().id).unwrap(), None);
    }

    #[test]
    fn one_state_is_the_configuration() {
        let scope = genid().id;
        let other = genid().id;
        let facts = state(&genid().id, scope, descriptor(0xAB), &[]);

        assert_eq!(
            configured_in(&facts, scope).unwrap(),
            Some(descriptor(0xAB))
        );
        assert_eq!(
            configured_in(&facts, other).unwrap(),
            None,
            "one faculty's register must not answer for another's"
        );
    }

    /// The behaviour a name-keyed table could not express: reconfiguring is an
    /// ordinary append that supersedes, not an edit and not a contradiction.
    #[test]
    fn a_superseding_state_wins_and_the_old_one_stays_readable() {
        let scope = genid().id;
        let first = genid().id;
        let second = genid().id;

        let mut facts = state(&first, scope, descriptor(0xAB), &[]);
        facts.union(state(&second, scope, descriptor(0xCD), &[first]));

        assert_eq!(
            configured_in(&facts, scope).unwrap(),
            Some(descriptor(0xCD)),
            "the head is the state nothing supersedes"
        );
    }

    /// Duplicates are physics in a store that cannot reach consensus at write
    /// time. Two concurrent states that agree are not a conflict.
    #[test]
    fn concurrent_states_selecting_the_same_collection_still_resolve() {
        let scope = genid().id;
        let mut facts = state(&genid().id, scope, descriptor(0xAB), &[]);
        facts.union(state(&genid().id, scope, descriptor(0xAB), &[]));

        assert_eq!(
            configured_in(&facts, scope).unwrap(),
            Some(descriptor(0xAB))
        );
    }

    /// And the case the substrate refuses to guess at.
    #[test]
    fn concurrent_states_that_disagree_are_an_error_not_a_guess() {
        let scope = genid().id;
        let mut facts = state(&genid().id, scope, descriptor(0xAB), &[]);
        facts.union(state(&genid().id, scope, descriptor(0xCD), &[]));

        let error = configured_in(&facts, scope).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("concurrent"), "{text}");
        assert!(text.contains("supersede"), "{text}");
    }

    #[test]
    fn the_environment_wins_but_a_disagreement_is_reported() {
        assert_eq!(decide(true, true, true), Some(Source::Agreed));
        assert_eq!(
            decide(true, true, false),
            Some(Source::EnvironmentOverridesStored)
        );
        assert_eq!(decide(true, false, false), Some(Source::Environment));
        assert_eq!(decide(false, true, false), Some(Source::Stored));
        assert_eq!(decide(false, false, false), None);
    }

    /// A note that does not carry both handles in full cannot tell you which
    /// generation you are on, which is the only question it exists to answer.
    #[test]
    fn the_note_carries_both_values_whole() {
        let note = override_divergence_note("TRIBLESPACE_COLLECTION_MESSAGE", "aaaa", "bbbb");
        assert!(note.contains("TRIBLESPACE_COLLECTION_MESSAGE"), "{note}");
        assert!(note.contains("aaaa") && note.contains("bbbb"), "{note}");
        assert!(
            note.contains("copy taken when the process started"),
            "{note}"
        );
    }
}
