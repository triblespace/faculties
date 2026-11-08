//! Standalone CLI workspace selection, separate from pile/key bootstrap.
//!
//! Selection reads only resident Config facts. Ordinary faculty payload I/O
//! retains the CLI's existing acquisition policy. No selector means exactly
//! the legacy storage configuration, without opening a pile or loading a key.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use ed25519_dalek::VerifyingKey;
use triblespace::core::repo::pile::{Pile, PileFile, PileSnapshot};
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::*;

use crate::storage::Storage;
use crate::workspace::{self, config_in, resolve_in};

#[derive(Args, Clone, Debug, Default)]
pub struct WorkspaceArgs {
    /// Exact workspace revision from `faculties workspace list`; never a label or prefix.
    #[arg(long, env = "FACULTIES_WORKSPACE", global = true, value_name = "REVISION", value_parser = parse_revision)]
    pub workspace: Option<Id>,
}

/// Keep the CLI selector grammar identical for Clap and Spec-based frontends.
pub fn parse_revision(value: &str) -> Result<Id, String> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(
            "workspace must be an exact 32-hex revision ID from `faculties workspace list`".into(),
        );
    }
    Id::from_hex(value).ok_or_else(|| "invalid workspace revision ID".into())
}

impl WorkspaceArgs {
    pub fn storage(&self, pile: PathBuf, key: Option<PathBuf>) -> Result<Storage> {
        let Some(revision) = self.workspace else {
            return Ok(Storage::new(pile, key));
        };
        // Pin the resolved key path as well as its identity: later ambient
        // changes cannot choose another bootstrap for this invocation.
        let key = crate::storage::signer_path(&pile, key.as_deref());
        let (snapshot, signer) = snapshot(&pile, Some(&key))?;
        let selected = resolve_in(&snapshot, signer, revision)?
            .with_context(|| format!("workspace {revision:X} is unavailable; run `faculties workspace list` and choose a current revision"))?;
        Ok(Storage::new(pile, Some(key)).with_workspace(selected, signer))
    }
}

fn snapshot(pile: &Path, key: Option<&Path>) -> Result<(PileSnapshot, VerifyingKey)> {
    let signer = crate::storage::load_signer(pile, key)?.verifying_key();
    let file = PileFile::open_read_only(pile)
        .with_context(|| format!("open workspace configuration in {}", pile.display()))?;
    let mut store = Pile::with_host(file, Some(signer));
    let snapshot = store
        .snapshot()
        .context("observe workspace configuration")?;
    Ok((snapshot, signer))
}

/// Discovery is deliberately independent of FACULTIES_WORKSPACE. The output
/// is a projection of this one observation, not a cached workspace catalog.
pub fn list(pile: &Path, key: Option<&Path>, out: &mut crate::out::Out<'_>) -> Result<()> {
    let (snapshot, signer) = snapshot(pile, key)?;
    let facts = config_in(&snapshot, signer)?;
    let ids = workspace::revision_ids(&facts);
    if ids.is_empty() {
        return out.line("No configured workspaces. Workspace selection does not create a bundle; use Erlkonig's requested workspace setup to create or enroll one.");
    }
    for id in ids {
        match workspace::resolve_revision(&facts, id, |label| {
            let text: View<str> = snapshot.get(label)?;
            Ok(text.to_string())
        }) {
            Ok(Some(selected)) => {
                out.line(format!(
                    "{id:X}  {}",
                    serde_json::to_string(&selected.label)?
                ))?;
                for (role, root) in selected.routes {
                    let name = crate::collection_names::table()
                        .into_iter()
                        .find_map(|(known, name)| (known == role).then_some(name));
                    out.line(format!(
                        "  {} ({role:X})  blake3:{}",
                        name.unwrap_or("unknown role"),
                        hex::encode(root.raw)
                    ))?;
                }
            }
            Ok(None) => out.line(format!("{id:X}  unavailable"))?,
            Err(error) => out.line(format!("{id:X}  unavailable: {error:#}"))?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod remaining_tests;
