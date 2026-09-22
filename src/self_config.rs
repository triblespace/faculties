//! The one external plaintext configuration, read fresh on every question.
//!
//! # Why a file, and why read it every time
//!
//! Collection handles used to reach a faculty only through the environment:
//! [`crate::collection_names::configured_handle`] read
//! `TRIBLESPACE_COLLECTION_<NAME>` and nothing else. The file beside this one,
//! `faculty-collections.env`, had no standing in the code at all -- it was a
//! shell script somebody had to remember to source, so the process environment
//! was a CACHE of a file the code had never heard of, with nothing to
//! invalidate it.
//!
//! On 2026-09-21 that cost a night. A shell started before the file was edited
//! kept all 25 of its collection handles pointing at a retired generation, so
//! every faculty write went somewhere real and wrong for eight hours: four
//! journal entries, three compass goals, and every message to one peer, into
//! collections nobody else read. The failure was silent because a retired
//! handle is still a VALID handle -- an ABSENT variable already refuses
//! loudly, and a STALE one was simply accepted.
//!
//! So this module does not cache. Not a `OnceLock`, not a lazily-built map:
//! the file is opened and parsed for each resolution. That is a few
//! microseconds against a faculty call that costs seconds, and it is the whole
//! point -- a process that caches at startup reproduces exactly the bug this
//! exists to remove, and a long-lived daemon would hold the stale value for
//! its entire life rather than for one command.
//!
//! # Precedence
//!
//! The environment still wins, because an operator overriding one collection
//! for one command must keep working. But an override that DISAGREES with the
//! file says so on stderr rather than winning in silence, because silence is
//! the thing that cost the night.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Environment variable naming an explicit configuration file.
pub const PATH_OVERRIDE: &str = "TRIBLESPACE_SELF_CONFIG";

/// Where the configuration lives when nobody says otherwise.
///
/// The same directory as the `faculty-collections.env` it replaces, so an
/// operator looks in one place for both during the changeover, and so this
/// needs no pile to find -- the pile is one of the things it configures, and a
/// pile-relative config would have to be found before it could say where the
/// pile is.
pub fn path() -> PathBuf {
    if let Some(explicit) = std::env::var_os(PATH_OVERRIDE) {
        return PathBuf::from(explicit);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("triblespace").join("self.toml")
}

/// The parsed configuration.
#[derive(Debug, Default, serde::Deserialize)]
pub struct SelfConfig {
    /// Faculty scope name to exact 64-digit descriptor handle, `blake3:`
    /// prefix optional. Keys are the faculty's own collection name, not the
    /// shouting environment-variable spelling.
    #[serde(default)]
    pub collections: BTreeMap<String, String>,
}

/// Read the configuration, or `None` when there is no file.
///
/// A missing file is not an error: it is every host that has not been migrated
/// yet, and it must behave exactly as before. A file that exists and does not
/// parse IS an error, for the same reason an invalid handle is one -- falling
/// back would silently choose a different collection than the operator wrote.
pub fn load() -> Result<Option<SelfConfig>> {
    load_from(&path())
}

/// [`load`] against an explicit path.
///
/// Separate so the behaviour can be tested as a pure function of a file rather
/// than by mutating the process environment, which is shared state and races
/// with every other test in the binary.
pub fn load_from(path: &std::path::Path) -> Result<Option<SelfConfig>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", path.display()));
        }
    };
    let parsed: SelfConfig =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(parsed))
}

/// The handle this configuration assigns to one collection name.
pub fn collection(name: &str) -> Result<Option<String>> {
    collection_in(&path(), name)
}

/// [`collection`] against an explicit path.
pub fn collection_in(path: &std::path::Path, name: &str) -> Result<Option<String>> {
    Ok(load_from(path)?.and_then(|config| config.collections.get(name).cloned()))
}

/// Which source a resolved handle came from, and whether they disagreed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// Only the environment supplied one.
    Environment,
    /// Only the file supplied one.
    File,
    /// Both did, and they agree. Nothing to say.
    Agreed,
    /// Both did and they DISAGREE. The environment is used and this is worth
    /// a word, because it is the shape a stale process takes.
    EnvironmentOverridesFile,
}

/// Decide between an environment value and a file value.
///
/// Pure, and separate from both the reading and the printing, so the
/// precedence can be tested without a filesystem or a process environment.
/// The caller compares PARSED handles rather than text and passes the verdict
/// in as `equal`, so that `blake3:AB…` and `ab…` count as agreement rather
/// than as drift -- a warning that fires on every call is noise, and noise is
/// what stops anyone reading the one that matters.
pub fn decide(from_env: bool, from_file: bool, equal: bool) -> Option<Source> {
    match (from_env, from_file) {
        (true, true) if equal => Some(Source::Agreed),
        (true, true) => Some(Source::EnvironmentOverridesFile),
        (true, false) => Some(Source::Environment),
        (false, true) => Some(Source::File),
        (false, false) => None,
    }
}

/// Say, on stderr, that an environment override disagrees with the file.
///
/// Deliberately not an error. An operator pinning one collection for one
/// command is ordinary and must keep working; what must not happen is that a
/// process carrying a whole retired generation looks exactly like a correct
/// one. Both values are printed in full, because a truncated handle is not
/// enough to tell which generation you are on.
pub fn report_override_divergence(variable: &str, from_env: &str, from_file: &str) {
    eprintln!(
        "{}",
        override_divergence_note(variable, from_env, from_file)
    );
}

/// The text of that note, separated so it can be asserted on.
pub fn override_divergence_note(variable: &str, from_env: &str, from_file: &str) -> String {
    format!(
        "note: {variable} is set to {from_env} but {} says {from_file}; using the environment. \
         A process environment is a copy taken when the process started and does not update when \
         the file is edited -- if this is not deliberate, re-source it.",
        path().display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_file_is_not_an_error_because_that_is_every_unmigrated_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        assert!(load_from(&path).unwrap().is_none());
        assert!(collection_in(&path, "message").unwrap().is_none());
    }

    #[test]
    fn a_file_that_exists_and_does_not_parse_fails_rather_than_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.toml");
        std::fs::write(&path, "collections = [ this is not toml").unwrap();
        let error = load_from(&path).unwrap_err();
        assert!(format!("{error:#}").contains("parse"), "{error:#}");
    }

    #[test]
    fn a_handle_is_read_back_under_the_faculty_name_not_the_variable_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("self.toml");
        std::fs::write(
            &path,
            "[collections]\nmessage = \"a6d95693c5290e2b214ec1399eba1e420a1d2573618065fc6b59e0da9c05298b\"\n",
        )
        .unwrap();
        assert_eq!(
            collection_in(&path, "message").unwrap().as_deref(),
            Some("a6d95693c5290e2b214ec1399eba1e420a1d2573618065fc6b59e0da9c05298b")
        );
        assert!(collection_in(&path, "compass").unwrap().is_none());
    }

    /// The precedence, as a table, with no filesystem and no environment.
    #[test]
    fn the_environment_wins_and_only_a_disagreement_is_worth_saying() {
        use Source::*;
        assert_eq!(decide(false, false, true), None);
        assert_eq!(decide(true, false, true), Some(Environment));
        assert_eq!(decide(false, true, true), Some(File));
        // Both present and equal: the environment is used, and SILENTLY --
        // this is the ordinary case on a correctly-sourced host and must not
        // print anything at all.
        assert_eq!(decide(true, true, true), Some(Agreed));
        // Both present and different: still the environment, but said out
        // loud. This is the case that cost 2026-09-21.
        assert_eq!(decide(true, true, false), Some(EnvironmentOverridesFile));
    }

    /// The note has to carry BOTH handles in full. A truncated one cannot tell
    /// you which generation you are on, which is the only question being asked.
    #[test]
    fn the_divergence_note_names_the_variable_and_both_handles_in_full() {
        let stale = "b89287c34747ba5e737294049e2df991ded0f10b5b2ccc1f468afd3feb096cc7";
        let live = "a6d95693c5290e2b214ec1399eba1e420a1d2573618065fc6b59e0da9c05298b";
        let note = override_divergence_note("TRIBLESPACE_COLLECTION_MESSAGE", stale, live);
        assert!(note.contains("TRIBLESPACE_COLLECTION_MESSAGE"), "{note}");
        assert!(note.contains(stale), "{note}");
        assert!(note.contains(live), "{note}");
        assert!(note.contains("using the environment"), "{note}");
    }

    /// The property the whole module exists for: an edit is visible to the
    /// very next question, with no restart and nothing to invalidate. A
    /// `OnceLock` here would pass every other test in this file and fail this
    /// one, which is why it is written down.
    #[test]
    fn an_edit_is_seen_by_the_next_read_because_nothing_is_cached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("self.toml");
        let old = "b89287c34747ba5e737294049e2df991ded0f10b5b2ccc1f468afd3feb096cc7";
        let new = "a6d95693c5290e2b214ec1399eba1e420a1d2573618065fc6b59e0da9c05298b";
        std::fs::write(&path, format!("[collections]\nmessage = \"{old}\"\n")).unwrap();
        assert_eq!(
            collection_in(&path, "message").unwrap().as_deref(),
            Some(old)
        );
        std::fs::write(&path, format!("[collections]\nmessage = \"{new}\"\n")).unwrap();
        assert_eq!(
            collection_in(&path, "message").unwrap().as_deref(),
            Some(new),
            "a cached read would still answer with the retired handle"
        );
    }
}
