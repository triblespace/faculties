//! The attached mappings faculties defines, and the pass that keeps every
//! such index current on this host.
//!
//! A maintainer dispatches an attached collection by its descriptor, and
//! for each representation knows one canonical mapping. The Archive's BM25
//! ([`ArchiveTextBm25Mapping`], algorithm [`ARCHIVE_TEXT_BM25_MAPPING_V3`])
//! is a `PortableBM25Blob` under its own algorithm, which that canonical
//! mapping refuses, so faculties hands it to a maintainer as a registration
//! ([`mappings`]): an application that owns the generic maintenance
//! Driver chains `driver.with_registered(faculties::maintenance::mappings())`.
//!
//! [`Upkeep`] is the pass `faculties maintain --declared` runs, over these
//! attached mappings and one more registration (the WeMM one in that daemon,
//! `crate::wemm::declared::Registration`; `()` for none). It asks the store
//! which collections its records name whose algorithm is registered
//! ([`registered_targets`]: an index is named once a search kept its images,
//! or any maintenance attached a node or derived a leaf), and maintains each
//! that has work through its registration, the same call an application's
//! Driver makes.
//! No list of indexes: using an index is what declares it, and this host
//! maintains what its own records name.

use crate::archive_bm25::{ArchiveTextBm25Mapping, ARCHIVE_TEXT_BM25_MAPPING_V3};
use crate::storage::Storage;
use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;
use triblespace::core::collection::{
    registered_targets, AttachedMapping, CollectionHandle, Mappings as _,
};
use triblespace::core::repo::SnapshotSource;

/// The attached mappings faculties defines.
pub type Mappings = AttachedMapping<ArchiveTextBm25Mapping>;

/// The attached mappings faculties defines, as one registration.
pub fn mappings() -> Mappings {
    AttachedMapping::new(ARCHIVE_TEXT_BM25_MAPPING_V3)
}

/// Where maintenance lines go: stderr in the daemon, a collector in tests,
/// an application's log.
pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

/// The line a daemon running [`Upkeep`] starts with, for the attached
/// indexes.
pub fn describe() -> String {
    format!(
        "maintain: also every attached index the pile's records name whose mapping faculties defines (archive-text-bm25-v3 {ARCHIVE_TEXT_BM25_MAPPING_V3:X}), on the CPU"
    )
}

/// The daemon's pass over the attached indexes faculties defines and the
/// collections `D` runs. It keeps nothing between passes but what `D` keeps
/// (a bound model, say): each asks the store again.
pub struct Upkeep<D = ()> {
    registered: (Mappings, D),
    sink: Sink,
}

impl<D: triblespace::core::collection::Mappings> Upkeep<D> {
    pub fn new(other: D, sink: Sink) -> Self {
        Self {
            registered: (mappings(), other),
            sink,
        }
    }

    /// The registration beside faculties' attached mappings.
    pub fn other(&self) -> &D {
        &self.registered.1
    }

    /// One pass: every registered index with work is maintained, in the
    /// order the records name them. Its lines go to the sink: an attached
    /// index's one per index maintained, the other registration's its own,
    /// and one per index that could not be read or failed. Returns when the
    /// next pass is due even if the pile does not grow. Only a failure to
    /// read the store at all is an error.
    pub fn pass(&mut self, storage: &Storage) -> Result<Option<Instant>> {
        let (registered, sink) = (&self.registered, &self.sink);
        storage.with_store(|store, signer, runtime| {
            let snapshot = store.snapshot()?;
            registered.begin_pass(&snapshot);
            let targets = registered_targets(&snapshot, registered)?;
            drop(snapshot);
            for (index, algorithm) in targets {
                let snapshot = store.snapshot()?;
                let attached = registered.0.runs(algorithm);
                let kind = if attached { "attached index" } else { "index" };
                let pending = match registered.pending(&snapshot, index, algorithm) {
                    Ok(0) => continue,
                    Ok(pending) => pending,
                    Err(error) => {
                        sink(&format!("maintain: {kind} {} not read: {error}", short(index)));
                        continue;
                    }
                };
                let started = Instant::now();
                let result = runtime.block_on(registered.maintain(
                    store, &snapshot, index, algorithm, signer,
                ));
                drop(snapshot);
                match result {
                    Ok(after) if attached => {
                        let left = registered
                            .pending(&after, index, algorithm)
                            .map_or_else(|error| format!("unknown ({error})"), |n| n.to_string());
                        sink(&format!(
                            "maintain: attached index {}: {pending} lagging foundation(s) attached in {:.1} s, {left} left",
                            short(index),
                            started.elapsed().as_secs_f64()
                        ));
                    }
                    Ok(_) => {}
                    Err(error) => sink(&format!("maintain: {kind} {} failed: {error}", short(index))),
                }
            }
            Ok(())
        })?;
        Ok(self.registered.due())
    }
}

fn short(handle: CollectionHandle) -> String {
    hex::encode(&handle.raw[..8])
}
