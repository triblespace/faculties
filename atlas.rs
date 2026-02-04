#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! rand_core = "0.6.4"
//! triblespace = "0.9.0"
//! ```

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::valueschemas::{Blake3, Handle};
use triblespace::prelude::*;

const DEFAULT_BRANCH: &str = "atlas";

#[derive(Parser)]
#[command(name = "atlas", about = "Schema metadata inspection faculty")]
struct Cli {
    /// Path to the pile file to use.
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch name for schema metadata.
    #[arg(long, default_value = DEFAULT_BRANCH, global = true)]
    branch: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List entities that have metadata::shortname entries.
    List,
    /// Show metadata for a single id prefix.
    Show { id: String },
}

#[derive(Clone)]
struct MetaRow {
    id: Id,
    shortname: String,
    name: Option<String>,
    tags: Vec<Id>,
}

fn main() -> Result<()> {
    let Cli {
        pile,
        branch,
        command,
    } = Cli::parse();
    let Some(cmd) = command else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };

    match cmd {
        Command::List => cmd_list(&pile, &branch),
        Command::Show { id } => cmd_show(&pile, &branch, &id),
    }
}

fn cmd_list(pile: &Path, branch: &str) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout atlas: {e:?}"))?;

    let mut rows = collect_rows(&mut ws, &space)?;
    rows.sort_by(|a, b| match a.shortname.cmp(&b.shortname) {
        Ordering::Equal => format!("{:x}", a.id).cmp(&format!("{:x}", b.id)),
        other => other,
    });

    for row in rows {
        let short_id = id_prefix(row.id);
        let tags = if row.tags.is_empty() {
            String::new()
        } else {
            format!(
                " [{}]",
                row.tags
                    .iter()
                    .map(|id| id_prefix(*id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let name = row
            .name
            .map(|n| format!(" - {n}"))
            .unwrap_or_default();
        println!("{short_id} {shortname}{tags}{name}", shortname = row.shortname);
    }

    repo.close()
        .map_err(|e| anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn cmd_show(pile: &Path, branch: &str, prefix: &str) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout atlas: {e:?}"))?;
    let rows = collect_rows(&mut ws, &space)?;
    let row = resolve_prefix(rows, prefix)?;

    println!("id: {:x}", row.id);
    println!("shortname: {}", row.shortname);
    if let Some(name) = row.name {
        println!("name: {name}");
    }
    if !row.tags.is_empty() {
        let tags = row
            .tags
            .iter()
            .map(|id| format!("{id:x}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("tags: {tags}");
    }

    repo.close()
        .map_err(|e| anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn collect_rows(ws: &mut Workspace<Pile<Blake3>>, space: &TribleSet) -> Result<Vec<MetaRow>> {
    let mut rows = Vec::new();
    for (id, shortname) in find!(
        (id: Id, shortname: String),
        pattern!(space, [{ ?id @ metadata::shortname: ?shortname }])
    ) {
        let name = match find!(
            (handle: Value<Handle<Blake3, LongString>>),
            pattern!(space, [{ id @ metadata::name: ?handle }])
        )
        .into_iter()
        .next()
        {
            Some((handle,)) => {
                let view: View<str> = ws.get(handle).context("read name")?;
                Some(view.to_string())
            }
            None => None,
        };

        let tags = find!(
            (tag: Id),
            pattern!(space, [{ id @ metadata::tag: ?tag }])
        )
        .into_iter()
        .map(|(tag,)| tag)
        .collect::<Vec<_>>();

        rows.push(MetaRow {
            id,
            shortname,
            name,
            tags,
        });
    }
    Ok(rows)
}

fn resolve_prefix(rows: Vec<MetaRow>, prefix: &str) -> Result<MetaRow> {
    let prefix = prefix.trim().to_lowercase();
    if prefix.is_empty() {
        bail!("id prefix is empty");
    }
    let mut matches = Vec::new();
    for row in rows {
        let hex = format!("{:x}", row.id);
        if hex.starts_with(&prefix) {
            matches.push(row);
        }
    }
    match matches.len() {
        0 => bail!("no id matches prefix '{prefix}'"),
        1 => Ok(matches.remove(0)),
        _ => bail!("multiple ids match prefix '{prefix}'"),
    }
}

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
}

fn open_repo(pile_path: &Path, branch_name: &str) -> Result<(Repository<Pile<Blake3>>, Id)> {
    if let Some(parent) = pile_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }

    let mut pile =
        Pile::<Blake3>::open(pile_path).map_err(|e| anyhow!("open pile: {e:?}"))?;
    pile.restore()
        .map_err(|e| anyhow!("restore pile: {e:?}"))?;

    let existing = find_branch_by_name(&mut pile, branch_name)?;
    let signing_key = SigningKey::generate(&mut OsRng);
    let mut repo = Repository::new(pile, signing_key);
    let branch_id = match existing {
        Some(id) => id,
        None => repo
            .create_branch(branch_name, None)
            .map_err(|e| anyhow!("create branch: {e:?}"))?
            .release(),
    };

    Ok((repo, branch_id))
}

fn find_branch_by_name(pile: &mut Pile<Blake3>, branch_name: &str) -> Result<Option<Id>> {
    let reader = pile.reader().map_err(|e| anyhow!("pile reader: {e:?}"))?;
    let iter = pile.branches().map_err(|e| anyhow!("list branches: {e:?}"))?;

    for branch in iter {
        let branch_id = branch.map_err(|e| anyhow!("branch id: {e:?}"))?;
        let Some(head) = pile
            .head(branch_id)
            .map_err(|e| anyhow!("branch head: {e:?}"))?
        else {
            continue;
        };
        let metadata_set: TribleSet = reader
            .get(head)
            .map_err(|e| anyhow!("branch metadata: {e:?}"))?;
        let mut names = find!(
            (shortname: String),
            pattern!(&metadata_set, [{ metadata::shortname: ?shortname }])
        )
        .into_iter();
        let Some(name) = names.next().map(|(name,)| name) else {
            continue;
        };
        if names.next().is_some() {
            continue;
        }
        if name == branch_name {
            return Ok(Some(branch_id));
        }
    }
    Ok(None)
}
