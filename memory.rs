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

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser};
use ed25519_dalek::SigningKey;
use hifitime::{Duration, Epoch};
use rand_core::OsRng;
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{Repository, Workspace};
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
        "2407DD8440508B474B073A5ECF098500" as lens_id: GenId;
        "9B83D68AECD6888AA9CE95E754494768" as child: GenId;
        "CB97C36A32DEC70E0D1149E7C5D88588" as left: GenId;
        "087D07E3D9D94F0C4E96813C7BC5E74C" as right: GenId;
        "316834CC6B0EA6F073BF5362D67AC530" as about_exec_result: GenId;
        "A4E2B712CA28AB1EED76C34DE72AFA39" as about_archive_message: GenId;
    }
}

#[derive(Parser)]
#[command(
    name = "memory",
    about = "Show compacted context chunks (drill down by calling `memory <child>`).\n\n\
             Subcommands:\n  \
             memory <id>                     — show chunk by id prefix\n  \
             memory turn <turn-id>           — list memory facets for a turn\n  \
             memory create <lens> <summary>  — create a memory chunk"
)]
struct Cli {
    /// Path to the pile file to use.
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Optional explicit branch id (hex) to read chunks from (defaults to config.branch_id).
    #[arg(long, global = true)]
    branch_id: Option<String>,
    /// One or more chunk ids / id prefixes to show, or `turn <turn-id>`, or `create <lens> <summary>`.
    #[arg(value_name = "ID")]
    ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct Chunk {
    id: Id,
    lens_id: Id,
    level: u64,
    summary: Value<Handle<Blake3, LongString>>,
    children: Vec<Id>,
    about_exec_result: Option<Id>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.ids.is_empty() {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    }

    // Dispatch to subcommand handlers.
    if cli.ids.first().is_some_and(|value| value == "create") {
        return cmd_create(&cli.pile, &cli.ids[1..]);
    }

    let explicit_branch_id = parse_optional_hex_id(cli.branch_id.as_deref())?;
    with_repo(&cli.pile, |repo| {
        let branch_id = match explicit_branch_id {
            Some(id) => id,
            None => load_core_branch_id(repo)?,
        };

        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull branch {branch_id:x}: {e:?}"))?;
        let catalog = ws.checkout(..).context("checkout branch")?;
        let index = load_chunks(&catalog);

        if cli.ids.first().is_some_and(|value| value == "turn") {
            if cli.ids.len() != 2 {
                bail!("usage: memory turn <turn-id>");
            }
            return print_turn_facets(&mut ws, &index, &cli.ids[1]);
        }

        let mut first = true;
        for raw in &cli.ids {
            let chunk_id = match resolve_chunk_id(&index, raw) {
                Ok(chunk_id) => chunk_id,
                Err(err) => {
                    return Err(invalid_memory_id_error(raw, err));
                }
            };
            let chunk = index
                .get(&chunk_id)
                .with_context(|| format!("missing chunk {raw}"))?;
            if !first {
                println!();
            }
            first = false;
            print_chunk(&mut ws, chunk)?;
        }

        Ok(())
    })
}

// ---------------------------------------------------------------------------
// create subcommand
// ---------------------------------------------------------------------------

fn cmd_create(pile_path: &Path, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!(
            "usage: memory create <lens> <summary...>\n\
             \n\
             Create a memory chunk and store it in the pile.\n\
             Reads branch_id and lens config from the config branch.\n\
             Per-invocation context from environment variables:\n  \
             FORK_LEVEL — chunk level (default 0)\n  \
             FORK_EVENT_TIME_NS — event timestamp as TAI nanoseconds\n  \
             FORK_ABOUT_EXEC_RESULT — exec result id (hex, optional)\n  \
             FORK_ABOUT_ARCHIVE_MESSAGE — archive message id (hex, optional)\n  \
             FORK_CHILD_IDS — comma-separated child chunk ids (hex, optional)"
        );
    }

    let lens_name = &args[0];
    let summary_text = args[1..].join(" ");
    if summary_text.is_empty() {
        bail!("summary text is required: memory create <lens> <summary...>");
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
    let now_epoch =
        Epoch::now().unwrap_or_else(|_| Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0));
    let created_at: Value<NsTAIInterval> = (now_epoch, now_epoch).to_value();

    with_repo(pile_path, |repo| {
        let (branch_id, lens_id) = load_create_config(repo, lens_name)?;

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
            Some(&format!("memory create {lens_name} lvl={level}")),
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
fn load_create_config(
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

// ---------------------------------------------------------------------------
// show / turn subcommands
// ---------------------------------------------------------------------------

fn load_core_branch_id(repo: &mut Repository<Pile<Blake3>>) -> Result<Id> {
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
    let space = ws.checkout(..).context("checkout config")?;

    let mut latest: Option<(Id, i128, Id)> = None;
    for (config_id, updated_at, branch_id) in find!(
        (config_id: Id, updated_at: Value<NsTAIInterval>, branch_id: Value<GenId>),
        pattern!(&space, [{
            ?config_id @
            config_schema::kind: &KIND_CONFIG_ID,
            config_schema::updated_at: ?updated_at,
            config_schema::branch_id: ?branch_id,
        }])
    ) {
        let key = interval_key(updated_at);
        let branch_id = Id::from_value(&branch_id);
        match latest {
            Some((_id, best_key, _best_branch)) if best_key >= key => {}
            _ => latest = Some((config_id, key, branch_id)),
        }
    }

    latest
        .map(|(_id, _key, branch_id)| branch_id)
        .ok_or_else(|| {
            anyhow!("config missing branch_id; set it with `playground config set branch-id <ID>`")
        })
}

fn load_chunks(space: &TribleSet) -> HashMap<Id, Chunk> {
    let mut chunks = HashMap::<Id, Chunk>::new();

    for (chunk_id, lens_id, level, summary) in find!(
        (
            chunk_id: Id,
            lens_id: Value<GenId>,
            level: Value<U256BE>,
            summary: Value<Handle<Blake3, LongString>>
        ),
        pattern!(space, [{
            ?chunk_id @
            ctx::kind: &KIND_CHUNK_ID,
            ctx::lens_id: ?lens_id,
            ctx::level: ?level,
            ctx::summary: ?summary,
        }])
    ) {
        let level = u256be_to_u64(level).unwrap_or_default();
        chunks.insert(
            chunk_id,
            Chunk {
                id: chunk_id,
                lens_id: Id::from_value(&lens_id),
                level,
                summary,
                children: Vec::new(),
                about_exec_result: None,
            },
        );
    }

    for (chunk_id, child) in find!(
        (chunk_id: Id, child: Value<GenId>),
        pattern!(space, [{
            ?chunk_id @
            ctx::kind: &KIND_CHUNK_ID,
            ctx::child: ?child,
        }])
    ) {
        if let Some(chunk) = chunks.get_mut(&chunk_id) {
            chunk.children.push(Id::from_value(&child));
        }
    }

    for (chunk_id, child) in find!(
        (chunk_id: Id, child: Value<GenId>),
        pattern!(space, [{
            ?chunk_id @
            ctx::kind: &KIND_CHUNK_ID,
            ctx::left: ?child,
        }])
    ) {
        if let Some(chunk) = chunks.get_mut(&chunk_id) {
            chunk.children.push(Id::from_value(&child));
        }
    }

    for (chunk_id, child) in find!(
        (chunk_id: Id, child: Value<GenId>),
        pattern!(space, [{
            ?chunk_id @
            ctx::kind: &KIND_CHUNK_ID,
            ctx::right: ?child,
        }])
    ) {
        if let Some(chunk) = chunks.get_mut(&chunk_id) {
            chunk.children.push(Id::from_value(&child));
        }
    }

    for (chunk_id, exec_id) in find!(
        (chunk_id: Id, exec_id: Value<GenId>),
        pattern!(space, [{
            ?chunk_id @
            ctx::kind: &KIND_CHUNK_ID,
            ctx::about_exec_result: ?exec_id,
        }])
    ) {
        if let Some(chunk) = chunks.get_mut(&chunk_id) {
            chunk.about_exec_result = Some(Id::from_value(&exec_id));
        }
    }

    let start_by_id: HashMap<Id, i128> = find!(
        (chunk_id: Id, start_at: Value<NsTAIInterval>),
        pattern!(space, [{
            ?chunk_id @
            ctx::kind: &KIND_CHUNK_ID,
            ctx::start_at: ?start_at,
        }])
    )
    .into_iter()
    .map(|(chunk_id, start_at)| (chunk_id, interval_key(start_at)))
    .collect();
    for chunk in chunks.values_mut() {
        chunk.children.sort_by_key(|child_id| {
            (
                start_by_id.get(child_id).copied().unwrap_or(i128::MAX),
                *child_id,
            )
        });
        chunk.children.dedup();
    }

    chunks
}

fn u256be_to_u64(value: Value<U256BE>) -> Option<u64> {
    let raw = value.raw;
    if raw[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    let bytes: [u8; 8] = raw[24..32].try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn print_chunk(ws: &mut Workspace<Pile<Blake3>>, chunk: &Chunk) -> Result<()> {
    let mut header = format!(
        "mem {} lens={} lvl={}",
        id_prefix(chunk.id),
        id_prefix(chunk.lens_id),
        chunk.level
    );
    if let Some(exec_id) = chunk.about_exec_result {
        header.push_str(&format!(" exec={}", id_prefix(exec_id)));
    }
    if !chunk.children.is_empty() {
        header.push_str(" children=");
        for (idx, child) in chunk.children.iter().enumerate() {
            if idx > 0 {
                header.push(',');
            }
            header.push_str(id_prefix(*child).as_str());
        }
    }
    println!("{header}");

    let summary: View<str> = ws.get(chunk.summary).context("read chunk summary")?;
    print!("{}", summary.trim_end());
    println!();
    Ok(())
}

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
}

fn resolve_chunk_id(index: &HashMap<Id, Chunk>, raw: &str) -> Result<Id> {
    let prefix = normalize_prefix(raw)?;

    let mut chunk_matches = Vec::new();
    for chunk_id in index.keys().copied() {
        if id_starts_with(chunk_id, prefix.as_str()) {
            chunk_matches.push(chunk_id);
        }
    }
    match chunk_matches.len() {
        1 => return Ok(chunk_matches[0]),
        n if n > 1 => {
            bail!("multiple chunk ids match prefix '{prefix}' (use a longer prefix)")
        }
        _ => {}
    }

    for chunk in index.values() {
        if let Some(turn_id) = chunk.about_exec_result {
            if id_starts_with(turn_id, prefix.as_str()) {
                bail!("turn id `{prefix}` is not a chunk id; use `memory turn {prefix}`");
            }
        }
    }

    bail!("no chunk id matches prefix '{prefix}'")
}

fn print_turn_facets(ws: &mut Workspace<Pile<Blake3>>, index: &HashMap<Id, Chunk>, raw: &str) -> Result<()> {
    let prefix = normalize_prefix(raw)?;
    let mut turn_matches = Vec::new();
    for chunk in index.values() {
        if let Some(turn_id) = chunk.about_exec_result {
            if id_starts_with(turn_id, prefix.as_str()) {
                turn_matches.push((turn_id, chunk.id));
            }
        }
    }
    match turn_matches.len() {
        0 => bail!("no turn_id matches prefix '{prefix}'"),
        _ => {}
    }

    turn_matches.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    turn_matches.dedup();

    let first_turn = turn_matches[0].0;
    if turn_matches.iter().any(|(turn_id, _)| *turn_id != first_turn) {
        bail!("multiple turn_id values match prefix '{prefix}' (use a longer prefix)");
    }

    let mut chunks: Vec<&Chunk> = turn_matches
        .iter()
        .filter_map(|(_, chunk_id)| index.get(chunk_id))
        .collect();
    chunks.sort_unstable_by(|a, b| a.level.cmp(&b.level).then(a.id.cmp(&b.id)));

    println!(
        "turn {} has {} memory facet(s)",
        id_prefix(first_turn),
        chunks.len()
    );
    for (i, chunk) in chunks.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_chunk(ws, chunk)?;
    }

    Ok(())
}

fn invalid_memory_id_error(raw: &str, cause: anyhow::Error) -> anyhow::Error {
    anyhow!(
        "memory lookup failed for id `{raw}`: {cause}\n\
         hint: that id is wrong here.\n\
         hint: only call `memory <id>` when you want to inspect an id that already appeared in prior output.\n\
         hint: do not guess memory ids or loop lookups; switch to a concrete non-memory action if no valid id is available."
    )
}

// ---------------------------------------------------------------------------
// utilities
// ---------------------------------------------------------------------------

fn normalize_prefix(raw: &str) -> Result<String> {
    let mut prefix = raw.trim().to_ascii_lowercase();
    if let Some(rest) = prefix.strip_prefix("0x") {
        prefix = rest.to_string();
    }
    if prefix.is_empty() {
        bail!("id prefix is empty");
    }
    Ok(prefix)
}

fn id_starts_with(id: Id, prefix: &str) -> bool {
    format!("{id:x}").starts_with(prefix)
}

fn parse_optional_hex_id(raw: Option<&str>) -> Result<Option<Id>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let id = Id::from_hex(trimmed).ok_or_else(|| anyhow!("invalid id {trimmed}"))?;
    Ok(Some(id))
}

fn interval_key(interval: Value<NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.from_value();
    lower.to_tai_duration().total_nanoseconds()
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
