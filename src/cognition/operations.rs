//! Whole-collection Cognition validation as a direct library operation.
//! Event authoring remains in the existing shared publication API.

use crate::storage::FactRead;
use std::path::PathBuf;

use anyhow::{Context, Result};
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::CollectionStoreExt;
use triblespace::core::repo::SnapshotSource;

use crate::collection_names::open_configured;
use crate::schemas::cognition::DEFAULT_SCOPE_ID;
use crate::storage::Storage;

#[derive(Clone, Debug)]
pub struct Cognition {
    storage: Storage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckReport {
    pub facts: usize,
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
        self.storage.with_pile(|pile, signer| {
            let source = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
            let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
            let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
            let snapshot = pile
                .snapshot()
                .context("freeze resident Cognition fact collection")?;
            let facts = snapshot
                .read_facts(rank9)
                .context("read Cognition Rank9 collection")?;
            super::validate_archive(&snapshot, &facts)?;
            Ok(CheckReport {
                facts: facts.iter().count(),
            })
        })
    }
}
