//! Provider credentials: one exact Secrets selector per provider, bound in the
//! `imagine` collection the way Discord binds its bot token.
//!
//! A binding names either a raw Secrets version, or a structured item version
//! (for instance a 1Password "API Credential" item) together with one exact
//! body of it and one exact private field. Names are never used to select:
//! they are neither unique nor version-safe. Binding proves the selected
//! credential opens before anything is written; status is metadata only;
//! the plaintext is opened at the point of use and never stored, logged or
//! put in an error.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{CollectionCommit, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::*;
use zeroize::Zeroizing;

use crate::schemas::imagine::{imagine, DEFAULT_SCOPE_ID, KIND_AUTH, PROVIDER_OPENAI};
use crate::secrets::item::{ItemBody, PrivateText};
use crate::storage::{AcquiringReader, FactArchive, FacultyAccess, FacultyStore, Storage};

/// A remote provider that needs a credential. The local FLUX path needs none.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Provider {
    OpenAi,
}

impl Provider {
    pub const ALL: &'static [Self] = &[Self::OpenAi];
    pub fn id(self) -> Id {
        match self {
            Self::OpenAi => PROVIDER_OPENAI,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
        }
    }
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;
    fn from_str(word: &str) -> Result<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "openai" => Ok(Self::OpenAi),
            other => bail!("unknown credential provider {other:?}; known: openai"),
        }
    }
}

/// What a caller names when binding: exact ids and handles only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Selector {
    /// A raw Secrets version whose plaintext is the key.
    Version(Id),
    /// One private text field of one exact body of a structured item version.
    ItemField {
        secret: Id,
        body: ItemBody,
        field: Id,
    },
}

/// A bound credential. An item field also carries the field's exact
/// ciphertext child, resolved when it was bound, so use opens one child.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Credential {
    Version(Id),
    ItemField {
        secret: Id,
        body: ItemBody,
        field: Id,
        child: PrivateText,
    },
}

impl Credential {
    /// Public references only, never plaintext.
    pub fn describe(&self) -> String {
        match self {
            Self::Version(secret) => format!("secret {secret:x}"),
            Self::ItemField {
                secret,
                body,
                field,
                child,
            } => format!(
                "item {secret:x} body blake3:{} field {field:x} child blake3:{}",
                hex::encode(body.raw),
                hex::encode(child.raw)
            ),
        }
    }
}

/// The current binding revisions for one provider and what they name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthStatus {
    pub heads: BTreeSet<Id>,
    pub credentials: BTreeSet<Credential>,
}

impl AuthStatus {
    /// A settled binding, never an arbitrary winner among concurrent ones.
    pub fn selected(&self, provider: Provider) -> Result<Credential> {
        let label = provider.label();
        if self.heads.is_empty() {
            bail!(
                "no {label} credential is bound; bind an exact Secrets version or item field with \
                 `imagine auth set {label}`"
            );
        }
        if self.heads.len() != 1 || self.credentials.len() != 1 {
            bail!(
                "the {label} credential binding is unsettled; inspect `imagine auth status` and \
                 rebind with `imagine auth set {label}`"
            );
        }
        Ok(*self.credentials.first().expect("one credential"))
    }
}

/// The bindings the facts hold for `provider`. Facts this reader does not
/// model are ignored. A head naming an item body, field and child is an item
/// field; a head naming only a version is a raw version.
pub fn status<P: TriblePattern>(facts: &P, provider: Provider) -> AuthStatus {
    let provider_id = provider.id();
    let candidates = find!(revision: Id, pattern!(facts, [{
        ?revision @ metadata::tag: KIND_AUTH, imagine::provider: provider_id,
    }]));
    let heads = latest(facts, metadata::supersedes.id(), candidates);
    let mut credentials = BTreeSet::new();
    for head in &heads {
        let head = *head;
        let items: Vec<Credential> = find!(
            (secret: Id, body: ItemBody, field: Id, child: PrivateText),
            pattern!(facts, [{ head @
                imagine::auth_secret_version: ?secret,
                imagine::auth_item_body: ?body,
                imagine::auth_item_field: ?field,
                imagine::auth_item_child: ?child,
            }])
        )
        .map(|(secret, body, field, child)| Credential::ItemField {
            secret,
            body,
            field,
            child,
        })
        .collect();
        if items.is_empty() {
            credentials.extend(
                find!(secret: Id, pattern!(facts, [{ head @ imagine::auth_secret_version: ?secret }]))
                    .map(Credential::Version),
            );
        } else {
            credentials.extend(items);
        }
    }
    AuthStatus { heads, credentials }
}

/// One binding revision superseding `predecessors`.
pub fn binding_fragment(
    provider: Provider,
    credential: Credential,
    predecessors: impl IntoIterator<Item = Id>,
) -> Fragment {
    match credential {
        Credential::Version(secret) => entity! { _ @
            metadata::tag: KIND_AUTH,
            imagine::provider: provider.id(),
            imagine::auth_secret_version: secret,
            metadata::supersedes*: predecessors,
        },
        Credential::ItemField {
            secret,
            body,
            field,
            child,
        } => entity! { _ @
            metadata::tag: KIND_AUTH,
            imagine::provider: provider.id(),
            imagine::auth_secret_version: secret,
            imagine::auth_item_body: body,
            imagine::auth_item_field: field,
            imagine::auth_item_child: child,
            metadata::supersedes*: predecessors,
        },
    }
}

/// Metadata only: no Secrets version is opened.
pub fn auth_status(storage: &Storage, provider: Provider) -> Result<AuthStatus> {
    storage.with_store(|pile, signer, runtime| {
        let facts = observe(storage, pile, signer, runtime)?;
        Ok(status(&facts, provider))
    })
}

/// Bind `selector` for `provider`: resolve an item field to its exact
/// ciphertext child, prove the credential opens to a usable key, then publish
/// one revision superseding every current head. Binding the settled current
/// credential again publishes nothing.
pub fn auth_set(
    storage: &Storage,
    provider: Provider,
    selector: Selector,
) -> Result<(Id, Credential)> {
    storage.scope(|storage| {
        let credential = resolve(storage, selector)?;
        drop(open(storage, credential)?);
        storage.with_store(|pile, signer, runtime| {
            let facts = observe(storage, pile, signer, runtime)?;
            let current = status(&facts, provider);
            if current.selected(provider).ok() == Some(credential) {
                return Ok((*current.heads.first().expect("settled head"), credential));
            }
            let fragment = binding_fragment(provider, credential, current.heads);
            let revision = fragment.root().expect("binding has one intrinsic root");
            commit(storage, pile, signer, runtime, fragment)?;
            Ok((revision, credential))
        })
    })
}

/// Open the bound key for `provider`.
pub(crate) fn open_key(storage: &Storage, provider: Provider) -> Result<Zeroizing<String>> {
    let credential = auth_status(storage, provider)?.selected(provider)?;
    open(storage, credential)
}

fn resolve(storage: &Storage, selector: Selector) -> Result<Credential> {
    match selector {
        Selector::Version(secret) => Ok(Credential::Version(secret)),
        Selector::ItemField {
            secret,
            body,
            field,
        } => {
            use crate::secrets::schema::{field_text, item_field, KIND_ITEM};
            let facts = crate::secrets::Secrets::with_storage(storage.clone())
                .open_item_body(secret, body)
                .with_context(|| format!("open Secrets item {secret:X}"))?;
            let children: BTreeSet<PrivateText> = find!(
                child: PrivateText,
                pattern!(&facts, [
                    { metadata::tag: KIND_ITEM, item_field: field },
                    { field @ field_text: ?child }
                ])
            )
            .collect();
            match children.len() {
                1 => Ok(Credential::ItemField {
                    secret,
                    body,
                    field,
                    child: *children.first().expect("one child"),
                }),
                0 => bail!("item {secret:X} has no text field {field:X} in that body"),
                count => bail!("item {secret:X} field {field:X} has {count} text values; it does not select one"),
            }
        }
    }
}

fn open(storage: &Storage, credential: Credential) -> Result<Zeroizing<String>> {
    let secrets = crate::secrets::Secrets::with_storage(storage.clone());
    let bytes: Zeroizing<Vec<u8>> = match credential {
        Credential::Version(secret) => secrets
            .get(secret)
            .with_context(|| format!("open the bound Secrets version {secret:X}"))?,
        Credential::ItemField {
            secret,
            body,
            child,
            ..
        } => Zeroizing::new(
            secrets
                .open_item_child(secret, body, child)
                .with_context(|| format!("open the bound field of Secrets item {secret:X}"))?
                .bytes
                .to_vec(),
        ),
    };
    let key = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("the bound credential is not UTF-8"))?
        .trim();
    if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        bail!("the bound credential is empty or contains spaces or control characters");
    }
    Ok(Zeroizing::new(key.to_owned()))
}

/// Advance this key's own commits into the Rank9 view and read it.
fn observe(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
) -> Result<FactArchive> {
    let source =
        storage.open_collection_read(pile, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    pile.with_local(|local| {
        pollster::block_on(async {
            crate::storage::tolerate_own_lag(local.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(local.maintain_attached(rank9, signer).await)
        })
        .context("maintain Imagine fact collection")
    })?;
    let snapshot = AcquiringReader::new(
        pile.snapshot()
            .context("freeze maintained Imagine snapshot")?,
        Arc::clone(runtime),
    );
    crate::storage::acquire_facts(&snapshot, rank9).context("read Imagine Rank9 collection")
}

fn commit(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    fragment: Fragment,
) -> Result<CollectionCommit> {
    let collection =
        storage.open_collection_write(pile, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)?;
    crate::collection_names::require_command_write_admission_acquiring(
        pile,
        collection,
        signer,
        "Imagine",
        "imagine auth",
        runtime,
    )?;
    let commit = pile
        .commit(collection, signer, fragment)
        .context("commit Imagine credential binding")?;
    {
        let mut local = pile.store();
        drop(
            pollster::block_on(crate::storage::ensure_downstream(
                &mut *local,
                collection,
                signer,
            ))
            .context("Imagine binding was committed, but ensuring its derived views failed")?,
        );
    }
    Ok(commit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_supersession_and_reads_both_shapes() {
        let secret = Id::new([7; 16]).unwrap();
        let item = Id::new([8; 16]).unwrap();
        let field = Id::new([9; 16]).unwrap();
        let body: ItemBody = Inline::new([1; 32]);
        let child: PrivateText = Inline::new([2; 32]);
        let first = binding_fragment(Provider::OpenAi, Credential::Version(secret), []);
        let first_id = first.root().unwrap();
        let mut facts = TribleSet::new();
        facts += first;
        assert_eq!(
            status(&facts, Provider::OpenAi)
                .selected(Provider::OpenAi)
                .unwrap(),
            Credential::Version(secret)
        );
        let rotated = Credential::ItemField {
            secret: item,
            body,
            field,
            child,
        };
        let second = binding_fragment(Provider::OpenAi, rotated, [first_id]);
        facts += second;
        let current = status(&facts, Provider::OpenAi);
        assert_eq!(current.heads.len(), 1);
        assert_eq!(current.selected(Provider::OpenAi).unwrap(), rotated);
        // A concurrent revision is a fork, never an arbitrary winner.
        facts += binding_fragment(Provider::OpenAi, Credential::Version(secret), [first_id]);
        assert!(status(&facts, Provider::OpenAi)
            .selected(Provider::OpenAi)
            .unwrap_err()
            .to_string()
            .contains("unsettled"));
    }

    #[test]
    fn nothing_bound_says_how_to_bind() {
        let error = status(&TribleSet::new(), Provider::OpenAi)
            .selected(Provider::OpenAi)
            .unwrap_err();
        assert!(error.to_string().contains("imagine auth set openai"));
    }
}
