//! The attached mappings faculties defines, and the pass that keeps every
//! such index current on this host.
//!
//! A maintainer dispatches an attached collection by its descriptor, and
//! for each representation knows one canonical mapping. The Archive's BM25
//! ([`ArchiveTextBm25Mapping`], algorithm [`ARCHIVE_TEXT_BM25_MAPPING_V3`])
//! is a `PortableBM25Blob` under its own algorithm, which that canonical
//! mapping refuses, so faculties hands it to a maintainer as a registration
//! ([`mappings`]): an application that owns the generic maintenance
//! Driver chains `driver.with_attached(faculties::maintenance::mappings())`.
//!
//! [`AttachedUpkeep`] is the pass `faculties maintain --declared` runs: it
//! asks the store which attached collections its records name whose
//! algorithm is registered ([`registered_targets`]: an index is named once
//! a search kept its images, or any maintenance attached a node), and
//! maintains each that lags its parent through the registration itself,
//! the same `maintain_attached_with` a Driver would call. No list and no
//! environment: using an index is what declares it, and this host
//! maintains what its own records name.

use crate::archive_bm25::{ArchiveTextBm25Mapping, ARCHIVE_TEXT_BM25_MAPPING_V3};
use crate::storage::Storage;
use anyhow::Result;
use std::time::Instant;
use triblespace::core::collection::{
    registered_targets, AttachedMapping, AttachedMappings, CollectionHandle,
};
use triblespace::core::repo::SnapshotSource;

/// The attached mappings faculties defines.
pub type Mappings = AttachedMapping<ArchiveTextBm25Mapping>;

/// The attached mappings faculties defines, as one registration.
pub fn mappings() -> Mappings {
    AttachedMapping::new(ARCHIVE_TEXT_BM25_MAPPING_V3)
}

/// The pass over the attached indexes faculties defines. It keeps nothing
/// between passes: each asks the store again.
pub struct AttachedUpkeep {
    mappings: Mappings,
}

impl Default for AttachedUpkeep {
    fn default() -> Self {
        Self::new()
    }
}

impl AttachedUpkeep {
    pub fn new() -> Self {
        Self {
            mappings: mappings(),
        }
    }

    /// The line a daemon running these passes starts with.
    pub fn describe() -> String {
        format!(
            "maintain: also every attached index the pile's records name whose mapping faculties defines (archive-text-bm25-v3 {ARCHIVE_TEXT_BM25_MAPPING_V3:X}), on the CPU"
        )
    }

    /// One pass: find the registered indexes the records name, and maintain
    /// each whose parent has foundations no attachment reaches. Returns the
    /// lines to log: one per index maintained or failed. Only a failure to
    /// read the store at all is an error.
    pub fn pass(&mut self, storage: &Storage) -> Result<Vec<String>> {
        let mappings = &self.mappings;
        storage.with_store(|store, signer, runtime| {
            let mut lines = Vec::new();
            let snapshot = store.snapshot()?;
            let targets = registered_targets(&snapshot, mappings)?;
            drop(snapshot);
            for (index, algorithm) in targets {
                let snapshot = store.snapshot()?;
                let lagging = match mappings.residual(&snapshot, index, algorithm) {
                    Ok(0) => continue,
                    Ok(lagging) => lagging,
                    Err(error) => {
                        lines.push(format!(
                            "maintain: attached index {} not read: {error}",
                            short(index)
                        ));
                        continue;
                    }
                };
                let started = Instant::now();
                let result = runtime.block_on(mappings.maintain(
                    store, &snapshot, index, algorithm, signer,
                ));
                drop(snapshot);
                lines.push(match result {
                    Ok(after) => {
                        let left = mappings
                            .residual(&after, index, algorithm)
                            .map_or_else(|error| format!("unknown ({error})"), |n| n.to_string());
                        format!(
                            "maintain: attached index {}: {lagging} lagging foundation(s) attached in {:.1} s, {left} left",
                            short(index),
                            started.elapsed().as_secs_f64()
                        )
                    }
                    Err(error) => format!(
                        "maintain: attached index {} failed: {error}",
                        short(index)
                    ),
                });
            }
            Ok(lines)
        })
    }
}

fn short(handle: CollectionHandle) -> String {
    hex::encode(&handle.raw[..8])
}
