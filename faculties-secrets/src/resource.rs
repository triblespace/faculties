//! Secret-specific authority carried by an immutable resource descriptor.
//!
//! Collection facts declare which resource a version uses, but never choose the
//! delivery policy for a recovered DEK: the opened envelope supplies R. A new R
//! names no collection; the grants over it reach every participant of R by
//! grant gossip, whichever collection declares the version.

use std::num::NonZeroUsize;

use triblespace::core::capability::policy::{
    resource_handle, resource_policies, resource_policy, AdmissionPolicy,
};
use triblespace::core::capability::{
    capability_action, capability_delegate_action, capability_quorum_authorized_subjects_if,
    CapabilityHandle, CapabilityProof, CapabilityProofId, CapabilityProofPrefix, CapabilityRequest,
    CapabilityResource,
};
use triblespace::core::repo::{
    BlobStorePut, CapabilityProofRead, CapabilityProofStore, SnapshotSource,
};

use super::*;
use crate::schema::{
    delivery_expires_at, delivery_not_before, ACTION_KEY_DELIVERY, KIND_SECRET_RESOURCE,
};

/// The retired routing backlink, which versions sealed before grant gossip
/// carry and nothing reads: declared for tests that write such an old R. It is
/// an anchored attribute, so anchor and encoding are its identity and this
/// declaration has the published attribute's id.
#[cfg(test)]
pub(crate) mod retired {
    use triblespace::macros::attributes;
    use triblespace::prelude::*;

    attributes! {
        "62E951EC3ABB7F9B7F123BB7DE2F9F99" as pub resource_collection: inlineencodings::Handle<blobencodings::SimpleArchive>;
    }
}

/// The selected immutable version, or its exact resource descriptor handle.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SecretTarget {
    Secret(Id),
    Resource(CollectionHandle),
}

/// Restrictions on future delivery, not on possession of an existing envelope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeliveryLimits {
    pub not_before: Option<Epoch>,
    pub expires_at: Option<Epoch>,
}

impl DeliveryLimits {
    pub fn validate(self) -> Result<()> {
        if let (Some(start), Some(end)) = (self.not_before, self.expires_at) {
            if start >= end {
                bail!("delivery expiry must be later than not-before");
            }
        }
        Ok(())
    }

    pub fn definition(self, delegate: bool) -> Result<Fragment> {
        self.validate()?;
        let point = |epoch: Epoch| -> Result<IntervalValue> {
            (epoch, epoch)
                .try_to_inline()
                .map_err(|error| anyhow!("encode delivery instant: {error:?}"))
        };
        Ok(entity! {
            capability_action: ACTION_KEY_DELIVERY,
            capability_delegate_action?: delegate.then_some(ACTION_KEY_DELIVERY),
            delivery_not_before?: self.not_before.map(point).transpose()?,
            delivery_expires_at?: self.expires_at.map(point).transpose()?,
        })
    }
}

/// Interpret every restriction in every ancestor of this exact signed prefix.
/// Repeated restrictions intersect; unknown facts do not invalidate a definition.
pub fn delivery_prefix_is_current<R: BlobStoreGet>(
    reader: &R,
    prefix: CapabilityProofPrefix<'_>,
    now: Epoch,
) -> bool {
    let now = now.to_tai_duration().total_nanoseconds();
    prefix.capabilities().all(|handle| {
        let Ok(facts) = reader.get::<TribleSet, _>(handle) else {
            return false;
        };
        let starts = find!(
            value: IntervalValue,
            pattern!(&facts, [{ delivery_not_before: ?value }])
        );
        let ends = find!(
            value: IntervalValue,
            pattern!(&facts, [{ delivery_expires_at: ?value }])
        );
        starts.into_iter().all(|value| {
            value
                .try_from_inline::<(i128, i128)>()
                .is_ok_and(|(lower, upper)| lower == upper && now >= lower)
        }) && ends.into_iter().all(|value| {
            value
                .try_from_inline::<(i128, i128)>()
                .is_ok_and(|(lower, upper)| lower == upper && now < upper)
        })
    })
}

/// Prepare one bound raw secret and its signer-only envelope without publishing.
/// The immutable resource binds the opaque secret id and exact body.
/// Callers may compose its fragment with dependent facts before one commit.
pub fn seal_version(
    signer: &SigningKey,
    name: &str,
    plaintext: &[u8],
    created_at: IntervalValue,
) -> Result<SealedVersion> {
    validate_name("secret name", name)?;
    point_value("secret creation time", created_at)?;
    let secret = genid().id;
    let dek = Key::gen();
    let nonce = Nonce::gen();
    let mut body = nonce.to_vec();
    body.extend_from_slice(&DryocSecretBox::encrypt_to_vecbox(plaintext, &nonce, &dek).to_vec());
    let mut fragment = Fragment::empty();
    let body = fragment.put::<blobencodings::RawBytes, _>(body);
    let name = fragment.put(name.to_owned());
    fragment += secret_record(secret, name, body, created_at);

    // Preserve the published capability definition bytes. The policy belongs
    // inside this descriptor, not in appendable facts about the secret entity.
    let definition = key_delivery_definition();
    let capability = fragment.put::<blobencodings::SimpleArchive, _>(definition.facts().clone());
    fragment += definition;
    let descriptor = entity! {
        metadata::tag: KIND_SECRET_RESOURCE,
        wrap_secret: secret,
        secret_body: body,
        resource_policy*: AdmissionPolicy::direct(signer.verifying_key()).binding(capability),
    };
    let resource = fragment.put::<blobencodings::SimpleArchive, _>(descriptor.facts().clone());
    fragment += entity! { ExclusiveId::force_ref(&secret) @ resource_handle: resource };
    let binding = envelope::BoundKey {
        secret,
        body,
        resource,
        dek,
    };
    fragment += envelope::fragment(&binding, signer.verifying_key().to_bytes())?;
    Ok(SealedVersion {
        fragment,
        secret,
        recipients: vec![signer.verifying_key()],
    })
}

/// Same immutable S/H/R/DEK envelope contract, with a distinct typed body.
pub(crate) fn seal_item_bundle(
    signer: &SigningKey,
    name: &str,
    key: crate::encrypted::ItemKey,
    bundle: Bundle<crate::encrypted::Encrypted<SimpleArchive>>,
    created_at: IntervalValue,
) -> Result<SealedVersion> {
    use crate::schema::{structured_body, KIND_ITEM_VERSION};
    let secret = key.id();
    let body = bundle.root();
    let mut fragment = entity! { ExclusiveId::force_ref(&secret) @
        metadata::tag: KIND_ITEM_VERSION,
        metadata::name: name.to_owned(),
        metadata::created_at: created_at,
        structured_body*: bundle,
    };
    let definition = key_delivery_definition();
    let capability = fragment.put::<SimpleArchive, _>(definition.facts().clone());
    fragment += definition;
    let descriptor = entity! {
        metadata::tag: KIND_SECRET_RESOURCE,
        wrap_secret: secret,
        structured_body: body,
        resource_policy*: AdmissionPolicy::direct(signer.verifying_key()).binding(capability),
    };
    let resource = fragment.put::<SimpleArchive, _>(descriptor.facts().clone());
    fragment += entity! { ExclusiveId::force_ref(&secret) @ resource_handle: resource };
    fragment += envelope::fragment(
        &envelope::BoundKey {
            secret,
            body: body.transmute(),
            resource,
            dek: key.into_dek(),
        },
        signer.verifying_key().to_bytes(),
    )?;
    Ok(SealedVersion {
        fragment,
        secret,
        recipients: vec![signer.verifying_key()],
    })
}

/// Delivery is decided by R's own policy and the proofs over R, never by which
/// collection declares the version, so a version carried into another
/// collection keeps its delivery policy there. The policy grants delivery for
/// R, not access to any collection. An old R may still carry the retired
/// `resource_collection` annotation; nothing reads it.
fn policies<R: BlobStoreGet>(
    reader: &R,
    facts: &TribleSet,
    bound: Option<(Id, BytesHandle)>,
) -> Vec<AdmissionPolicy> {
    let raw = find!(
        (entity: Id, secret: Id, body: BytesHandle),
        pattern!(facts, [{
            ?entity @ metadata::tag: KIND_SECRET_RESOURCE,
            wrap_secret: ?secret,
            secret_body: ?body,
        }])
    );
    let structured = find!(
        (entity: Id, secret: Id, body: crate::item::ItemBody),
        pattern!(facts, [{ ?entity @ metadata::tag: KIND_SECRET_RESOURCE,
            wrap_secret: ?secret, crate::schema::structured_body: ?body }])
    )
    .map(|(entity, secret, body)| (entity, secret, body.transmute()));
    let candidates = raw.chain(structured);
    let mut policies = Vec::new();
    for (entity, secret, body) in candidates {
        if bound.is_some_and(|expected| expected != (secret, body)) {
            continue;
        }
        for (definition, policy) in resource_policies(facts, entity, None) {
            let Ok(definition) = reader.get::<TribleSet, _>(definition) else {
                continue;
            };
            if exists!(pattern!(&definition, [{ capability_action: ACTION_KEY_DELIVERY }])) {
                policies.push(policy);
            }
        }
    }
    policies
}

pub(crate) fn recipients<R>(
    reader: &R,
    binding: &envelope::BoundKey,
    now: Epoch,
) -> Result<Vec<VerifyingKey>>
where
    R: BlobStoreGet + CapabilityProofRead,
{
    let Ok(facts) = reader.get::<TribleSet, _>(binding.resource) else {
        return Ok(Vec::new());
    };
    let proofs = reader
        .proofs()
        .map_err(|error| anyhow!("read delivery proofs: {error}"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| anyhow!("read delivery proof: {error}"))?;
    let mut recipients = BTreeSet::new();
    for policy in policies(reader, &facts, Some((binding.secret, binding.body))) {
        let AdmissionPolicy::Quorum(quorum) = policy else {
            // An open policy cannot enumerate public keys for finite envelopes.
            continue;
        };
        recipients.extend(
            capability_quorum_authorized_subjects_if(
                reader,
                proofs.iter(),
                quorum.roots().iter().copied(),
                CapabilityRequest::new(
                    CapabilityResource::from(binding.resource),
                    ACTION_KEY_DELIVERY,
                ),
                NonZeroUsize::new(quorum.invoke_threshold() as usize).expect("validated quorum"),
                |prefix| delivery_prefix_is_current(reader, prefix, now),
            )
            .into_iter()
            .map(|key| key.to_bytes()),
        );
    }
    Ok(recipients
        .into_iter()
        .map(|bytes| VerifyingKey::from_bytes(&bytes).expect("validated key"))
        .collect())
}

/// Whether this holder has an authenticated envelope for exactly S/H/R and can
/// open that body. This reads possession evidence only: it grants no admission,
/// key delivery or onward delegation and does not infer R from public edges.
pub fn holder_has_binding<R: BlobStoreGet, P: TriblePattern>(
    reader: &R,
    facts: &P,
    secret: Id,
    body: BytesHandle,
    resource: CollectionHandle,
    holder: &SigningKey,
) -> Result<bool> {
    Ok(envelope::recover_all(reader, facts, secret, holder)?
        .into_iter()
        .any(|binding| binding.body == body && binding.resource == resource))
}

/// Open exactly one authenticated raw S/H/R binding held by this key. Other
/// raw bodies under S neither replace nor veto this explicit selection. This
/// is possession-based reading, not an admission or key-delivery grant.
pub fn open_bound_raw<R: BlobStoreGet, P: TriblePattern>(
    reader: &R,
    facts: &P,
    secret: Id,
    body: BytesHandle,
    resource: CollectionHandle,
    holder: &SigningKey,
) -> Result<Zeroizing<Vec<u8>>> {
    let binding = envelope::recover(reader, facts, secret, holder)?
        .into_iter()
        .find(|binding| binding.body == body && binding.resource == resource)
        .ok_or_else(|| anyhow!("no usable exact raw body/resource binding for this holder"))?;
    envelope::decrypt_body(reader, binding.body, &binding.dek)
}

pub fn grant<S, R>(
    store: &mut S,
    signer: &SigningKey,
    secrets: &SecretsSnapshot<R>,
    target: SecretTarget,
    recipient: VerifyingKey,
    limits: DeliveryLimits,
    delegate: bool,
) -> Result<Vec<CapabilityProofId>>
where
    S: BlobStorePut + CapabilityProofStore + SnapshotSource,
    S::Snapshot: BlobStoreGet,
    R: BlobStoreGet + CapabilityProofRead,
{
    if recipient.is_weak() || recipient.to_edwards().compress().to_bytes() != recipient.to_bytes() {
        bail!("recipient must be a canonical, non-weak Ed25519 key");
    }
    let resources = match target {
        SecretTarget::Resource(resource) => BTreeSet::from([(resource, None)]),
        SecretTarget::Secret(secret) => {
            let facts = secrets
                .facts()
                .ok_or_else(|| anyhow!("secret {secret} not found"))?;
            envelope::recover_all(secrets.store_snapshot(), facts, secret, signer)?
                .into_iter()
                .map(|binding| (binding.resource, Some((binding.secret, binding.body))))
                .collect()
        }
    };
    if resources.is_empty() {
        bail!("no bound resource for this holder; legacy envelopes remain readable but do not infer new authority");
    }
    let definition = limits.definition(delegate)?;
    let capability: CapabilityHandle = store
        .put(definition.facts().clone())
        .map_err(|error| anyhow!("store delivery capability definition: {error}"))?;
    let reader = store
        .snapshot()
        .map_err(|error| anyhow!("freeze delivery grant snapshot: {error}"))?;
    let existing = secrets
        .store_snapshot()
        .proofs()
        .map_err(|error| anyhow!("read delivery proofs: {error}"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| anyhow!("read delivery proof: {error}"))?;
    let mut issued = Vec::new();
    for (resource, bound) in resources {
        let facts: TribleSet = match reader.get(resource) {
            Ok(facts) => facts,
            Err(_) if bound.is_some() => continue,
            Err(error) => return Err(error).context("read selected secret resource"),
        };
        // Anybody can seal their own DEK/body to this signer and name an
        // unrelated R in that envelope. Grant-by-secret must preserve the
        // recovered S/H binding; only an explicit R target omits that check.
        let roots = policies(&reader, &facts, bound)
            .into_iter()
            .filter_map(|policy| match policy {
                AdmissionPolicy::Quorum(quorum) => Some(quorum),
                _ => None,
            })
            .flat_map(|quorum| quorum.roots().to_vec())
            .map(|key| key.to_bytes())
            .collect::<BTreeSet<_>>();
        let request =
            CapabilityRequest::new(CapabilityResource::from(resource), ACTION_KEY_DELIVERY);
        if roots.contains(&signer.verifying_key().to_bytes()) {
            issued.push(CapabilityProof::new(
                request.resource(),
                signer,
                capability,
                recipient,
            ));
        }
        for proof in &existing {
            if proof.resource() != request.resource()
                || !roots.contains(&proof.root_key().to_bytes())
            {
                continue;
            }
            // A valid earlier delegation remains usable even when a later suffix
            // names somebody else. Prefix slicing preserves its original bytes.
            for prefix in proof
                .prefixes()
                .filter(|prefix| prefix.subject() == signer.verifying_key())
            {
                let Ok(parent) = CapabilityProof::from_bytes(prefix.as_bytes()) else {
                    continue;
                };
                let Ok(candidate) = parent.delegate(signer, capability, recipient) else {
                    continue;
                };
                if candidate.validate_delegation(&reader).is_ok() {
                    issued.push(candidate);
                }
            }
        }
    }
    drop(reader);
    if issued.is_empty() {
        bail!("signer has no delegation authority for the selected secret resource");
    }
    let mut ids = BTreeSet::new();
    for proof in issued {
        if ids.insert(proof.id()) {
            store
                .insert_proof(proof)
                .map_err(|error| anyhow!("publish delivery proof: {error}"))?;
        }
    }
    Ok(ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::retired::resource_collection;
    use triblespace::core::collection::{
        grant_collection_write, CollectionPolicy, CollectionStoreExt,
    };
    use triblespace::core::repo::memoryrepo::MemoryRepo;

    use super::*;

    #[test]
    fn grant_by_secret_cannot_be_redirected_by_a_valid_attacker_envelope() {
        let alice = SigningKey::from_bytes(&[1; 32]);
        let bob = SigningKey::from_bytes(&[2; 32]);
        let attacker = SigningKey::from_bytes(&[3; 32]);
        let mut store = MemoryRepo::for_host(alice.verifying_key());
        let collection = storage::SecretsCollection::register(
            &mut store,
            "binding",
            CollectionPolicy::new(
                AdmissionPolicy::Open,
                AdmissionPolicy::direct(alice.verifying_key()),
            ),
        )
        .unwrap();
        let now = Epoch::from_unix_seconds(1.0);
        let created_at = (now, now).try_to_inline().unwrap();
        let first = storage::add_secret(
            &mut store,
            &alice,
            collection,
            "first",
            b"first value",
            created_at,
        )
        .unwrap();
        let second = storage::add_secret(
            &mut store,
            &alice,
            collection,
            "second",
            b"second value",
            created_at,
        )
        .unwrap();
        let original =
            pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice))
                .unwrap();
        let resource_of = |secret| {
            envelope::recover(
                original.store_snapshot(),
                original.facts().unwrap(),
                secret,
                &alice,
            )
            .unwrap()
            .remove(0)
            .resource
        };
        let first_resource = resource_of(first);
        let second_resource = resource_of(second);
        assert_ne!(first_resource, second_resource);

        // The attacker may write the source; its commit then reaches the
        // attached encodings like any other.
        grant_collection_write(
            &mut store,
            collection.handle(),
            &alice,
            attacker.verifying_key(),
        )
        .unwrap();
        // The attacker knows neither genuine DEK. Their own valid body and
        // envelope bind S1 to R2, exploiting public-key sealing rather than
        // forging an authenticator or substituting an immutable descriptor.
        let dek = Key::gen();
        let nonce = Nonce::gen();
        let mut ciphertext = nonce.to_vec();
        ciphertext.extend_from_slice(
            &DryocSecretBox::encrypt_to_vecbox(b"attacker value", &nonce, &dek).to_vec(),
        );
        let mut attack = Fragment::empty();
        let body = attack.put::<blobencodings::RawBytes, _>(ciphertext);
        attack += envelope::fragment(
            &envelope::BoundKey {
                secret: first,
                body,
                resource: second_resource,
                dek,
            },
            alice.verifying_key().to_bytes(),
        )
        .unwrap();
        store
            .commit(collection.source(), &attacker, attack)
            .unwrap();
        drop(
            pollster::block_on(storage::ensure_and_snapshot(
                &mut store, collection, &attacker,
            ))
            .unwrap(),
        );
        let selected =
            pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice))
                .unwrap();
        assert!(envelope::recover(
            selected.store_snapshot(),
            selected.facts().unwrap(),
            first,
            &alice,
        )
        .unwrap()
        .iter()
        .any(|binding| binding.resource == second_resource));

        let issued = grant(
            &mut store,
            &alice,
            &selected,
            SecretTarget::Secret(first),
            bob.verifying_key(),
            DeliveryLimits::default(),
            false,
        )
        .unwrap();
        assert_eq!(issued.len(), 1);
        let after_grant = store.snapshot().unwrap();
        let proofs = after_grant
            .proofs()
            .unwrap()
            .map(|proof| proof.unwrap())
            .collect::<Vec<_>>();
        assert!(proofs.iter().any(|proof| {
            issued.contains(&proof.id())
                && proof.resource() == CapabilityResource::from(first_resource)
                && proof.leaf_key() == bob.verifying_key()
        }));
        assert!(!proofs.iter().any(|proof| {
            proof.resource() == CapabilityResource::from(second_resource)
                && proof.leaf_key() == bob.verifying_key()
        }));
        let current = storage::snapshot(after_grant, collection).unwrap();
        assert_eq!(
            storage::maintain_recipient_envelopes(
                &mut store,
                &alice,
                &current,
                collection,
                &alice,
                Epoch::from_unix_seconds(100.0),
            )
            .unwrap(),
            1
        );
        let delivered =
            pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice))
                .unwrap();
        assert_eq!(delivered.open(first, &bob).unwrap(), b"first value");
        assert!(delivered.open(second, &bob).is_err());

        // Naming R2 explicitly is still a valid independent grant, without
        // requiring a bound envelope to be used as a selector for that call.
        assert_eq!(
            grant(
                &mut store,
                &alice,
                &delivered,
                SecretTarget::Resource(second_resource),
                bob.verifying_key(),
                DeliveryLimits::default(),
                false,
            )
            .unwrap()
            .len(),
            1
        );
        let current = storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert_eq!(
            storage::maintain_selected_recipient_envelopes(
                &mut store,
                &alice,
                &current,
                collection,
                &alice,
                &[SecretTarget::Resource(second_resource)],
                Epoch::from_unix_seconds(100.0),
            )
            .unwrap(),
            1
        );
        let delivered =
            pollster::block_on(storage::ensure_and_snapshot(&mut store, collection, &alice))
                .unwrap();
        assert_eq!(delivered.open(second, &bob).unwrap(), b"second value");
    }

    #[test]
    fn deadlines_filter_each_root_share_before_quorum_and_do_not_revoke_earlier_prefixes() {
        let alice = SigningKey::from_bytes(&[1; 32]);
        let anna = SigningKey::from_bytes(&[2; 32]);
        let bob = SigningKey::from_bytes(&[3; 32]);
        let carol = SigningKey::from_bytes(&[4; 32]);
        // An old R's retired home annotation changes nothing: not R's
        // audience, not its proofs' reach.
        for home in [
            Some(CollectionHandle::new([5; 32])),
            Some(CollectionHandle::new([7; 32])),
            None,
        ] {
            let secret = genid().id;
            let body = BytesHandle::new([6; 32]);
            let mut store = MemoryRepo::default();
            let definition = store
                .put::<blobencodings::SimpleArchive, _>(key_delivery_definition().facts().clone())
                .unwrap();
            let descriptor = entity! {
                metadata::tag: KIND_SECRET_RESOURCE,
                wrap_secret: secret,
                secret_body: body,
                resource_collection?: home,
                resource_policy*: AdmissionPolicy::quorum([alice.verifying_key(), anna.verifying_key()], 2, None).unwrap().binding(definition),
            };
            let resource = store
                .put::<blobencodings::SimpleArchive, _>(descriptor.facts().clone())
                .unwrap();
            let binding = envelope::BoundKey {
                secret,
                body,
                resource,
                dek: Key::gen(),
            };
            let parent = store
                .put::<blobencodings::SimpleArchive, _>(
                    DeliveryLimits {
                        not_before: None,
                        expires_at: Some(Epoch::from_unix_seconds(150.0)),
                    }
                    .definition(true)
                    .unwrap()
                    .facts()
                    .clone(),
                )
                .unwrap();
            let early_child = store
                .put::<blobencodings::SimpleArchive, _>(
                    DeliveryLimits {
                        not_before: None,
                        expires_at: Some(Epoch::from_unix_seconds(50.0)),
                    }
                    .definition(false)
                    .unwrap()
                    .facts()
                    .clone(),
                )
                .unwrap();
            let unbounded_child = store
                .put::<blobencodings::SimpleArchive, _>(
                    DeliveryLimits::default()
                        .definition(false)
                        .unwrap()
                        .facts()
                        .clone(),
                )
                .unwrap();
            let resource_id = CapabilityResource::from(resource);
            // Only the extended proofs are stored. Their Bob prefixes stay valid
            // at t=100 even though one Carol suffix has expired.
            store
                .insert_proof(
                    CapabilityProof::new(resource_id, &alice, parent, bob.verifying_key())
                        .delegate(&bob, early_child, carol.verifying_key())
                        .unwrap(),
                )
                .unwrap();
            store
                .insert_proof(
                    CapabilityProof::new(resource_id, &anna, parent, bob.verifying_key())
                        .delegate(&bob, unbounded_child, carol.verifying_key())
                        .unwrap(),
                )
                .unwrap();
            let current = store.snapshot().unwrap();
            assert_eq!(
                recipients(&current, &binding, Epoch::from_unix_seconds(100.0)).unwrap(),
                vec![bob.verifying_key()]
            );
            // One immutable evidence view supports different delivery instants;
            // changing evaluation time does not need a new storage observation.
            assert!(
                recipients(&current, &binding, Epoch::from_unix_seconds(150.0))
                    .unwrap()
                    .is_empty()
            );
            let audience = recipients(&current, &binding, Epoch::from_unix_seconds(40.0)).unwrap();
            assert!(audience.contains(&bob.verifying_key()));
            assert!(audience.contains(&carol.verifying_key()));
        }
    }

    #[test]
    fn invalid_delivery_window_is_rejected_before_definition_construction() {
        let instant = Epoch::from_unix_seconds(1.0);
        assert!(DeliveryLimits {
            not_before: Some(instant),
            expires_at: Some(instant)
        }
        .definition(false)
        .is_err());
    }

    #[test]
    fn exact_raw_binding_opens_one_valid_body_without_competing_body_veto() {
        use crate::storage::{self, SecretsCollection};
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        let alice = SigningKey::from_bytes(&[81; 32]);
        let bob = SigningKey::from_bytes(&[82; 32]);
        let mut store = MemoryRepo::for_host(alice.verifying_key());
        let collection = SecretsCollection::register(
            &mut store,
            "exact raw synthetic fixture",
            CollectionPolicy::new(
                AdmissionPolicy::Open,
                AdmissionPolicy::direct(alice.verifying_key()),
            ),
        )
        .unwrap();
        let at = {
            let epoch = Epoch::from_unix_seconds(1.0);
            (epoch, epoch).try_to_inline().unwrap()
        };
        let first = storage::add_secret(
            &mut store,
            &alice,
            collection,
            "first public",
            b"synthetic first",
            at,
        )
        .unwrap();
        let second = storage::add_secret(
            &mut store,
            &alice,
            collection,
            "second public",
            b"synthetic second",
            at,
        )
        .unwrap();
        let observed = storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        let original = envelope::recover(
            observed.store_snapshot(),
            observed.facts().unwrap(),
            first,
            &alice,
        )
        .unwrap()
        .remove(0);
        let second = envelope::recover(
            observed.store_snapshot(),
            observed.facts().unwrap(),
            second,
            &alice,
        )
        .unwrap()
        .remove(0);
        let mut fragment = entity! { ExclusiveId::force_ref(&first) @ metadata::tag: KIND_SECRET,
        metadata::name: "competing public", metadata::created_at: at, secret_body: second.body };
        let capability = fragment.put::<SimpleArchive, _>(key_delivery_definition().into_facts());
        let descriptor = entity! { metadata::tag: KIND_SECRET_RESOURCE,
        wrap_secret: first, secret_body: second.body, resource_collection: collection.handle(),
        resource_policy*: AdmissionPolicy::direct(alice.verifying_key()).binding(capability) };
        let competing_resource = fragment.put::<SimpleArchive, _>(descriptor.into_facts());
        fragment +=
            entity! { ExclusiveId::force_ref(&first) @ resource_handle: competing_resource };
        fragment += envelope::fragment(
            &envelope::BoundKey {
                secret: first,
                body: second.body,
                resource: competing_resource,
                dek: second.dek,
            },
            alice.verifying_key().to_bytes(),
        )
        .unwrap();
        store.commit(collection.source(), &alice, fragment).unwrap();
        let observed = storage::snapshot(store.snapshot().unwrap(), collection).unwrap();
        assert!(
            observed.open(first, &alice).is_err(),
            "old raw open still reports competing values"
        );
        assert_eq!(
            &*open_bound_raw(
                observed.store_snapshot(),
                observed.facts().unwrap(),
                first,
                original.body,
                original.resource,
                &alice
            )
            .unwrap(),
            b"synthetic first"
        );
        assert_eq!(
            &*open_bound_raw(
                observed.store_snapshot(),
                observed.facts().unwrap(),
                first,
                second.body,
                competing_resource,
                &alice
            )
            .unwrap(),
            b"synthetic second"
        );
        assert!(open_bound_raw(
            observed.store_snapshot(),
            observed.facts().unwrap(),
            first,
            original.body,
            competing_resource,
            &alice
        )
        .is_err());
        assert!(open_bound_raw(
            observed.store_snapshot(),
            observed.facts().unwrap(),
            first,
            original.body,
            original.resource,
            &bob
        )
        .is_err());
    }
}
