//! Operation-owned model reads through the same exact-acquisition boundary as
//! ordinary faculties. Model facts are frozen once; selected tensors keep their
//! native bytes and the existing GPU loaders decide how to consume them.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use mary::model_collection::ModelSnapshot;
use triblespace::core::repo::{SnapshotSource, StorageClose};

use crate::storage::{AcquiringReader, FacultySnapshot};

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
            .with_context(|| format!(
                "discover and freeze the native Mary collection for {model} in {}",
                path.display()
            ))?;
        operation(&snapshot)
    })();
    let close = store.close().context("close model reader");
    match (result, close) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}
