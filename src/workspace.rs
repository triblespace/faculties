//! Reusable exact workspace resolution extracted from the app configuration.
//!
//! The caller supplies one frozen view of its node's private `config`
//! collection. These queries neither select a workspace for the caller nor
//! open a pile, load a key, read environment variables, or acquire payloads.
//! Labels are read through the caller so its observation can track successful
//! and failed blob reads. The result describes one requested binding, not a
//! catalog of the store. Frontends keep bootstrap and selection policy.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use triblespace::core::collection::{Collection, CollectionHandle};
use triblespace::core::metadata;
use triblespace::core::query::register::{maximal, ObservationOrder};
use triblespace::core::repo::StoreRead;
use triblespace::prelude::*;

use crate::schemas::{config::config, workspace as attrs};

/// One edit request / native-fact projection, not a catalog of the store.
/// `id` is the only request-facing ID. `anchor` exists for explicit edits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    pub id: Id,
    pub anchor: Id,
    pub label: String,
    pub routes: BTreeMap<Id, CollectionHandle>,
}

impl Workspace {
    /// Prepare a new binding value without publishing or selecting it.
    pub fn new(label: impl Into<String>) -> Self {
        let anchor = genid().id;
        Self {
            id: anchor,
            anchor,
            label: label.into(),
            routes: BTreeMap::new(),
        }
    }
}

/// Resident-only configuration from the supplied node observation. An absent
/// descriptor means no configured workspaces, never permission to create it.
pub fn config_in<S: StoreRead>(snapshot: &S, signer: VerifyingKey) -> Result<TribleSet> {
    let handle = crate::config::handle(signer);
    if !snapshot.contains_blob(handle)? {
        return Ok(TribleSet::new());
    }
    let collection = Collection::<blobencodings::SimpleArchive>::open(snapshot, handle)
        .context("open existing workspace Config")?;
    collection
        .read(snapshot)
        .context("read resident workspace Config")
}

/// Resolve from exactly the supplied observation without acquisition, a
/// second snapshot, or reopening the caller's store.
pub fn resolve_in<S: StoreRead>(
    snapshot: &S,
    signer: VerifyingKey,
    revision: Id,
) -> Result<Option<Workspace>> {
    let facts = config_in(snapshot, signer)?;
    resolve_revision(&facts, revision, |label| {
        let text: View<str> = snapshot.get(label)?;
        Ok(text.to_string())
    })
}

/// Exact current revision IDs, never a global or per-session selection.
/// Resolving each ID remains fallible: unavailable/conflicting bindings must
/// not silently disappear from discovery in favor of another workspace.
pub fn revision_ids(facts: &TribleSet) -> Vec<Id> {
    let order = ObservationOrder::new(facts, metadata::supersedes.id());
    let mut ids: Vec<Id> = find!(state: Id, and!(
        pattern!(facts, [{ ?state @ metadata::tag: attrs::WORKSPACE,
            attrs::enrolled: true }]),
        maximal(state, &order),
    ))
    .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Resolve one exact current revision in the supplied configuration view.
/// Unknown, withdrawn, or superseded IDs resolve to `None`, never to their
/// replacements. Concurrent revisions and conflicting role bindings are
/// errors. An absent role stays absent; no roots or defaults are invented.
///
/// `read_text` must read from the same frozen observation as `facts`. The
/// caller owns dependency tracking, including a label whose bytes are absent.
pub fn resolve_revision(
    facts: &TribleSet,
    revision: Id,
    mut read_text: impl FnMut(
        Inline<inlineencodings::Handle<blobencodings::UTF8String>>,
    ) -> Result<String>,
) -> Result<Option<Workspace>> {
    let order = ObservationOrder::new(facts, metadata::supersedes.id());
    let current = find!(state: Id, and!(
        pattern!(facts, [{ ?state @ metadata::tag: attrs::WORKSPACE,
            attrs::enrolled: true }]),
        state.is(revision.to_inline()), maximal(state, &order),
    ))
    .next()
    .is_some();
    if !current {
        return Ok(None);
    }
    // Ignoring old path bindings would silently retarget out-of-store data.
    if exists!(pattern!(facts, [{ revision @ attrs::managed_pile: _?old }])) {
        bail!("legacy workspace {revision:X} requires explicit single-store migration");
    }
    let mut routes = BTreeMap::new();
    for (role, root) in find!((role: Id, root: CollectionHandle),
        pattern!(facts, [
            { revision @ attrs::workspace_route: _?route },
            { _?route @ config::anchor: ?role, config::selects: ?root },
        ])
    ) {
        if routes.insert(role, root).is_some_and(|old| old != root) {
            bail!("workspace {revision:X} has conflicting exact roots for role {role:X}");
        }
    }
    let mut candidates = Vec::new();
    for (anchor, label) in find!(
        (anchor: Id, label: Inline<inlineencodings::Handle<blobencodings::UTF8String>>),
        pattern!(facts, [{ revision @ config::anchor: ?anchor, metadata::name: ?label }])
    ) {
        let heads: Vec<Id> = find!(state: Id, and!(
            pattern!(facts, [{ ?state @ metadata::tag: attrs::WORKSPACE,
                config::anchor: anchor }]), maximal(state, &order),
        ))
        .collect();
        if heads.iter().any(|head| *head != revision)
            || exists!(pattern!(facts, [{ revision @ attrs::enrolled: false }]))
        {
            bail!(
                "workspace {revision:X} has concurrent revisions; explicitly reconcile before use"
            );
        }
        let value = Workspace {
            id: revision,
            anchor,
            label: read_text(label)?,
            routes: routes.clone(),
        };
        if !candidates.contains(&value) {
            candidates.push(value);
        }
    }
    if candidates.len() != 1 {
        bail!("workspace {revision:X} has no unambiguous understood binding");
    }
    Ok(candidates.pop())
}

#[cfg(test)]
mod tests;
