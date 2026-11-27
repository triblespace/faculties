//! Callable Schemas operations over the configured collection, without output
//! routing, CLI invocations, or MCP values.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::CollectionStoreExt;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::{find, pattern, Id};

use super::{named_entries, named_entry, SchemasEntry};
use crate::schemas::atlas::DEFAULT_SCOPE_ID;
use crate::storage::{AcquiringReader, FactArchive, FacultySnapshot, Noted, Storage};

/// An acquiring Schemas reader. Each operation opens the existing collection
/// mappings, then projects facts and attachments from one final snapshot.
/// Returned entries are owned observations, not a second mutable catalog.
///
/// Reads may append deterministic maintained derivations, but never author
/// Schemas metadata or create a signing key. Storage acquisition is lazy; each
/// operation borrows the configured lifetime and freezes its own observation.
pub struct Store {
    storage: Storage,
}

impl Store {
    pub fn open(pile_path: &Path, key_path: Option<&Path>) -> Result<Self> {
        Ok(Self::with_storage(Storage::new(
            pile_path.to_owned(),
            key_path.map(Path::to_owned),
        )))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    /// Every named entity, ordered by all name variants and then entity id.
    /// Read over what is here, as [`Self::show`] is: the answer carries a
    /// note naming each foundation the read could not load.
    pub fn list(&self) -> Result<Noted<Vec<SchemasEntry>>> {
        self.with_view(|facts, reader| {
            let mut rows = named_entries(reader, facts)?;
            rows.sort_by(|left, right| {
                left.names
                    .cmp(&right.names)
                    .then_with(|| left.id.cmp(&right.id))
            });
            Ok(rows)
        })
    }

    /// One named entity selected by a case-insensitive hexadecimal id prefix.
    /// Missing and ambiguous prefixes are errors, never arbitrary winners.
    /// Not finding a prefix while a foundation is away refuses with the note.
    pub fn show(&self, prefix: &str) -> Result<Noted<SchemasEntry>> {
        self.with_view(|facts, reader| {
            let id = resolve_prefix(facts, prefix)?;
            named_entry(reader, facts, id)?
                .ok_or_else(|| anyhow!("resolved Schemas id {id:x} has no typed name"))
        })
    }

    pub fn close(self) -> Result<()> {
        self.storage.close().context("close Schemas pile")
    }

    pub(super) fn finish<T>(self, result: Result<T>) -> Result<T> {
        match (result, self.close()) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) | (Err(error), Ok(())) => Err(error),
            (Err(error), Err(close_error)) => {
                Err(error.context(format!("closing Schemas pile also failed: {close_error:#}")))
            }
        }
    }

    fn with_view<T>(
        &self,
        operation: impl FnOnce(&FactArchive, &AcquiringReader<FacultySnapshot>) -> Result<T>,
    ) -> Result<Noted<T>> {
        let mut notes = Vec::new();
        let result = self.storage.with_store(|pile, signer, runtime| {
            let source = self.storage.open_collection_read(
                pile,
                DEFAULT_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            let collection_succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
            let collection_rank9 =
                pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, collection_succinct)?;
            let store_snapshot = AcquiringReader::new(
                pile.snapshot().context("freeze Schemas fact collection")?,
                std::sync::Arc::clone(runtime),
            );
            let facts = crate::storage::acquire_facts_noted(
                &store_snapshot,
                collection_rank9,
                "Schemas",
                &mut notes,
            )
            .context("read maintained Schemas fact collection")?;
            operation(&facts, &store_snapshot)
        });
        match result {
            Ok(value) => Ok(Noted { value, notes }),
            Err(error) => Err(crate::storage::with_unread_notes(error, &notes)),
        }
    }
}

fn resolve_prefix<P: TriblePattern>(facts: &P, prefix: &str) -> Result<Id> {
    let prefix = prefix.trim().to_lowercase();
    if prefix.is_empty() {
        bail!("id prefix is empty");
    }
    let mut matches = find!(
        id: Id,
        pattern!(facts, [{ ?id @ metadata::name: _?name }])
    )
    .filter(|id| format!("{id:x}").starts_with(&prefix))
    .collect::<BTreeSet<_>>()
    .into_iter();
    match (matches.next(), matches.next()) {
        (None, _) => bail!("no id matches prefix '{prefix}'"),
        (Some(id), None) => Ok(id),
        (Some(_), Some(_)) => bail!("multiple ids match prefix '{prefix}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::residual_fixture::{
        is_fault, malformed_in, storage, strand_in, unrelated_fragment,
    };

    #[test]
    fn reads_name_a_schemas_foundation_that_is_away_and_a_fault_fails() {
        let directory = tempfile::tempdir().unwrap();
        let storage = storage(directory.path());
        let store = Store::with_storage(storage.clone());
        let away = strand_in(&storage, DEFAULT_SCOPE_ID, unrelated_fragment());
        let listed = store.list().unwrap();
        assert!(
            listed.notes.iter().any(|note| note.contains(&away)),
            "{:?}",
            listed.notes
        );
        let error = store.show("ab").unwrap_err();
        assert!(format!("{error:#}").contains(&away), "{error:#}");
        let pending = malformed_in(&storage, DEFAULT_SCOPE_ID);
        assert!(store
            .list()
            .unwrap()
            .notes
            .iter()
            .any(|note| note.contains(&pending)));
        crate::storage::residual_fixture::malformed_attachment_in(&storage, DEFAULT_SCOPE_ID);
        assert!(is_fault(&store.list().unwrap_err()));
        storage.close().unwrap();
    }
}
