//! Whole-collection Cognition validation as a direct library operation.
//! Event authoring remains in the existing shared publication API.

use std::path::PathBuf;

use anybytes::View;
use anyhow::{Context, Result};
use itertools::Itertools;
use triblespace::core::blob::encodings::simplearchive::CanonicalTribles;
use triblespace::core::collection::CollectionSnapshotExt;
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};

use crate::schemas::cognition::DEFAULT_SCOPE_ID;
use crate::storage::Storage;

#[derive(Clone, Debug)]
pub struct Cognition {
    storage: Storage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckReport {
    pub facts: usize,
    /// Selected raw source-cover members whose bytes could not be loaded:
    /// the check validates every other one and names these
    /// ([`crate::storage::unread_foundations_note`]).
    pub notes: Vec<String>,
}

impl CheckReport {
    pub fn summary(&self) -> String {
        format!(
            "Cognition scope {DEFAULT_SCOPE_ID:X}: {} facts validated",
            self.facts
        )
    }
}

impl Cognition {
    /// Trusted launcher-owned configuration, not caller-selected tool fields.
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    pub fn check(&self) -> Result<CheckReport> {
        self.storage.with_store(|pile, signer, runtime| {
            let source = self.storage.open_collection_read(
                pile,
                DEFAULT_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            let snapshot = crate::storage::AcquiringReader::new(
                pile.snapshot()
                    .context("freeze Cognition fact collection")?,
                runtime.clone(),
            );
            // This explicit diagnostic validates the selected raw source,
            // independently of attachment upkeep. CanonicalTribles checks
            // bytes in place; no indexed representation is built/published.
            let selected = snapshot.collection_acquiring(source)?;
            let mut shards = Vec::<View<CanonicalTribles>>::new();
            let mut unread = Vec::new();
            for handle in selected.cover().members() {
                match snapshot.get::<View<CanonicalTribles>, _>(handle) {
                    Ok(shard) => shards.push(shard),
                    Err(error) if triblespace::core::repo::is_missing_blob(&error) => {
                        unread.push(handle)
                    }
                    Err(error) => {
                        return Err(
                            anyhow::Error::new(error).context("validate raw Cognition source")
                        )
                    }
                }
            }
            let notes: Vec<_> = crate::storage::unread_handles_note(
                "Cognition source cover",
                unread.iter().copied(),
                unread.len(),
            )
            .into_iter()
            .collect();
            // Preserve logical set/count semantics while streaming the sorted
            // raw shards, rather than flattening them into another database.
            let mut facts = 0;
            let rows = shards
                .iter()
                .map(|shard| shard.as_tribles().iter().copied())
                .kmerge()
                .dedup()
                .inspect(|_| facts += 1);
            super::validate_payloads(
                &snapshot,
                None::<&triblespace::core::repo::pile::PileSnapshot>,
                rows,
            )
                .map_err(|error| crate::storage::with_unread_notes(error, &notes))?;
            Ok(CheckReport { facts, notes })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::residual_fixture::{
        is_fault, malformed_in, storage, strand_in, unrelated_fragment,
    };

    #[test]
    fn a_check_validates_what_is_here_and_names_a_foundation_that_is_away() {
        use triblespace::core::collection::{CollectionRead, CollectionStoreExt};
        let directory = tempfile::tempdir().unwrap();
        let storage = storage(directory.path());
        let cognition = Cognition::with_storage(storage.clone());
        assert!(cognition.check().unwrap().notes.is_empty());
        let first = unrelated_fragment();
        let mut second = first.clone();
        second += unrelated_fragment();
        let expected = second.facts().len();
        let before = storage
            .with_pile(|pile, signer| {
                let source = storage.open_collection_local(
                    pile,
                    DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                )?;
                pile.commit(source, signer, first)?;
                pile.commit(source, signer, second)?;
                Ok(pile.snapshot()?.records()?.count())
            })
            .unwrap();
        let report = cognition.check().unwrap();
        assert_eq!(
            report.facts, expected,
            "raw validation preserves union deduplication"
        );
        assert!(
            report.notes.is_empty(),
            "a valid raw source needs no attachment upkeep"
        );
        let after = storage
            .with_pile(|pile, _| Ok(pile.snapshot()?.records()?.count()))
            .unwrap();
        assert_eq!(
            before, after,
            "diagnostic validation must not publish MAPs or merges"
        );
        let away = strand_in(&storage, DEFAULT_SCOPE_ID, unrelated_fragment());
        let report = cognition.check().unwrap();
        assert!(
            report.notes.iter().any(|note| note.contains(&away)),
            "{:?}",
            report.notes
        );
        malformed_in(&storage, DEFAULT_SCOPE_ID);
        let error = cognition.check().unwrap_err();
        assert!(is_fault(&error), "{error:#}");
        storage.close().unwrap();
    }
}
