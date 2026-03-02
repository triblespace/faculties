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
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::valueschemas::{Blake3, GenId, Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

const CONFIG_BRANCH_ID: Id = id_hex!("4790808CF044F979FC7C2E47FCCB4A64");

const KIND_CONFIG_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");
const KIND_CHUNK_ID: Id = id_hex!("40E6004417F9B767AFF1F138DE3D3AAC");

mod config_schema {
    use super::*;
    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: NsTAIInterval;
        "4E2F9CA7A8456DED8C43A3BE741ADA58" as branch_id: GenId;
    }
}

mod ctx {
    use super::*;
    attributes! {
        "81E520987033BE71EB0AFFA8297DE613" as kind: GenId;
        "8D5B05B6360EDFB6101A3E9A73A32F43" as level: U256BE;
        "3292CF0B3B6077991D8ECE6E2973D4B6" as summary: Handle<Blake3, LongString>;
        "4EAF7FE3122A0AE2D8309B79DCCB8D75" as start_at: NsTAIInterval;
        "95D629052C40FA09B378DDC507BEA0D3" as end_at: NsTAIInterval;
        "2407DD8440508B474B073A5ECF098500" as lens_id: GenId;
        "9B83D68AECD6888AA9CE95E754494768" as child: GenId;
        "CB97C36A32DEC70E0D1149E7C5D88588" as left: GenId;
        "087D07E3D9D94F0C4E96813C7BC5E74C" as right: GenId;
        "316834CC6B0EA6F073BF5362D67AC530" as about_exec_result: GenId;
    }
}

#[derive(Parser)]
#[command(
    name = "memory",
    about = "Show compacted context chunks (drill down by calling `memory <child>`)."
)]
struct Cli {
    /// Path to the pile file to use.
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Optional explicit branch id (hex) to read chunks from (defaults to config.branch_id).
    #[arg(long, global = true)]
    branch_id: Option<String>,
    /// One or more chunk ids / id prefixes to show, or `turn <turn-id>` to list facets for a turn.
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
