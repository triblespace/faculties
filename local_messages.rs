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

use anyhow::{Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::core::metadata;
use triblespace::core::repo::branch as branch_proto;
use triblespace::core::repo::{PushResult, Repository, Workspace};
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::*;

const DEFAULT_BRANCH: &str = "local-messages";
const DEFAULT_RELATIONS_BRANCH: &str = "relations";
const ATLAS_BRANCH: &str = "atlas";
const CONFIG_BRANCH_ID: Id = id_hex!("4790808CF044F979FC7C2E47FCCB4A64");
const CONFIG_KIND_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");

const KIND_MESSAGE_LABEL: &str = "local_message";
const KIND_READ_LABEL: &str = "local_read";

const KIND_MESSAGE_ID: Id = id_hex!("A3556A66B00276797FCE8A2742AB850F");
const KIND_READ_ID: Id = id_hex!("B663C15BB6F2BF591EA870386DD48537");

const KIND_PERSON_ID: Id = id_hex!("D8ADDE47121F4E7868017463EC860726");

const KIND_SPECS: [(Id, &str); 2] = [
    (KIND_MESSAGE_ID, KIND_MESSAGE_LABEL),
    (KIND_READ_ID, KIND_READ_LABEL),
];

type TextHandle = Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>>;

mod local {
    use super::*;

    attributes! {
        "42C4DB210F7EAFAF38F179ADCB4A9D5B" as from: valueschemas::GenId;
        "95D58D3E68A43979F8AA51415541414C" as to: valueschemas::GenId;
        "23075866B369B5F393D43B30649469F6" as body: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "53ECCC7489AF8D30EF385ED12073F4A3" as created_at: valueschemas::NsTAIInterval;

        "2213B191326E9B99605FA094E516E50E" as about_message: valueschemas::GenId;
        "99E92F483731FA6D59115A8D6D187A37" as reader: valueschemas::GenId;
        "934C5AD3DA8F7A2EB467460E50D17A4F" as read_at: valueschemas::NsTAIInterval;
    }
}

mod config_schema {
    use super::*;
    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: valueschemas::GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: valueschemas::NsTAIInterval;
        "2ED6FF7EAB93CB5608555AE4B9664CF8" as local_messages_branch_id: valueschemas::GenId;
        "D35F4F02E29825FBC790E324EFCD1B34" as relations_branch_id: valueschemas::GenId;
    }
}

mod relations_schema {
    use super::*;
    attributes! {
        "299E28A10114DC8C3B1661CD90CB8DF6" as label_norm: valueschemas::ShortString;
        "3E8812F6D22B2C93E2BCF0CE3C8C1979" as alias_norm: valueschemas::ShortString;
    }
}

fn normalize_label(label: &str) -> Result<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        bail!("label is empty");
    }
    Ok(trimmed.to_string())
}

fn normalize_lookup_key(label: &str) -> Result<String> {
    Ok(normalize_label(label)?.to_ascii_lowercase())
}

fn parse_optional_hex_id(raw: Option<&str>, label: &str) -> Result<Option<Id>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{label} is empty");
    }
    let Some(id) = Id::from_hex(trimmed) else {
        bail!("invalid {label} '{trimmed}'");
    };
    Ok(Some(id))
}

#[derive(Parser)]
#[command(
    name = "local-messages",
    about = "Local messaging faculty for the agent"
)]
struct Cli {
    /// Path to the pile file to use
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch name for local messages
    #[arg(long, default_value = DEFAULT_BRANCH, global = true)]
    branch: String,
    /// Branch id for local messages (hex). Overrides config.
    #[arg(long, global = true)]
    branch_id: Option<String>,
    /// Branch name for relations
    #[arg(long, default_value = DEFAULT_RELATIONS_BRANCH, global = true)]
    relations_branch: String,
    /// Branch id for relations (hex). Overrides config.
    #[arg(long, global = true)]
    relations_branch_id: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Send a message
    Send {
        /// Sender label.
        from: String,
        /// Receiver label.
        to: String,
        /// Message text.
        #[arg(value_name = "TEXT")]
        text: String,
    },
    /// List recent messages (latest first)
    List {
        /// Reader id or label.
        reader: String,
        /// Only show inbox messages unread by the reader.
        #[arg(long)]
        unread: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Mark a message as read
    Ack {
        id: String,
        /// Reader id or label.
        by: String,
    },
}

#[derive(Debug, Clone)]
struct MessageRow {
    id: Id,
    from: Id,
    to: Id,
    body: String,
    created_at: i128,
}

#[derive(Debug, Clone, Default)]
struct ConfigBranches {
    local_messages_branch_id: Option<Id>,
    relations_branch_id: Option<Id>,
}

fn now_epoch() -> Epoch {
    Epoch::now().unwrap_or_else(|_| Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0))
}

fn epoch_interval(epoch: Epoch) -> Value<valueschemas::NsTAIInterval> {
    (epoch, epoch).to_value()
}

fn interval_key(interval: Value<valueschemas::NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.from_value();
    lower.to_tai_duration().total_nanoseconds()
}

fn format_age(now_key: i128, past_key: i128) -> String {
    let delta_ns = now_key.saturating_sub(past_key);
    let delta_s = (delta_ns / 1_000_000_000).max(0) as i64;
    if delta_s < 60 {
        format!("{delta_s}s")
    } else if delta_s < 60 * 60 {
        format!("{}m", delta_s / 60)
    } else if delta_s < 24 * 60 * 60 {
        format!("{}h", delta_s / 3600)
    } else {
        format!("{}d", delta_s / 86_400)
    }
}

fn truncate_single_line(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(max);
    for ch in text.chars() {
        if out.len() >= max {
            out.push_str("...");
            break;
        }
        if ch == '\n' || ch == '\r' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn render_list_body(text: &str) -> String {
    text.replace('\r', "").replace('\n', "\\n")
}

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
}

fn load_relations_space(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    relations_branch_id: Id,
) -> Result<TribleSet> {
    if repo
        .storage_mut()
        .head(relations_branch_id)
        .map_err(|e| anyhow::anyhow!("relations branch head: {e:?}"))?
        .is_none()
    {
        bail!(
            "missing relations branch {:x} (create with relations faculty)",
            relations_branch_id
        );
    }
    let mut ws = repo
        .pull(relations_branch_id)
        .map_err(|e| anyhow::anyhow!("pull relations workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout relations: {e:?}"))?;
    Ok(space)
}

fn resolve_normalized_person_matches(relations_space: &TribleSet, key: &str) -> Vec<Id> {
    let mut matches = HashSet::new();

    for (person_id,) in find!(
        (person_id: Id),
        pattern!(&relations_space, [{
            ?person_id @
            metadata::tag: &KIND_PERSON_ID,
            relations_schema::label_norm: key,
        }])
    ) {
        matches.insert(person_id);
    }

    for (person_id,) in find!(
        (person_id: Id),
        pattern!(&relations_space, [{
            ?person_id @
            metadata::tag: &KIND_PERSON_ID,
            relations_schema::alias_norm: key,
        }])
    ) {
        matches.insert(person_id);
    }

    matches.into_iter().collect()
}

fn resolve_person_id(relations_space: &TribleSet, input: &str) -> Result<Id> {
    let trimmed = input.trim();
    if let Some(id) = Id::from_hex(trimmed) {
        return Ok(id);
    }
    let label = normalize_label(trimmed)?;
    let key = normalize_lookup_key(trimmed)?;
    let matches = resolve_normalized_person_matches(relations_space, &key);

    match matches.len() {
        0 => bail!(
            "unknown person label '{label}' (run playground/migrations/relations_backfill_norm.rs for older piles)"
        ),
        1 => Ok(matches[0]),
        _ => bail!("multiple people match label '{label}'"),
    }
}

fn load_person_labels(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    relations_space: &TribleSet,
) -> HashMap<Id, String> {
    let mut labels = HashMap::new();
    let Ok(reader) = repo.storage_mut().reader() else {
        return labels;
    };
    for (person_id, handle) in find!(
        (person_id: Id, handle: TextHandle),
        pattern!(&relations_space, [{
            ?person_id @
            metadata::tag: &KIND_PERSON_ID,
            metadata::name: ?handle,
        }])
    ) {
        let Ok(view) = reader.get::<View<str>, _>(handle) else {
            continue;
        };
        labels.insert(person_id, view.as_ref().to_string());
    }
    labels
}

fn open_repo(path: &Path) -> Result<Repository<Pile<valueschemas::Blake3>>> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create pile dir {}: {e}", parent.display()))?;
    }

    let mut pile = Pile::<valueschemas::Blake3>::open(path)
        .map_err(|e| anyhow::anyhow!("open pile {}: {e:?}", path.display()))?;
    if let Err(err) = pile.restore() {
        // Avoid Drop warnings on early errors.
        let _ = pile.close();
        return Err(anyhow::anyhow!("restore pile {}: {err:?}", path.display()));
    }

    let signing_key = SigningKey::generate(&mut OsRng);
    Ok(Repository::new(pile, signing_key))
}

fn with_repo<T>(
    pile: &Path,
    f: impl FnOnce(&mut Repository<Pile<valueschemas::Blake3>>) -> Result<T>,
) -> Result<T> {
    let mut repo = open_repo(pile)?;
    let result = f(&mut repo);
    let close_res = repo
        .close()
        .map_err(|e| anyhow::anyhow!("close pile: {e:?}"));
    if let Err(err) = close_res {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }
    result
}

fn ensure_branch_with_id(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_id: Id,
    branch_name: &str,
) -> Result<()> {
    if repo
        .storage_mut()
        .head(branch_id)
        .map_err(|e| anyhow::anyhow!("branch head {branch_name}: {e:?}"))?
        .is_some()
    {
        return Ok(());
    }

    let name_blob = branch_name.to_owned().to_blob();
    let name_handle = name_blob.get_handle::<valueschemas::Blake3>();
    repo.storage_mut()
        .put(name_blob)
        .map_err(|e| anyhow::anyhow!("store branch name {branch_name}: {e:?}"))?;
    let metadata = branch_proto::branch_unsigned(branch_id, name_handle, None);
    let metadata_handle = repo
        .storage_mut()
        .put(metadata.to_blob())
        .map_err(|e| anyhow::anyhow!("store branch metadata {branch_name}: {e:?}"))?;
    let result = repo
        .storage_mut()
        .update(branch_id, None, Some(metadata_handle))
        .map_err(|e| anyhow::anyhow!("create branch {branch_name} ({branch_id:x}): {e:?}"))?;
    match result {
        PushResult::Success() | PushResult::Conflict(_) => Ok(()),
    }
}

fn load_config_branches(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
) -> Result<ConfigBranches> {
    let Some(_config_head) = repo
        .storage_mut()
        .head(CONFIG_BRANCH_ID)
        .map_err(|e| anyhow::anyhow!("config branch head: {e:?}"))?
    else {
        return Ok(ConfigBranches::default());
    };

    let mut ws = repo
        .pull(CONFIG_BRANCH_ID)
        .map_err(|e| anyhow::anyhow!("pull config workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout config workspace: {e:?}"))?;

    let mut latest: Option<(Id, i128)> = None;
    for (config_id, updated_at) in find!(
        (config_id: Id, updated_at: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{
            ?config_id @
            config_schema::kind: &CONFIG_KIND_ID,
            config_schema::updated_at: ?updated_at,
        }])
    ) {
        let key = interval_key(updated_at);
        if latest.is_none_or(|(_, current)| key > current) {
            latest = Some((config_id, key));
        }
    }
    let Some((config_id, _)) = latest else {
        return Ok(ConfigBranches::default());
    };

    let local_messages_branch_id = find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config_schema::local_messages_branch_id: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == config_id).then_some(value.from_value()));
    let relations_branch_id = find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config_schema::relations_branch_id: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == config_id).then_some(value.from_value()));

    Ok(ConfigBranches {
        local_messages_branch_id,
        relations_branch_id,
    })
}

fn resolve_branch_id(
    explicit_id: Option<Id>,
    configured_id: Option<Id>,
    branch_name: &str,
) -> Result<Id> {
    if let Some(id) = explicit_id {
        return Ok(id);
    }
    configured_id.ok_or_else(|| {
        anyhow::anyhow!(
            "missing {branch_name} branch id in config (set it via `playground config set`)"
        )
    })
}

fn find_branch_by_name(
    pile: &mut Pile<valueschemas::Blake3>,
    branch_name: &str,
) -> Result<Option<Id>> {
    let name_handle = branch_name
        .to_owned()
        .to_blob()
        .get_handle::<valueschemas::Blake3>();
    let reader = pile
        .reader()
        .map_err(|e| anyhow::anyhow!("pile reader: {e:?}"))?;
    let iter = pile
        .branches()
        .map_err(|e| anyhow::anyhow!("list branches: {e:?}"))?;

    for branch in iter {
        let branch_id = branch.map_err(|e| anyhow::anyhow!("branch id: {e:?}"))?;
        let Some(head) = pile
            .head(branch_id)
            .map_err(|e| anyhow::anyhow!("branch head: {e:?}"))?
        else {
            continue;
        };
        let metadata_set: TribleSet = reader
            .get(head)
            .map_err(|e| anyhow::anyhow!("branch metadata: {e:?}"))?;
        let mut names = find!(
            (handle: TextHandle),
            pattern!(&metadata_set, [{ metadata::name: ?handle }])
        )
        .into_iter();
        let Some(name) = names.next().map(|(handle,)| handle) else {
            continue;
        };
        if names.next().is_some() {
            continue;
        }
        if name == name_handle {
            return Ok(Some(branch_id));
        }
    }

    Ok(None)
}

fn ensure_metadata(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<TribleSet> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
    let mut change = TribleSet::new();

    let mut existing_kinds: HashSet<Id> = find!(
        (kind: Id),
        pattern!(&space, [{ ?kind @ metadata::name: _?name }])
    )
    .into_iter()
    .map(|(kind,)| kind)
    .collect();

    for (id, label) in KIND_SPECS {
        if !existing_kinds.contains(&id) {
            let name_handle = label
                .to_owned()
                .to_blob()
                .get_handle::<valueschemas::Blake3>();
            change += entity! { ExclusiveId::force_ref(&id) @ metadata::name: name_handle };
            existing_kinds.insert(id);
        }
    }

    Ok(change)
}

fn resolve_message_id(space: &TribleSet, prefix: &str) -> Result<Id> {
    let prefix = prefix.trim().to_lowercase();
    if prefix.is_empty() {
        bail!("message id prefix is empty");
    }
    if prefix.len() == 32 {
        if let Some(id) = Id::from_hex(&prefix) {
            return Ok(id);
        }
    }

    let mut matches = Vec::new();
    for (message_id,) in find!(
        (message_id: Id),
        pattern!(&space, [{ ?message_id @ metadata::tag: &KIND_MESSAGE_ID }])
    ) {
        let hex = format!("{message_id:x}");
        if hex.starts_with(&prefix) {
            matches.push(message_id);
        }
    }

    match matches.len() {
        0 => bail!("no message id matches prefix '{prefix}'"),
        1 => Ok(matches[0]),
        _ => bail!("multiple messages match prefix '{prefix}'"),
    }
}

fn load_text(ws: &mut Workspace<Pile<valueschemas::Blake3>>, handle: TextHandle) -> Result<String> {
    let view: View<str> = ws.get(handle).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(view.as_ref().to_string())
}

fn cmd_send(
    pile: &Path,
    branch_id: Id,
    relations_branch_id: Id,
    branch_name: &str,
    text: String,
    from: String,
    to: String,
) -> Result<()> {
    with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch_name)?;
        let relations_space = load_relations_space(repo, relations_branch_id)?;
        let from_id = resolve_person_id(&relations_space, &from)?;
        let to_id = resolve_person_id(&relations_space, &to)?;

        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
        let mut change = ensure_metadata(&mut ws)?;

        let now = epoch_interval(now_epoch());
        let message_id = ufoid();
        let body_handle = ws.put(text.clone());
        change += entity! { &message_id @
            metadata::tag: &KIND_MESSAGE_ID,
            local::from: from_id,
            local::to: to_id,
            local::body: body_handle,
            local::created_at: now,
        };

        ws.commit(change, None, Some("local message"));
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push message: {e:?}"))?;
        drop(ws);
        println!(
            "[{}] {} -> {}: {}",
            id_prefix(*message_id),
            from_id,
            to_id,
            truncate_single_line(&text, 120)
        );
        Ok(())
    })
}

fn cmd_ack(
    pile: &Path,
    branch_id: Id,
    relations_branch_id: Id,
    branch_name: &str,
    id: String,
    by: String,
) -> Result<()> {
    with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch_name)?;
        let relations_space = load_relations_space(repo, relations_branch_id)?;
        let reader_id = resolve_person_id(&relations_space, &by)?;

        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
        let mut change = ensure_metadata(&mut ws)?;

        let space = ws
            .checkout(..)
            .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
        let message_id = resolve_message_id(&space, &id)?;

        let now = epoch_interval(now_epoch());
        let read_id = ufoid();
        change += entity! { &read_id @
            metadata::tag: &KIND_READ_ID,
            local::about_message: message_id,
            local::reader: reader_id,
            local::read_at: now,
        };

        ws.commit(change, None, Some("local message read"));
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push read: {e:?}"))?;
        drop(ws);
        println!("Marked {} as read by {}.", id_prefix(message_id), reader_id);
        Ok(())
    })
}

fn cmd_list(
    pile: &Path,
    branch_id: Id,
    relations_branch_id: Id,
    branch_name: &str,
    reader: String,
    unread: bool,
    limit: usize,
) -> Result<()> {
    with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch_name)?;
        let relations_space = load_relations_space(repo, relations_branch_id)?;
        let party_names = load_person_labels(repo, &relations_space);
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
        let space = ws
            .checkout(..)
            .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;

        let mut messages = Vec::new();
        for (message_id, from, to, body, created_at) in find!(
            (
                message_id: Id,
                from: Id,
                to: Id,
                body: TextHandle,
                created_at: Value<valueschemas::NsTAIInterval>
            ),
            pattern!(&space, [{
                ?message_id @
                metadata::tag: &KIND_MESSAGE_ID,
                local::from: ?from,
                local::to: ?to,
                local::body: ?body,
                local::created_at: ?created_at,
            }])
        ) {
            let body_text = load_text(&mut ws, body)?;
            messages.push(MessageRow {
                id: message_id,
                from,
                to,
                body: body_text,
                created_at: interval_key(created_at),
            });
        }

        let mut reads: HashMap<(Id, Id), i128> = HashMap::new();
        for (_read_id, message_id, reader_id, read_at) in find!(
            (
                read_id: Id,
                message_id: Id,
                reader_id: Id,
                read_at: Value<valueschemas::NsTAIInterval>
            ),
            pattern!(&space, [{
                ?read_id @
                metadata::tag: &KIND_READ_ID,
                local::about_message: ?message_id,
                local::reader: ?reader_id,
                local::read_at: ?read_at,
            }])
        ) {
            let key = (message_id, reader_id);
            let ts = interval_key(read_at);
            reads
                .entry(key)
                .and_modify(|existing| {
                    if ts > *existing {
                        *existing = ts;
                    }
                })
                .or_insert(ts);
        }

        messages.sort_by_key(|msg| msg.created_at);
        messages.reverse();

        let now_key = interval_key(epoch_interval(now_epoch()));
        let reader_id = resolve_person_id(&relations_space, &reader)?;
        let mut shown = 0usize;

        for msg in messages {
            let incoming = msg.to == reader_id;
            let outgoing = msg.from == reader_id;
            if !incoming && !outgoing {
                continue;
            }

            let read = reads.get(&(msg.id, reader_id)).copied();
            if unread && !(incoming && read.is_none()) {
                continue;
            }

            let from_label = party_names
                .get(&msg.from)
                .cloned()
                .unwrap_or_else(|| id_prefix(msg.from));
            let to_label = party_names
                .get(&msg.to)
                .cloned()
                .unwrap_or_else(|| id_prefix(msg.to));
            let status = if incoming {
                if read.is_some() {
                    "read".to_string()
                } else {
                    "unread".to_string()
                }
            } else if reads.contains_key(&(msg.id, msg.to)) {
                format!("read-by:{to_label}")
            } else {
                "sent".to_string()
            };
            let age = format_age(now_key, msg.created_at);
            println!(
                "[{}] {} {} -> {} ({}) {}",
                id_prefix(msg.id),
                age,
                from_label,
                to_label,
                status,
                render_list_body(&msg.body)
            );
            shown += 1;
            if shown >= limit {
                break;
            }
        }

        if shown == 0 {
            println!("No messages.");
        }

        drop(ws);
        Ok(())
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Err(err) = emit_schema_to_atlas(&cli.pile) {
        eprintln!("atlas emit: {err}");
    }
    let Some(cmd) = cli.command else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };

    let branch_id_override = parse_optional_hex_id(cli.branch_id.as_deref(), "branch id")?;
    let relations_branch_id_override =
        parse_optional_hex_id(cli.relations_branch_id.as_deref(), "relations branch id")?;

    let config_branches = with_repo(&cli.pile, |repo| load_config_branches(repo))?;
    let branch_id = resolve_branch_id(
        branch_id_override,
        config_branches.local_messages_branch_id,
        &cli.branch,
    )?;
    let relations_branch_id = resolve_branch_id(
        relations_branch_id_override,
        config_branches.relations_branch_id,
        &cli.relations_branch,
    )?;

    match cmd {
        Command::Send { text, from, to } => cmd_send(
            &cli.pile,
            branch_id,
            relations_branch_id,
            &cli.branch,
            text,
            from,
            to,
        ),
        Command::List {
            reader,
            unread,
            limit,
        } => cmd_list(
            &cli.pile,
            branch_id,
            relations_branch_id,
            &cli.branch,
            reader,
            unread,
            limit,
        ),
        Command::Ack { id, by } => cmd_ack(
            &cli.pile,
            branch_id,
            relations_branch_id,
            &cli.branch,
            id,
            by,
        ),
    }
}

fn emit_schema_to_atlas(pile_path: &Path) -> Result<()> {
    with_repo(pile_path, |repo| {
        let branch_id = if let Some(id) = find_branch_by_name(repo.storage_mut(), ATLAS_BRANCH)? {
            id
        } else {
            repo.create_branch(ATLAS_BRANCH, None)
                .map_err(|e| anyhow::anyhow!("create branch: {e:?}"))?
                .release()
        };
        let metadata = build_local_metadata(repo.storage_mut())
            .map_err(|e| anyhow::anyhow!("build local metadata: {e:?}"))?;

        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull atlas workspace: {e:?}"))?;
        let space = ws
            .checkout(..)
            .map_err(|e| anyhow::anyhow!("checkout atlas workspace: {e:?}"))?;
        let delta = metadata.difference(&space);
        if !delta.is_empty() {
            ws.commit(delta, None, Some("atlas schema metadata"));
            repo.push(&mut ws)
                .map_err(|e| anyhow::anyhow!("push atlas metadata: {e:?}"))?;
        }
        Ok(())
    })
}

fn build_local_metadata<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<valueschemas::Blake3>,
{
    let mut metadata = TribleSet::new();

    metadata += <valueschemas::GenId as metadata::ConstDescribe>::describe(blobs)?;
    metadata += <valueschemas::NsTAIInterval as metadata::ConstDescribe>::describe(blobs)?;
    metadata +=
        <valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString> as metadata::ConstDescribe>::describe(
            blobs,
        )?;
    metadata += <blobschemas::LongString as metadata::ConstDescribe>::describe(blobs)?;

    metadata += metadata::Describe::describe(&local::from, blobs)?;
    metadata += metadata::Describe::describe(&local::to, blobs)?;
    metadata += metadata::Describe::describe(&local::body, blobs)?;
    metadata += metadata::Describe::describe(&local::created_at, blobs)?;
    metadata += metadata::Describe::describe(&local::about_message, blobs)?;
    metadata += metadata::Describe::describe(&local::reader, blobs)?;
    metadata += metadata::Describe::describe(&local::read_at, blobs)?;

    metadata += describe_kind(
        blobs,
        &KIND_MESSAGE_ID,
        "local_message",
        "Local message kind.",
    )?;
    metadata += describe_kind(
        blobs,
        &KIND_READ_ID,
        "local_read",
        "Local read receipt kind.",
    )?;

    Ok(metadata)
}

fn describe_kind<B>(
    blobs: &mut B,
    id: &Id,
    name: &str,
    description: &str,
) -> std::result::Result<Fragment, B::PutError>
where
    B: BlobStore<valueschemas::Blake3>,
{
    let name_handle = blobs.put(name.to_string())?;

    Ok(entity! { ExclusiveId::force_ref(id) @
        metadata::name: name_handle,
        metadata::description: blobs.put(description.to_string())?,
    })
}
