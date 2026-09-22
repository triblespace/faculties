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

use triblespace::core::blob::IntoBlob;
use triblespace::core::collection::descriptor;
use triblespace::core::collection::records::{collection_name, CollectionHandle};
use triblespace::core::id::Id;
use triblespace::core::inline::Inline;
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

/// The exact descriptor this pile's configuration resolves `name` to.
///
/// Open world throughout. A name with no entry is `None` rather than an error,
/// because that is every host not yet configured. A name with SEVERAL entries
/// is also `None`, and deliberately: two entries disagreeing about which
/// collection a name means is exactly the case where guessing is worst, and the
/// schema language cannot forbid the second entry anyway. Picking the first
/// match would make the answer depend on iteration order.
pub fn resolve_in<P>(facts: &P, name: &str) -> Option<CollectionHandle>
where
    P: TriblePattern + ?Sized,
{
    let named = name_handle(name);
    let mut found = find!(
        (entry: Id, selected: Inline<inlineencodings::Handle<blobencodings::SimpleArchive>>),
        pattern!(facts, [{ ?entry @
            collection_name: named,
            crate::schemas::config::config::resolves_to: ?selected,
        }])
    )
    .map(|(_, selected)| selected);

    let first = found.next()?;
    // A second row means the configuration contradicts itself.
    found.next().is_none().then_some(first)
}

/// Content address of a collection name, the way a descriptor stores it.
///
/// `collection_name` is a `Handle<UTF8String>`, so looking one up means hashing
/// the name rather than comparing text. Pure, and the same bytes a descriptor
/// would carry -- which is what lets a configuration entry and a descriptor be
/// joined on the same value.
fn name_handle(name: &str) -> Inline<inlineencodings::Handle<blobencodings::UTF8String>> {
    IntoBlob::<blobencodings::UTF8String>::to_blob(name.to_owned()).get_handle()
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
