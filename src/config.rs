//! This pile's own configuration: which exact collection each name resolves to.
//!
//! See [`crate::schemas::config`] for why the mapping lives in the pile rather
//! than in the process environment or beside it in a file. The short version is
//! that a process environment is a copy taken at start which cannot be
//! invalidated, and a file is a copy a process reads once; a collection is
//! neither, because opening the pile is the thing every faculty call already
//! does. No faculty reads a collection from the environment; a command that
//! writes somewhere other than its default names the collection with
//! `--target` ([`crate::collection_names::write_target`]).

use ed25519_dalek::VerifyingKey;

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
    triblespace::core::collection::selection::config_handle(authority)
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

    /// The handle every existing pile's configuration lives at. Core's
    /// `config_handle_is_pinned` pins the same value for the same key, so the
    /// faculties and `trible` find one collection.
    #[test]
    fn the_handle_is_pinned() {
        assert_eq!(
            hex::encode_upper(handle(key(0x2A)).raw),
            "127AF2EB0A3CD05103BEA7996297584A856B34533002F5B921303C0BDD01E9F7",
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

    /// Saying the same thing twice is ONE state, not two.
    ///
    /// A state written without an explicit id takes an intrinsic one derived
    /// from its content, so two writers stating the same configuration mint the
    /// same entity and the facts collapse under set union. This is the primary
    /// defence against a duplicate, and it is why the reader's own de-duplication
    /// is a backstop rather than the mechanism.
    #[test]
    fn writing_the_same_configuration_twice_collapses_to_one_state() {
        let scope = genid().id;
        let once = entity! {
            crate::schemas::config::config::anchor: scope,
            crate::schemas::config::config::selects: descriptor(0xAB),
        };
        let twice = entity! {
            crate::schemas::config::config::anchor: scope,
            crate::schemas::config::config::selects: descriptor(0xAB),
        };
        assert_eq!(
            once.root(),
            twice.root(),
            "the same statement must mint the same entity"
        );

        let mut facts = once.facts().clone();
        facts.union(twice.facts().clone());
        assert_eq!(
            facts.len(),
            once.facts().len(),
            "the union of one statement with itself adds nothing"
        );
        assert_eq!(
            configured_in(&facts, scope).unwrap(),
            Some(descriptor(0xAB))
        );
    }

    /// Why the supersession edges must sit INSIDE the identity core.
    ///
    /// The intrinsic id is BLAKE3 over the entity's own sorted rows, so it is
    /// a function of exactly what the `entity!` block contains. If the edges
    /// are added afterwards as an annotation, they are not in the core, and a
    /// revert re-mints the id of the state it is reverting TO -- the same
    /// entity would then both supersede and be superseded by the newer one,
    /// closing a cycle in a graph whose whole job is to be acyclic.
    ///
    /// This test pins the collision that causes it. The writer's answer is to
    /// carry the edges in the core, so "select H again, replacing H2" is a
    /// different statement from "select H" and mints a different state.
    #[test]
    fn reverting_would_collide_unless_supersession_is_in_the_core() {
        let scope = genid().id;
        let original = entity! {
            crate::schemas::config::config::anchor: scope,
            crate::schemas::config::config::selects: descriptor(0xAB),
        };
        let reverted_without_edges_in_core = entity! {
            crate::schemas::config::config::anchor: scope,
            crate::schemas::config::config::selects: descriptor(0xAB),
        };
        assert_eq!(
            original.root(),
            reverted_without_edges_in_core.root(),
            "a revert that states only (anchor, selects) IS the earlier state"
        );
    }

    /// And a DIFFERENT statement is a different state, which is what keeps a
    /// revert from closing a cycle in the supersession DAG.
    #[test]
    fn selecting_a_different_collection_mints_a_different_state() {
        let scope = genid().id;
        let one = entity! {
            crate::schemas::config::config::anchor: scope,
            crate::schemas::config::config::selects: descriptor(0xAB),
        };
        let other = entity! {
            crate::schemas::config::config::anchor: scope,
            crate::schemas::config::config::selects: descriptor(0xCD),
        };
        assert_ne!(one.root(), other.root());
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
}
