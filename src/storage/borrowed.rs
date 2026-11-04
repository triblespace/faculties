//! A call borrows the application's existing store only for synchronous local
//! operations. Exact-byte acquisition uses its snapshot reader outside that
//! borrow. This owns neither a second peer nor a second set of indexes.

use super::*;
use anybytes::Bytes;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};
use triblespace::core::blob::{BlobEncoding, IntoBlob};
use triblespace::core::capability::CapabilityProof;
use triblespace::core::collection::CollectionStore;
use triblespace::core::inline::Inline;
use triblespace::core::repo::async_store::{AcquiringReadControl, AsyncBlobStoreGet};
use triblespace::core::repo::{BlobStorePut, CapabilityProofStore};

/// The existing Orient state machine also needs explicitly resident-only
/// health upkeep. That operation stays on the same Pile, with no network
/// await inside its short borrow.
pub(crate) trait FacultyAccess:
    Store + SnapshotSource<Snapshot = FacultySnapshot> + AsyncBlobStoreAcquire + Send
{
    fn with_local<T>(&mut self, operation: impl FnOnce(&mut Pile) -> Result<T>) -> Result<T>;

    fn read_control(&self) -> Option<AcquiringReadControl> { None }
}

impl FacultyAccess for FacultyStore {
    fn with_local<T>(&mut self, operation: impl FnOnce(&mut Pile) -> Result<T>) -> Result<T> {
        operation(&mut self.store())
    }
}

/// No selected view is retained here. Each capability uses the original
/// Storage owner; only the call's cancellation/deadline is shared.
pub(crate) struct BorrowedStore {
    storage: Storage,
    control: AcquiringReadControl,
    prepared: Arc<AtomicBool>,
    runtime: Arc<tokio::runtime::Runtime>,
    remote: bool,
}

#[derive(Debug)]
pub(crate) struct BorrowError(anyhow::Error);

impl std::fmt::Display for BorrowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.0) }
}
impl std::error::Error for BorrowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.0.as_ref()) }
}

impl BorrowedStore {
    pub(crate) fn open(storage: Storage, control: AcquiringReadControl, prepared: Arc<AtomicBool>)
        -> Result<(Self, SigningKey, Arc<tokio::runtime::Runtime>)>
    {
        let (signer, runtime, remote) = borrow(&storage, &control, |store, signer, runtime| {
            Ok((signer.clone(), runtime.clone(), store.acquires_remotely()))
        })?;
        Ok((Self { storage, control, prepared, runtime: runtime.clone(), remote }, signer, runtime))
    }

    fn control(&self) -> AcquiringReadControl {
        // Selection has one absolute deadline. Once its complete report is
        // owned by the caller, token admission/publication and Presented keep
        // the same cancellation but do not restart or expire that selection.
        if self.prepared.load(Ordering::Acquire) { self.control.without_deadline() }
        else { self.control.clone() }
    }

    fn local<T>(&self, operation: impl FnOnce(&mut FacultyStore) -> Result<T>) -> Result<T> {
        borrow(&self.storage, &self.control(), |store, _, _| operation(store))
    }
}

fn borrow<T>(
    storage: &Storage,
    control: &AcquiringReadControl,
    operation: impl FnOnce(&mut FacultyStore, &SigningKey, &Arc<tokio::runtime::Runtime>) -> Result<T>,
) -> Result<T> {
    let mut operation = Some(operation);
    loop {
        control.check()?;
        if let Some(value) = storage.try_with_store(|store, signer, runtime| {
            control.check()?;
            operation.take().expect("a borrow executes at most once")(store, signer, runtime)
        })? {
            return Ok(value);
        }
        // Called only by the owned blocking worker. A busy shared owner does
        // not trap cancellation behind Mutex::lock. Synchronous work already
        // executing in either owner remains cooperatively, not forcibly, bounded.
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl SnapshotSource for BorrowedStore {
    type Snapshot = FacultySnapshot;
    type SnapshotError = BorrowError;
    fn snapshot(&mut self) -> std::result::Result<FacultySnapshot, BorrowError> {
        self.local(|store| Ok(store.snapshot()?)).map_err(BorrowError)
    }
}

impl BlobStorePut for BorrowedStore {
    type PutError = BorrowError;
    fn put<S, T>(&mut self, item: T) -> std::result::Result<Inline<Handle<S>>, BorrowError>
    where S: BlobEncoding + 'static, T: IntoBlob<S>, Handle<S>: InlineEncoding,
    {
        self.local(|store| Ok(store.put(item)?)).map_err(BorrowError)
    }
}

impl CollectionStore for BorrowedStore {
    type InsertError = BorrowError;
    fn insert(&mut self, record: CollectionRecord) -> std::result::Result<(), BorrowError> {
        self.local(|store| Ok(CollectionStore::insert(store, record)?)).map_err(BorrowError)
    }
}

impl CapabilityProofStore for BorrowedStore {
    type InsertError = BorrowError;
    fn insert_proof(&mut self, proof: CapabilityProof) -> std::result::Result<(), BorrowError> {
        self.local(|store| Ok(store.insert_proof(proof)?)).map_err(BorrowError)
    }
}

impl AsyncBlobStoreAcquire for BorrowedStore {
    type AcquireError = BorrowError;
    async fn acquire(&mut self, handle: Inline<Handle<UnknownBlob>>)
        -> std::result::Result<Option<Bytes>, BorrowError>
    {
        let snapshot = self.snapshot()?;
        let reader = AcquiringReader::new(snapshot, self.runtime.clone())
            .with_control(self.control());
        match AsyncBlobStoreGet::get::<Bytes, UnknownBlob>(&reader, handle).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if triblespace::core::repo::is_missing_blob(&error) => Ok(None),
            Err(error) => Err(BorrowError(error.into())),
        }
    }

    fn acquires_remotely(&self) -> bool { self.remote }
}

impl FacultyAccess for BorrowedStore {
    fn with_local<T>(&mut self, operation: impl FnOnce(&mut Pile) -> Result<T>) -> Result<T> {
        self.local(|store| operation(&mut store.store()))
    }

    fn read_control(&self) -> Option<AcquiringReadControl> { Some(self.control()) }
}
