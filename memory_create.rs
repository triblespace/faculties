#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4.2.3"
//! rand_core = "0.6.4"
//! triblespace = "0.16.0"
//! ```

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser};
use ed25519_dalek::SigningKey;
use hifitime::{Duration, Epoch};
use rand_core::OsRng;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::Repository;
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::valueschemas::{Blake3, GenId, Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

const CONFIG_BRANCH_ID: Id = id_hex!("4790808CF044F979FC7C2E47FCCB4A64");
const KIND_CONFIG_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");
const KIND_MEMORY_LENS_ID: Id = id_hex!("D982F64C48F263A312D6E342D09554B0");
const KIND_CHUNK_ID: Id = id_hex!("40E6004417F9B767AFF1F138DE3D3AAC");

mod config_schema {
    use super::*;
    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: NsTAIInterval;
        "4E2F9CA7A8456DED8C43A3BE741ADA58" as branch_id: GenId;
        "24CF9D532E03C44CF719546DDE7E0493" as memory_lens_id: GenId;
    }
}

mod ctx {
    use super::*;
    attributes! {
        "81E520987033BE71EB0AFFA8297DE613" as kind: GenId;
        "8D5B05B6360EDFB6101A3E9A73A32F43" as level: U256BE;
        "3292CF0B3B6077991D8ECE6E2973D4B6" as summary: Handle<Blake3, LongString>;
        "3D5865566AF5118471DA1FF7F87CB791" as created_at: NsTAIInterval;
        "4EAF7FE3122A0AE2D8309B79DCCB8D75" as start_at: NsTAIInterval;
        "95D629052C40FA09B378DDC507BEA0D3" as end_at: NsTAIInterval;
        "9B83D68AECD6888AA9CE95E754494768" as child: GenId;
        "316834CC6B0EA6F073BF5362D67AC530" as about_exec_result: GenId;
        "A4E2B712CA28AB1EED76C34DE72AFA39" as about_archive_message: GenId;
        "2407DD8440508B474B073A5ECF098500" as lens_id: GenId;
    }
}

#[derive(Parser)]
#[command(
    name = "memory_create",
    about = "Create a memory chunk and store it in the pile.\n\n\
             Reads branch_id and lens config from the config branch.\n\
             Per-invocation context from environment variables:\n  \
             FORK_LEVEL — chunk level (default 0)\n  \
             FORK_EVENT_TIME_NS — event timestamp as TAI nanoseconds\n  \
             FORK_ABOUT_EXEC_RESULT — exec result id (hex, optional)\n  \
             FORK_ABOUT_ARCHIVE_MESSAGE — archive message id (hex, optional)\n  \
             FORK_CHILD_IDS — comma-separated child chunk ids (hex, optional)"
)]
struct Cli {
    /// Lens name (e.g. factual, technical, emotional).
    lens_name: Option<String>,
    /// Summary text (all remaining arguments joined with spaces).
    #[arg(trailing_var_arg = true)]
    summary: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.lens_name.is_none() {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    }

    let lens_name = cli.lens_name.as_deref().unwrap();
    let summary_text = cli.summary.join(" ");
    if summary_text.is_empty() {
        bail!("summary text is required");
    }

    // When FORK_EVENT_TIME_NS is not set, run in validate-only mode:
    // confirm the summary and lens name but skip the pile write.
    let event_time_ns: i128 = match env::var("FORK_EVENT_TIME_NS") {
        Ok(raw) => raw.parse().context("parse FORK_EVENT_TIME_NS")?,
        Err(_) => {
            println!("memory noted for {lens_name} lens.");
            return Ok(());
        }
    };

    let level: u64 = env::var("FORK_LEVEL")
        .unwrap_or_else(|_| "0".to_string())
        .parse()
        .context("parse FORK_LEVEL")?;
    let about_exec_result = parse_optional_hex_env("FORK_ABOUT_EXEC_RESULT")?;
    let about_archive_message = parse_optional_hex_env("FORK_ABOUT_ARCHIVE_MESSAGE")?;
    let child_ids = parse_child_ids_env()?;

    let event_epoch = Epoch::from_tai_duration(Duration::from_total_nanoseconds(event_time_ns));
    let event_time: Value<NsTAIInterval> = (event_epoch, event_epoch).to_value();
    let now_epoch = Epoch::now().unwrap_or_else(|_| Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0));
    let created_at: Value<NsTAIInterval> = (now_epoch, now_epoch).to_value();

    let pile_path = PathBuf::from(
        env::var("PILE").unwrap_or_else(|_| "self.pile".to_string()),
    );

    with_repo(&pile_path, |repo| {
        // Read branch_id and lens_id from config.
        let (branch_id, lens_id) = load_fork_config(repo, lens_name)?;

        // Write chunk to the core branch.
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull branch {branch_id:x}: {e:?}"))?;

        let summary_handle = ws.put(summary_text.clone());
        let chunk_id = ufoid();
        let level_value: Value<U256BE> = level.to_value();

        let mut change = TribleSet::new();
        change += entity! { &chunk_id @
            ctx::kind: KIND_CHUNK_ID,
            ctx::lens_id: lens_id,
            ctx::level: level_value,
            ctx::summary: summary_handle,
            ctx::created_at: created_at,
            ctx::start_at: event_time,
            ctx::end_at: event_time,
        };

        if let Some(exec_id) = about_exec_result {
            change += entity! { &chunk_id @ ctx::about_exec_result: exec_id };
        }
        if let Some(archive_id) = about_archive_message {
            change += entity! { &chunk_id @ ctx::about_archive_message: archive_id };
        }
        for child_id in &child_ids {
            change += entity! { &chunk_id @ ctx::child: *child_id };
        }

        ws.commit(
            change,
            None,
            Some(&format!("memory_create {lens_name} lvl={level}")),
        );
        repo.push(&mut ws)
            .map_err(|e| anyhow!("push failed: {e:?}"))?;

        let chunk_id_released = chunk_id.release();
        let hex = format!("{chunk_id_released:x}");
        println!("memory created: {}", &hex[..8]);
        Ok(())
    })
}

/// Read branch_id (from latest config entity) and lens_id (from lens entry
/// matching the given name) from the config branch.
fn load_fork_config(
    repo: &mut Repository<Pile<Blake3>>,
    lens_name: &str,
) -> Result<(Id, Id)> {
    let Some(_head) = repo
        .storage_mut()
        .head(CONFIG_BRANCH_ID)
        .map_err(|e| anyhow!("config branch head: {e:?}"))?
    else {
        bail!("config branch is empty; run `playground config ...` to initialize it");
    };

    let mut ws = repo
        .pull(CONFIG_BRANCH_ID)
        .map_err(|e| anyhow!("pull config workspace: {e:?}"))?;
    let catalog = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout config workspace: {e:?}"))?;

    // Find latest config entity → branch_id.
    let mut latest_config: Option<(i128, Id)> = None;
    for (_config_id, updated_at, branch_id) in find!(
        (config_id: Id, updated_at: Value<NsTAIInterval>, branch_id: Value<GenId>),
        pattern!(&catalog, [{
            ?config_id @
            config_schema::kind: &KIND_CONFIG_ID,
            config_schema::updated_at: ?updated_at,
            config_schema::branch_id: ?branch_id,
        }])
    ) {
        let key = interval_key(updated_at);
        let branch_id = Id::from_value(&branch_id);
        match latest_config {
            Some((best_key, _)) if best_key >= key => {}
            _ => latest_config = Some((key, branch_id)),
        }
    }
    let branch_id = latest_config
        .map(|(_, id)| id)
        .ok_or_else(|| anyhow!("config missing branch_id"))?;

    // Find lens entry matching the name → lens_id.
    // Collect all lens entries with their updated_at keys.
    let mut lens_candidates: Vec<(Id, Id, i128)> = Vec::new();
    for (entry_id, lens_id, updated_at) in find!(
        (entry_id: Id, lens_id: Value<GenId>, updated_at: Value<NsTAIInterval>),
        pattern!(&catalog, [{
            ?entry_id @
            config_schema::kind: &KIND_MEMORY_LENS_ID,
            config_schema::updated_at: ?updated_at,
            config_schema::memory_lens_id: ?lens_id,
        }])
    ) {
        let key = interval_key(updated_at);
        let lens_id = Id::from_value(&lens_id);
        lens_candidates.push((entry_id, lens_id, key));
    }

    // Keep only the latest entry per lens_id.
    lens_candidates.sort_by(|a, b| b.2.cmp(&a.2));
    let mut seen_lens_ids = std::collections::HashSet::new();
    let latest_entries: Vec<(Id, Id)> = lens_candidates
        .into_iter()
        .filter(|(_, lens_id, _)| seen_lens_ids.insert(*lens_id))
        .map(|(entry_id, lens_id, _)| (entry_id, lens_id))
        .collect();

    // Match by name.
    for (entry_id, lens_id) in &latest_entries {
        // Resolve the name blob.
        let name_handles: Vec<_> = find!(
            (eid: Id, name: Value<Handle<Blake3, LongString>>),
            pattern!(&catalog, [{ ?eid @ metadata::name: ?name }])
        )
        .into_iter()
        .filter(|(eid, _)| *eid == *entry_id)
        .collect();

        if let Some((_, name_handle)) = name_handles.into_iter().next() {
            let view: View<str> = ws.get(name_handle).context("read lens name")?;
            if view.as_ref() == lens_name {
                return Ok((branch_id, *lens_id));
            }
        }
    }

    bail!("no memory lens named '{lens_name}' found in config")
}

fn interval_key(interval: Value<NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.from_value();
    lower.to_tai_duration().total_nanoseconds()
}

fn parse_optional_hex_env(name: &str) -> Result<Option<Id>> {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => {
            let id = Id::from_hex(raw.trim())
                .ok_or_else(|| anyhow!("{name}: invalid hex id"))?;
            Ok(Some(id))
        }
        _ => Ok(None),
    }
}

fn parse_child_ids_env() -> Result<Vec<Id>> {
    let raw = match env::var("FORK_CHILD_IDS") {
        Ok(raw) if !raw.trim().is_empty() => raw,
        _ => return Ok(Vec::new()),
    };
    raw.split(',')
        .map(|s| {
            Id::from_hex(s.trim())
                .ok_or_else(|| anyhow!("FORK_CHILD_IDS: invalid hex id '{}'", s.trim()))
        })
        .collect()
}

fn open_repo(path: &Path) -> Result<Repository<Pile<Blake3>>> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }
    let mut pile =
        Pile::<Blake3>::open(path).map_err(|e| anyhow!("open pile {}: {e:?}", path.display()))?;
    if let Err(err) = pile.restore() {
        let _ = pile.close();
        return Err(anyhow!("restore pile {}: {err:?}", path.display()));
    }
    Ok(Repository::new(pile, SigningKey::generate(&mut OsRng)))
}

fn with_repo<T>(
    pile: &Path,
    f: impl FnOnce(&mut Repository<Pile<Blake3>>) -> Result<T>,
) -> Result<T> {
    let mut repo = open_repo(pile)?;
    let result = f(&mut repo);
    let close_res = repo.close().map_err(|e| anyhow!("close pile: {e:?}"));
    if let Err(err) = close_res {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }
    result
}
