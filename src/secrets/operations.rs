//! Configured native operations over the standalone encrypted Secrets core.
use crate::clock;
use crate::secrets::{self, storage as secret_storage};
use crate::storage::{AcquiringReader, FacultySnapshot};
#[cfg(test)]
use crate::storage::open_secrets_collection_read;
use anyhow::{Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use faculties_secrets::resource::{DeliveryLimits, SecretTarget};
use std::path::PathBuf;
use triblespace::core::capability::CapabilityProofId;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::*;
use zeroize::Zeroizing;

#[derive(Clone, Debug)]
pub struct Secrets {
    storage: crate::storage::Storage,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretMetadata {
    pub id: Id,
    pub name: String,
}
impl Secrets {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self { storage }
    }
    fn storage(&self) -> SecretsStorage<'_> {
        SecretsStorage {
            storage: &self.storage,
        }
    }
    /// Encrypt resident bytes once, returning the exact newly authored version.
    pub fn add(&self, name: &str, plaintext: &[u8]) -> Result<Id> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = crate::storage::open_secrets_collection_acquiring(
                pile, signer.verifying_key(), runtime,
            )?;
            let secret = secret_storage::add_secret(
                pile,
                signer,
                collection,
                name,
                plaintext,
                clock::point_now()?,
            )?;
            drop(
                runtime.block_on(crate::storage::ensure_downstream(
                    pile,
                    collection.source(),
                    signer,
                ))
                .context("Encrypted secret was committed, but ensuring its derived views failed")?,
            );
            Ok(secret)
        })
    }
    /// Explicitly open one exact version. Callers decide how plaintext is used.
    pub fn get(&self, secret: Id) -> Result<Zeroizing<Vec<u8>>> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = crate::storage::open_secrets_collection_acquiring(
                pile, signer.verifying_key(), runtime,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            snapshot.open(secret, signer).map(Zeroizing::new)
        })
    }
    /// Metadata only. Listing never attempts plaintext decryption.
    pub fn list(&self) -> Result<Vec<SecretMetadata>> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = crate::storage::open_secrets_collection_acquiring(
                pile, signer.verifying_key(), runtime,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let Some(facts) = snapshot.facts() else {
                return Ok(Vec::new());
            };
            secrets::secret_rows(facts)
                .into_iter()
                .map(|row| {
                    Ok(SecretMetadata {
                        id: row.id,
                        name: secrets::read_text(snapshot.store_snapshot(), row.name)?,
                    })
                })
                .collect()
        })
    }
    /// Explicit key-delivery maintenance; does not grant new capabilities.
    pub fn maintain(&self) -> Result<usize> {
        self.maintain_selected(&[])
    }
    pub fn maintain_selected(&self, selected: &[SecretTarget]) -> Result<usize> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = crate::storage::open_secrets_collection_acquiring(
                pile, signer.verifying_key(), runtime,
            )?;
            let snapshot = acquiring_snapshot(pile, collection, signer, runtime, true)?;
            let count = secret_storage::maintain_selected_recipient_envelopes(
                pile,
                signer,
                &snapshot,
                collection,
                signer,
                selected,
                clock::now()?,
            )?;
            if count != 0 {
                drop(
                    runtime.block_on(crate::storage::ensure_downstream(
                        pile,
                        collection.source(),
                        signer,
                    ))
                    .context(
                        "Recipient envelopes were committed, but ensuring their derived views failed",
                    )?,
                );
            }
            Ok(count)
        })
    }
    /// Sign resource-specific delivery authority, without granting collection
    /// READ/WRITE and without delivering or exporting plaintext.
    pub fn grant(
        &self,
        target: SecretTarget,
        recipient: VerifyingKey,
        limits: DeliveryLimits,
        delegate: bool,
    ) -> Result<Vec<CapabilityProofId>> {
        self.storage().with_store(|pile, signer, runtime| {
            let collection = crate::storage::open_secrets_collection_acquiring(
                pile, signer.verifying_key(), runtime,
            )?;
            let snapshot = match target {
                SecretTarget::Resource(_) => {
                    secret_storage::snapshot_acquiring(
                        AcquiringReader::new(pile.snapshot()?, std::sync::Arc::clone(runtime)),
                        collection,
                    )?
                }
                SecretTarget::Secret(_) => acquiring_snapshot(
                    pile, collection, signer, runtime, false,
                )?,
            };
            secrets::resource::grant(pile, signer, &snapshot, target, recipient, limits, delegate)
        })
    }
}

#[derive(Clone, Copy)]
struct SecretsStorage<'a> {
    storage: &'a crate::storage::Storage,
}

impl SecretsStorage<'_> {
    fn with_store<T>(
        self,
        operation: impl FnOnce(
            &mut crate::storage::FacultyStore,
            &SigningKey,
            &std::sync::Arc<tokio::runtime::Runtime>,
        ) -> Result<T>,
    ) -> Result<T> {
        self.storage.with_store(operation)
    }
}

/// Preserve the ordinary local-wrap possession contract. The selected
/// encrypted facts and delivery proofs stay fixed while exact ciphertext,
/// envelope and definition bytes are acquired; no plaintext crosses the wire.
fn acquiring_snapshot(
    store: &mut crate::storage::FacultyStore,
    collection: secret_storage::SecretsCollection,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    maintain: bool,
) -> Result<secrets::SecretsSnapshot<AcquiringReader<FacultySnapshot>>> {
    let result = if maintain {
        runtime.block_on(collection.maintain(store, signer))
    } else {
        runtime.block_on(collection.ensure(store, signer))
    };
    let snapshot = match result {
        Ok(snapshot) => snapshot,
        Err(error) if !maintain && matches!(
            error.downcast_ref::<triblespace::core::collection::CollectionRealizationError>(),
            Some(triblespace::core::collection::CollectionRealizationError::HostMismatch { .. })
        ) => store.snapshot()?,
        Err(error) => return Err(error),
    };
    secret_storage::snapshot_acquiring(
        AcquiringReader::new(snapshot, std::sync::Arc::clone(runtime)), collection,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_added_secret_is_queryable_and_openable_through_an_ordinary_read() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("secrets.pile");
        std::fs::File::create(&path).unwrap();
        let signer = crate::storage::initialize_signer(&path, None).unwrap();
        let id = Secrets::new(path.clone(), None)
            .add("eager fixture", b"generated disposable secret")
            .unwrap();

        let mut pile = crate::storage::open_pile_strict(&path).unwrap();
        let collection = open_secrets_collection_read(&mut pile, signer.verifying_key()).unwrap();
        // The ordinary read path prepares the target, exactly as get/list do.
        let snapshot = pollster::block_on(secret_storage::ensure_and_snapshot(
            &mut pile, collection, &signer,
        ))
        .unwrap();
        assert!(secrets::secret_rows(snapshot.facts().unwrap())
            .iter()
            .any(|row| row.id == id));
        assert_eq!(
            snapshot.open(id, &signer).unwrap(),
            b"generated disposable secret"
        );
        drop(snapshot);
        pile.close().unwrap();
    }
}
