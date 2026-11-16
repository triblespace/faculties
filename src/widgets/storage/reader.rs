//! Two immutable native observation surfaces; neither owns a mutable store.
//!
//! The standalone viewer retains exact-body acquisition. Embedded hosts lend a
//! resident snapshot of their already-loaded pile. Missing native bytes stay
//! missing in that observation until the host explicitly refreshes it.

use std::collections::BTreeSet;
use triblespace::core::blob::{BlobEncoding, TryFromBlob};
use triblespace::core::capability::{CapabilityProof, CapabilityProofId};
use triblespace::core::collection::{
    CollectionHandle, CollectionRead, CollectionRecord, CollectionRecordSelector,
    CollectionRecordView, CoverageRead, RecordViewChanges,
};
use triblespace::core::collection::coverage::CoverageIndex;
use triblespace::core::inline::{Inline, InlineEncoding};
use triblespace::core::inline::encodings::hash::Handle;
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::core::repo::{
    BlobInfo, BlobMetadata, BlobStoreGet, BlobStoreList, BlobStoreMeta,
    CapabilityProofRead, StoreChanges, StoreDependencies,
    StoreSnapshot, WantRead, WantRequest,
};
use crate::storage::{AcquiringReader, FacultySnapshot};

/// Preserves the native error chain, including exact missing-blob identities.
#[derive(Debug)]
pub struct WidgetReadError(Box<dyn std::error::Error + Send + Sync>);
impl WidgetReadError {
    fn new(error: impl std::error::Error + Send + Sync + 'static) -> Self { Self(Box::new(error)) }
}
impl std::fmt::Display for WidgetReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
}
impl std::error::Error for WidgetReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.0.as_ref()) }
}

/// Frozen evidence used by every faculty widget, independent of store ownership.
#[derive(Clone)]
pub enum WidgetReader {
    Resident(PileSnapshot),
    Acquiring(AcquiringReader<FacultySnapshot>),
}

macro_rules! dispatch {
    ($self:expr, $reader:ident => $body:expr) => {
        match $self {
            WidgetReader::Resident($reader) => $body,
            WidgetReader::Acquiring($reader) => $body,
        }
    };
}

impl StoreSnapshot for WidgetReader {
    fn changes_since(&self, previous: &Self) -> StoreChanges {
        match (self, previous) {
            (Self::Resident(a), Self::Resident(b)) => a.changes_since(b),
            (Self::Acquiring(a), Self::Acquiring(b)) => a.changes_since(b),
            _ => StoreChanges::ALL,
        }
    }

    fn changes_for(&self, previous: &Self, dependencies: &StoreDependencies) -> StoreChanges {
        match (self, previous) {
            (Self::Resident(a), Self::Resident(b)) => a.changes_for(b, dependencies),
            (Self::Acquiring(a), Self::Acquiring(b)) => a.changes_for(b, dependencies),
            _ => StoreChanges::ALL,
        }
    }
}

impl BlobStoreGet for WidgetReader {
    type GetError<E: std::error::Error + Send + Sync + 'static> = WidgetReadError;

    fn get<T, E>(&self, handle: Inline<Handle<E>>) -> Result<T, Self::GetError<T::Error>>
    where E: BlobEncoding + 'static, T: TryFromBlob<E>, Handle<E>: InlineEncoding {
        dispatch!(self, reader => reader.get(handle).map_err(WidgetReadError::new))
    }
}

impl BlobStoreList for WidgetReader {
    type Err = WidgetReadError;
    type Iter<'a> = Box<dyn Iterator<Item = Result<BlobInfo, WidgetReadError>> + 'a>;

    fn blobs(&self) -> Self::Iter<'_> {
        dispatch!(self, reader => Box::new(reader.blobs().map(|value| value.map_err(WidgetReadError::new))))
    }

    fn contains_blob<E>(&self, handle: Inline<Handle<E>>) -> Result<bool, Self::Err>
    where E: BlobEncoding + 'static, Handle<E>: InlineEncoding {
        dispatch!(self, reader => reader.contains_blob(handle).map_err(WidgetReadError::new))
    }
}

impl BlobStoreMeta for WidgetReader {
    type MetaError = WidgetReadError;

    fn metadata<E>(&self, handle: Inline<Handle<E>>) -> Result<Option<BlobMetadata>, Self::MetaError>
    where E: BlobEncoding + 'static, Handle<E>: InlineEncoding {
        dispatch!(self, reader => reader.metadata(handle).map_err(WidgetReadError::new))
    }

    fn resident(
        &self,
        handles: &triblespace::core::patch::PATCH<32, triblespace::core::patch::IdentitySchema, ()>,
    ) -> Result<triblespace::core::patch::PATCH<32, triblespace::core::patch::IdentitySchema, ()>, Self::MetaError> {
        dispatch!(self, reader => reader.resident(handles).map_err(WidgetReadError::new))
    }
}

impl CapabilityProofRead for WidgetReader {
    type ProofsError = WidgetReadError;
    type ProofIter<'a> = Box<dyn Iterator<Item = Result<CapabilityProof, WidgetReadError>> + 'a>;

    fn proofs(&self) -> Result<Self::ProofIter<'_>, Self::ProofsError> {
        dispatch!(self, reader => {
            let proofs = reader.proofs().map_err(WidgetReadError::new)?;
            Ok(Box::new(proofs.map(|value| value.map_err(WidgetReadError::new))) as Self::ProofIter<'_>)
        })
    }

    fn proof(&self, id: CapabilityProofId) -> Result<Option<CapabilityProof>, Self::ProofsError> {
        dispatch!(self, reader => reader.proof(id).map_err(WidgetReadError::new))
    }
}

impl CollectionRead for WidgetReader {
    type RecordsError = WidgetReadError;
    type RecordIter<'a> = Box<dyn Iterator<Item = Result<CollectionRecord, WidgetReadError>> + 'a>;

    fn records(&self) -> Result<Self::RecordIter<'_>, Self::RecordsError> {
        dispatch!(self, reader => {
            let records = reader.records().map_err(WidgetReadError::new)?;
            Ok(Box::new(records.map(|value| value.map_err(WidgetReadError::new))) as Self::RecordIter<'_>)
        })
    }

    fn collections(&self) -> Result<Vec<CollectionHandle>, Self::RecordsError> {
        dispatch!(self, reader => reader.collections().map_err(WidgetReadError::new))
    }

    fn select_records(&self, selectors: &BTreeSet<CollectionRecordSelector>)
        -> Result<Vec<CollectionRecord>, Self::RecordsError> {
        dispatch!(self, reader => reader.select_records(selectors).map_err(WidgetReadError::new))
    }

    fn select_record_views(&self, selectors: &BTreeSet<CollectionRecordSelector>)
        -> Result<Vec<CollectionRecordView>, Self::RecordsError> {
        dispatch!(self, reader => reader.select_record_views(selectors).map_err(WidgetReadError::new))
    }

    fn select_record_view_changes(&self, previous: &Self, selectors: &BTreeSet<CollectionRecordSelector>)
        -> Result<RecordViewChanges, Self::RecordsError> {
        match (self, previous) {
            (Self::Resident(a), Self::Resident(b)) => a.select_record_view_changes(b, selectors).map_err(WidgetReadError::new),
            (Self::Acquiring(a), Self::Acquiring(b)) => a.select_record_view_changes(b, selectors).map_err(WidgetReadError::new),
            _ => Ok(RecordViewChanges::Rebuild),
        }
    }
}

impl CoverageRead for WidgetReader {
    fn index(&self, lineage: &BTreeSet<CollectionHandle>) -> Result<CoverageIndex, Self::RecordsError> {
        dispatch!(self, reader => reader.index(lineage).map_err(WidgetReadError::new))
    }
}

impl WantRead for WidgetReader {
    type WantsError = WidgetReadError;
    type WantIter<'a> = Box<dyn Iterator<Item = Result<WantRequest, WidgetReadError>> + 'a>;

    fn wants(&self) -> Result<Self::WantIter<'_>, Self::WantsError> {
        dispatch!(self, reader => {
            let wants = reader.wants().map_err(WidgetReadError::new)?;
            Ok(Box::new(wants.map(|value| value.map_err(WidgetReadError::new))) as Self::WantIter<'_>)
        })
    }
}
