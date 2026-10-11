//! Operation-owned model reads through the same exact-acquisition boundary as
//! ordinary faculties. Model facts are frozen once; selected tensors keep their
//! native bytes and the existing GPU loaders decide how to consume them.

use std::path::Path;

use anyhow::{Context, Result};
#[cfg(any(feature = "local-embed", feature = "voice", feature = "imagine"))]
use {
    crate::storage::{AcquiringReader, FacultySnapshot},
    mary::model_collection::ModelSnapshot,
    std::sync::Arc,
    triblespace::core::repo::{SnapshotSource, StorageClose},
};

/// The shared model collection of the pile at `path`, frozen once: the exact
/// descriptor `TRIBLESPACE_COLLECTION_MODELS` names, read through a read-only
/// file descriptor of the local pile.
///
/// Local only, on purpose: a model that has not finished replicating here
/// fails to load rather than loading partly or from the network. Speech and
/// hearing copy every selected tensor to the device, so nothing stays aliased
/// to the pile once a model has loaded.
#[cfg(any(feature = "voice-breeze", feature = "discord-hearing"))]
pub(crate) fn models(path: &Path) -> Result<mary::model_collection::ModelPileSnapshot> {
    use crate::schemas::models::COLLECTION_SCOPE_ID;
    let handle =
        crate::collection_names::configured_handle(COLLECTION_SCOPE_ID)?.with_context(|| {
            format!(
                "{} must name the model collection's exact descriptor",
                crate::collection_names::override_env_name(COLLECTION_SCOPE_ID)
            )
        })?;
    let store = mary::persist::read_model_pile_snapshot_read_only(path)?;
    let collection = crate::collection_names::open_exact_in(&store, COLLECTION_SCOPE_ID, handle)
        .with_context(|| format!("open the model collection in {}", path.display()))?;
    mary::model_collection::snapshot_model_collection_for(&store, collection)
        .with_context(|| format!("freeze the model collection in {}", path.display()))
}

#[cfg(any(feature = "local-embed", feature = "voice", feature = "imagine"))]
pub(crate) fn with_snapshot<T>(
    path: &Path,
    model: &str,
    operation: impl FnOnce(&ModelSnapshot<AcquiringReader<FacultySnapshot>>) -> Result<T>,
) -> Result<T> {
    let runtime = Arc::new(crate::storage::runtime()?);
    let mut store = crate::storage::open_store(path)?;
    let result = (|| {
        let reader = AcquiringReader::new(store.snapshot()?, runtime);
        let snapshot = mary::model_collection::snapshot_model_collection_acquiring_in(&reader)
            .with_context(|| {
                format!(
                    "discover and freeze the native Mary collection for {model} in {}",
                    path.display()
                )
            })?;
        operation(&snapshot)
    })();
    let close = store.close().context("close model reader");
    match (result, close) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}
