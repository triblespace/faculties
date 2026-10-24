//! User-held envelopes. The node signs publication but receives no DEK wrap.
//! These are independent of ordinary recipient delivery; no unlock is persisted.

use super::*;
use crate::encrypted::{open_blob, Encrypted, ItemKey};
use crate::schema::{protected_envelope, structured_body, KIND_ITEM, KIND_ITEM_VERSION};
use chacha20poly1305::{
    aead::{Aead, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use dryoc::classic::crypto_pwhash::{crypto_pwhash, PasswordHashAlgorithm};
use triblespace::core::capability::policy::resource_handle;

// Exact installed trible genid outputs, 2026-10-23.
const FORMAT: Id = id_hex!("B071EBD51A3CF3474B6F5C0166CEF54D");
const RESOURCE: Id = id_hex!("BB3B50C0EB9FE11E8DD911F76FD2C738");
const BINDING: usize = 104;
const FIXED: usize = 172;
const MAX_DEVICE: usize = 16 * 1024;

pub struct DeviceProtection<'a> {
    pub key_id: &'a [u8],
    pub seal: &'a dyn Fn(&[u8]) -> Result<Vec<u8>>,
}

pub struct Protection<'a> {
    pub password: &'a [u8],
    pub device: Option<DeviceProtection<'a>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Purpose {
    Get,
    OpenFacts,
    Inspect,
    ExportText { child: [u8; 32] },
    ExportBytes { child: [u8; 32] },
    Totp { child: [u8; 32] },
}

/// Detached ciphertext and presentation metadata, not a live store or key.
#[derive(Clone)]
pub struct ProtectedAccess {
    request_nonce: [u8; 16],
    collection: CollectionHandle,
    secret: Id,
    body: BytesHandle,
    resource: CollectionHandle,
    envelope: BytesHandle,
    label: String,
    purpose: Purpose,
    bytes: Vec<u8>,
    device_key_len: usize,
    device_cipher_len: usize,
}

/// Exact request-bound one-use key. No Debug, Clone, serialization or export.
pub struct Unlocked {
    access: ProtectedAccess,
    dek: Key,
}

impl ProtectedAccess {
    pub fn secret(&self) -> Id {
        self.secret
    }
    pub fn body(&self) -> BytesHandle {
        self.body
    }
    pub fn resource(&self) -> CollectionHandle {
        self.resource
    }
    pub fn collection(&self) -> CollectionHandle {
        self.collection
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn purpose(&self) -> &Purpose {
        &self.purpose
    }
    pub fn device_key_id(&self) -> Option<&[u8]> {
        (self.device_key_len != 0).then(|| &self.bytes[FIXED..FIXED + self.device_key_len])
    }
    pub fn device_ciphertext(&self) -> Option<&[u8]> {
        (self.device_cipher_len != 0)
            .then(|| &self.bytes[FIXED + self.device_key_len..self.bytes.len() - 48])
    }
    pub fn unlock_password(&self, password: &[u8]) -> Result<Unlocked> {
        let key = password_key(password, &self.bytes[116..132])?;
        let end = self.bytes.len() - 48;
        let cipher = XChaCha20Poly1305::new_from_slice(&key[..]).expect("32-byte key");
        let dek = Zeroizing::new(
            cipher
                .decrypt(
                    XNonce::from_slice(&self.bytes[132..156]),
                    Payload {
                        msg: &self.bytes[end..],
                        aad: &self.bytes[..end],
                    },
                )
                .map_err(|_| anyhow!("protected secret authentication failed"))?,
        );
        Ok(Unlocked {
            access: self.clone(),
            dek: Key::try_from(dek.as_slice()).map_err(|_| anyhow!("invalid protected key"))?,
        })
    }
    pub fn unlock_device_payload(&self, payload: &[u8]) -> Result<Unlocked> {
        if payload.len() != BINDING + 32 || payload[..BINDING] != self.bytes[..BINDING] {
            bail!("device operation did not return this exact protected binding");
        }
        Ok(Unlocked {
            access: self.clone(),
            dek: Key::try_from(&payload[BINDING..])
                .map_err(|_| anyhow!("invalid protected device key"))?,
        })
    }
    fn matches(&self, other: &Self) -> bool {
        self.collection == other.collection
            && self.secret == other.secret
            && self.body == other.body
            && self.resource == other.resource
            && self.envelope == other.envelope
            && self.purpose == other.purpose
            && self.bytes == other.bytes
    }
}

fn password_key(password: &[u8], salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    if password.is_empty() || password.len() > 1024 {
        bail!("protection password must contain 1–1024 bytes");
    }
    let mut key = Zeroizing::new([0; 32]);
    crypto_pwhash(
        &mut key[..],
        password,
        salt,
        3,
        64 * 1024 * 1024,
        PasswordHashAlgorithm::Argon2id13,
    )
    .map_err(|_| anyhow!("cannot derive protected key"))?;
    Ok(key)
}

fn finish(
    mut fragment: Fragment,
    secret: Id,
    body: BytesHandle,
    structured: bool,
    dek: Key,
    protection: Protection<'_>,
) -> Result<SealedVersion> {
    // No key-delivery policy and no signer/recipient identity in this descriptor.
    let descriptor = entity! { metadata::tag: RESOURCE, wrap_secret: secret, secret_body: body };
    let resource = fragment.put::<SimpleArchive, _>(descriptor.facts().clone());
    fragment += entity! { ExclusiveId::force_ref(&secret) @ resource_handle: resource };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&FORMAT.raw());
    bytes.extend_from_slice(&secret.raw());
    bytes.extend_from_slice(&body.raw);
    bytes.extend_from_slice(&resource.raw);
    bytes.push(u8::from(structured));
    bytes.extend_from_slice(&[0; 7]);
    let mut device_payload = Zeroizing::new(bytes.clone());
    device_payload.extend_from_slice(&dek);
    let (device_key, device_cipher) = match protection.device {
        Some(device) => {
            if device.key_id.is_empty() || device.key_id.len() > 1024 {
                bail!("invalid device key identifier");
            }
            let cipher = (device.seal)(&device_payload)?;
            if cipher.is_empty() || cipher.len() > MAX_DEVICE {
                bail!("invalid device ciphertext size");
            }
            (device.key_id.to_vec(), cipher)
        }
        None => (Vec::new(), Vec::new()),
    };
    // Fixed profile: Argon2id v19, t=3, memory=65536 KiB, lanes=1.
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(&65536u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    let mut salt = [0; 16];
    dryoc::rng::copy_randombytes(&mut salt);
    bytes.extend_from_slice(&salt);
    let nonce = Nonce::gen();
    bytes.extend_from_slice(nonce.as_slice());
    bytes.extend_from_slice(&(device_key.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(device_cipher.len() as u32).to_le_bytes());
    // Reserved authenticated zero bytes keep the binary prefix aligned.
    bytes.extend_from_slice(&[0; 8]);
    bytes.extend_from_slice(&device_key);
    bytes.extend_from_slice(&device_cipher);
    let key = password_key(protection.password, &salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key[..]).expect("32-byte key");
    let sealed = cipher
        .encrypt(
            XNonce::from_slice(nonce.as_slice()),
            Payload {
                msg: &dek,
                aad: &bytes,
            },
        )
        .map_err(|_| anyhow!("cannot protect secret key"))?;
    bytes.extend_from_slice(&sealed);
    let envelope = fragment.put::<blobencodings::RawBytes, _>(bytes);
    fragment += entity! { ExclusiveId::force_ref(&secret) @ protected_envelope: envelope };
    Ok(SealedVersion {
        fragment,
        secret,
        recipients: Vec::new(),
    })
}

pub fn prepare_raw(
    name: &str,
    plaintext: &[u8],
    created_at: IntervalValue,
    protection: Protection<'_>,
) -> Result<SealedVersion> {
    validate_name("secret name", name)?;
    point_value("secret creation time", created_at)?;
    let secret = genid().id;
    let dek = Key::gen();
    let nonce = Nonce::gen();
    let mut bytes = nonce.to_vec();
    bytes.extend_from_slice(&DryocSecretBox::encrypt_to_vecbox(plaintext, &nonce, &dek).to_vec());
    let mut fragment = Fragment::empty();
    let body = fragment.put::<blobencodings::RawBytes, _>(bytes);
    let name = fragment.put(name.to_owned());
    fragment += secret_record(secret, name, body, created_at);
    finish(fragment, secret, body, false, dek, protection)
}

pub fn prepare_item(
    name: &str,
    input: item::ItemInput<'_>,
    created_at: IntervalValue,
    protection: Protection<'_>,
) -> Result<SealedVersion> {
    item::prepare_using(name, input, created_at, |key, bundle| {
        let secret = key.id();
        let body = bundle.root();
        let fragment = entity! { ExclusiveId::force_ref(&secret) @
        metadata::tag: KIND_ITEM_VERSION, metadata::name: name.to_owned(),
        metadata::created_at: created_at, structured_body*: bundle };
        finish(
            fragment,
            secret,
            body.transmute(),
            true,
            key.into_dek(),
            protection,
        )
    })
}

/// Detach only the exact selected envelopes; no password operation or live reader escapes.
pub fn prepare_access<R: BlobStoreGet, P: TriblePattern>(
    reader: &R,
    facts: &P,
    collection: CollectionHandle,
    secret: Id,
    selected: Option<BytesHandle>,
    purpose: Purpose,
) -> Result<Option<ProtectedAccess>> {
    let envelopes: BTreeSet<_> = find!(envelope: BytesHandle,
        pattern!(facts, [{ secret @ protected_envelope: ?envelope }]))
    .collect();
    if envelopes.is_empty() {
        return Ok(None);
    }
    let mut matches = Vec::new();
    for envelope in envelopes {
        // Inspect the mapped/borrowed blob before detaching an owned copy.
        // Untrusted blob length must not drive an allocation under this lease.
        let Ok(blob) = reader.get::<anybytes::Bytes, _>(envelope) else {
            continue;
        };
        let bytes = blob.as_ref();
        if bytes.len() < FIXED + 48
            || bytes.len() > FIXED + 48 + 1024 + MAX_DEVICE
            || bytes[..16] != FORMAT.raw()
            || bytes[16..32] != secret.raw()
            || bytes[96] > 1
            || bytes[97..104] != [0; 7]
            || bytes[104..108] != 3u32.to_le_bytes()
            || bytes[108..112] != 65536u32.to_le_bytes()
            || bytes[112..116] != 1u32.to_le_bytes()
            || bytes[164..172] != [0; 8]
        {
            continue;
        }
        let body = BytesHandle::new(bytes[32..64].try_into().unwrap());
        let resource = CollectionHandle::new(bytes[64..96].try_into().unwrap());
        let key_len = u32::from_le_bytes(bytes[156..160].try_into().unwrap()) as usize;
        let cipher_len = u32::from_le_bytes(bytes[160..164].try_into().unwrap()) as usize;
        if key_len > 1024
            || cipher_len > MAX_DEVICE
            || (key_len == 0) != (cipher_len == 0)
            || bytes.len() != FIXED + key_len + cipher_len + 48
            || selected.is_some_and(|selected| selected != body)
            || !exists!(pattern!(facts, [{ secret @ resource_handle: resource }]))
        {
            continue;
        }
        let advertised = if bytes[96] == 0 {
            exists!(pattern!(facts, [{ secret @ metadata::tag: KIND_SECRET, secret_body: body }]))
        } else {
            let item_body: item::ItemBody = body.transmute();
            exists!(
                pattern!(facts, [{ secret @ metadata::tag: KIND_ITEM_VERSION, structured_body: item_body }])
            )
        };
        if !advertised || (bytes[96] == 0) != (purpose == Purpose::Get) {
            continue;
        }
        let label = find!(name: TextHandle, pattern!(facts, [{ secret @ metadata::name: ?name }]))
            .find_map(|name| reader.get::<anybytes::View<str>, _>(name).ok())
            .map(|name| name.chars().filter(|c| !c.is_control()).take(160).collect())
            .unwrap_or_default();
        let mut request_nonce = [0; 16];
        dryoc::rng::copy_randombytes(&mut request_nonce);
        matches.push(ProtectedAccess {
            request_nonce,
            collection,
            secret,
            body,
            resource,
            envelope,
            label,
            purpose: purpose.clone(),
            bytes: bytes.to_vec(),
            device_key_len: key_len,
            device_cipher_len: cipher_len,
        });
    }
    // Alternatives remain data; this operation needs an explicit exact envelope,
    // not an iteration-order choice. Never fall through to a node envelope.
    match matches.len() {
        0 => {
            bail!("selected protected envelope is unavailable or unsupported; no node-key fallback")
        }
        1 => Ok(matches.pop()),
        _ => bail!("multiple protected envelopes match; exact envelope selection is required"),
    }
}

impl Unlocked {
    /// First verify against the ORIGINAL detached request returned to the UI.
    /// This checks its ephemeral nonce as well as all immutable selectors.
    /// Then re-read admitted facts and use `open_raw`/`open_item` with the FRESH
    /// prepared binding. Those methods compare immutable selectors, allowing
    /// a fresh observation but never substituting another body/purpose.
    pub fn validate(&self, access: &ProtectedAccess) -> Result<()> {
        if !self.access.matches(access) || self.access.request_nonce != access.request_nonce {
            bail!("protected access binding changed; nothing was disclosed");
        }
        Ok(())
    }
    pub fn open_raw<R: BlobStoreGet>(
        self,
        reader: &R,
        access: &ProtectedAccess,
    ) -> Result<Zeroizing<Vec<u8>>> {
        if !self.access.matches(access) {
            bail!("protected binding changed; nothing was disclosed");
        }
        if access.bytes[96] != 0 || access.purpose != Purpose::Get {
            bail!("wrong protected operation");
        }
        envelope::decrypt_body(reader, access.body, &self.dek)
    }
    pub fn open_item<R: BlobStoreGet>(
        self,
        reader: R,
        access: &ProtectedAccess,
    ) -> Result<item::OpenedItem<R>> {
        if !self.access.matches(access) {
            bail!("protected binding changed; nothing was disclosed");
        }
        if access.bytes[96] != 1 || access.purpose == Purpose::Get {
            bail!("wrong protected operation");
        }
        let body: item::ItemBody = access.body.transmute();
        let key = ItemKey::from_dek(access.secret, self.dek);
        let cipher = reader
            .get::<Blob<Encrypted<SimpleArchive>>, _>(body)
            .map_err(|_| anyhow!("protected item ciphertext is unavailable"))?;
        let facts: TribleSet = open_blob(&key, cipher)?
            .try_from_blob()
            .map_err(|_| anyhow!("protected item is not an archive"))?;
        if !exists!(pattern!(&facts, [{ metadata::tag: KIND_ITEM }])) {
            bail!("protected item root is unavailable");
        }
        Ok(item::OpenedItem {
            reader,
            key,
            facts,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use triblespace::core::repo::SnapshotSource;

    fn at() -> IntervalValue {
        let instant = Epoch::from_tai_seconds(1.0);
        (instant, instant).try_to_inline().unwrap()
    }

    #[test]
    fn password_bound_body_has_no_node_recipient_or_delivery_authority() {
        let sealed = prepare_raw(
            "synthetic",
            b"synthetic-protected-content",
            at(),
            Protection {
                password: b"synthetic user passphrase",
                device: None,
            },
        )
        .unwrap();
        assert!(sealed.recipients.is_empty());
        assert!(wrap_recipients(sealed.fragment.facts(), sealed.secret).is_empty());
        let mut blobs = sealed.fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();
        let node = SigningKey::from_bytes(&[7; 32]);
        assert!(
            open_version_from_facts(&reader, sealed.fragment.facts(), sealed.secret, &node)
                .is_err()
        );
        let collection = CollectionHandle::new([8; 32]);
        let access = prepare_access(
            &reader,
            sealed.fragment.facts(),
            collection,
            sealed.secret,
            None,
            Purpose::Get,
        )
        .unwrap()
        .unwrap();
        assert!(access.unlock_password(b"wrong password").is_err());
        let unlocked = access
            .unlock_password(b"synthetic user passphrase")
            .unwrap();
        unlocked.validate(&access).unwrap();
        let fresh = prepare_access(
            &reader,
            sealed.fragment.facts(),
            collection,
            sealed.secret,
            None,
            Purpose::Get,
        )
        .unwrap()
        .unwrap();
        assert!(
            unlocked.validate(&fresh).is_err(),
            "approval is one exact invocation, not a remembered unlock"
        );
        assert_eq!(
            &*unlocked.open_raw(&reader, &fresh).unwrap(),
            b"synthetic-protected-content"
        );
        let resource: TribleSet = reader.get(access.resource()).unwrap();
        assert!(!exists!(
            pattern!(&resource, [{ triblespace::core::capability::policy::resource_policy: _?policy }])
        ));
    }

    #[test]
    fn device_payload_and_password_authenticate_exact_binding_and_tampering() {
        let captured = std::cell::RefCell::new(Zeroizing::new(Vec::new()));
        let seal = |payload: &[u8]| {
            *captured.borrow_mut() = Zeroizing::new(payload.to_vec());
            Ok(vec![1, 2, 3]) // Synthetic platform seam, not a claimed hardware cipher.
        };
        let sealed = prepare_raw(
            "synthetic",
            b"synthetic-value",
            at(),
            Protection {
                password: b"synthetic-passphrase",
                device: Some(DeviceProtection {
                    key_id: b"fixture-key",
                    seal: &seal,
                }),
            },
        )
        .unwrap();
        let mut blobs = sealed.fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();
        let access = prepare_access(
            &reader,
            sealed.fragment.facts(),
            CollectionHandle::new([8; 32]),
            sealed.secret,
            None,
            Purpose::Get,
        )
        .unwrap()
        .unwrap();
        assert_eq!(access.device_key_id(), Some(b"fixture-key".as_slice()));
        assert_eq!(access.device_ciphertext(), Some([1, 2, 3].as_slice()));
        let payload = captured.borrow();
        access.unlock_device_payload(&payload).unwrap();
        let mut wrong = payload.to_vec();
        wrong[32] ^= 1;
        assert!(access.unlock_device_payload(&wrong).is_err());
        let mut altered = access.clone();
        altered.bytes[FIXED] ^= 1;
        assert!(altered.unlock_password(b"synthetic-passphrase").is_err());
        let unlocked = access.unlock_device_payload(&payload).unwrap();
        let mut other_purpose = access.clone();
        other_purpose.purpose = Purpose::Inspect;
        assert!(unlocked.validate(&other_purpose).is_err());
    }

    #[test]
    fn structured_protected_item_uses_existing_ciphertext_graph() {
        let sealed = prepare_item(
            "synthetic login",
            item::ItemInput {
                fields: &[item::FieldInput {
                    role: schema::ROLE_PASSWORD,
                    label: None,
                    value: item::FieldValue::Text("synthetic-private"),
                }],
                attachments: &[],
            },
            at(),
            Protection {
                password: b"synthetic-passphrase",
                device: None,
            },
        )
        .unwrap();
        let mut blobs = sealed.fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();
        let body = item::rows(sealed.fragment.facts())[0].body;
        let access = prepare_access(
            &reader,
            sealed.fragment.facts(),
            CollectionHandle::new([8; 32]),
            sealed.secret,
            Some(body.transmute()),
            Purpose::Inspect,
        )
        .unwrap()
        .unwrap();
        let node = SigningKey::from_bytes(&[7; 32]);
        assert!(item::open_exact(
            reader.clone(),
            sealed.fragment.facts(),
            sealed.secret,
            body,
            &node
        )
        .is_err());
        let opened = access
            .unlock_password(b"synthetic-passphrase")
            .unwrap()
            .open_item(reader, &access)
            .unwrap();
        assert!(exists!(
            pattern!(opened.facts(), [{ schema::field_role: schema::ROLE_PASSWORD }])
        ));
    }

    #[test]
    fn unavailable_malformed_and_oversized_envelopes_do_not_veto_a_usable_alternative() {
        let mut sealed = prepare_raw(
            "synthetic",
            b"private",
            at(),
            Protection {
                password: b"synthetic-passphrase",
                device: None,
            },
        )
        .unwrap();
        let secret = sealed.secret;
        let oversized = sealed
            .fragment
            .put::<blobencodings::RawBytes, _>(vec![0; 64 * 1024]);
        let malformed = sealed
            .fragment
            .put::<blobencodings::RawBytes, _>(vec![0; FIXED + 48]);
        let missing = BytesHandle::new([13; 32]);
        sealed.fragment += entity! { ExclusiveId::force_ref(&secret) @
        protected_envelope*: [oversized, malformed, missing] };
        let mut blobs = sealed.fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();
        let collection = CollectionHandle::new([8; 32]);
        let access = prepare_access(
            &reader,
            sealed.fragment.facts(),
            collection,
            secret,
            None,
            Purpose::Get,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            &*access
                .unlock_password(b"synthetic-passphrase")
                .unwrap()
                .open_raw(&reader, &access)
                .unwrap(),
            b"private"
        );
        let absent = genid().id;
        let only_bad = entity! { ExclusiveId::force_ref(&absent) @
        protected_envelope*: [oversized, malformed, missing] };
        assert!(prepare_access(
            &reader,
            only_bad.facts(),
            collection,
            absent,
            None,
            Purpose::Get
        )
        .is_err());
    }
}
