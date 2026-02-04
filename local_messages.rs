#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4.2.3"
//! rand_core = "0.6.4"
//! triblespace = "0.9.0"
//! ```

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::core::metadata;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::*;

const DEFAULT_BRANCH: &str = "local-messages";
const ATLAS_BRANCH: &str = "atlas";

const KIND_MESSAGE_LABEL: &str = "local_message";
const KIND_READ_LABEL: &str = "local_read";
const KIND_PARTY_LABEL: &str = "local_party";

const KIND_MESSAGE_ID: Id = id_hex!("A3556A66B00276797FCE8A2742AB850F");
const KIND_READ_ID: Id = id_hex!("B663C15BB6F2BF591EA870386DD48537");
const KIND_PARTY_ID: Id = id_hex!("3AA2883528D3812067DFA1CD5DE5F8B8");

const PARTY_AGENT_ID: Id = id_hex!("5EBC44A9FC4C8444AA01DFA7AC315AD5");
const PARTY_USER_ID: Id = id_hex!("7A39EB8857D1912501DACDA4DB29077B");

const KIND_SPECS: [(Id, &str); 3] = [
    (KIND_MESSAGE_ID, KIND_MESSAGE_LABEL),
    (KIND_READ_ID, KIND_READ_LABEL),
    (KIND_PARTY_ID, KIND_PARTY_LABEL),
];

const PARTY_SPECS: [(Id, &str); 2] = [
    (PARTY_AGENT_ID, "agent"),
    (PARTY_USER_ID, "user"),
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

fn normalize_label(label: &str) -> Result<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        bail!("party label is empty");
    }
    Ok(trimmed.to_string())
}

fn default_party_id(label: &str) -> Option<Id> {
    if label.eq_ignore_ascii_case("user") {
        Some(PARTY_USER_ID)
    } else if label.eq_ignore_ascii_case("agent") {
        Some(PARTY_AGENT_ID)
    } else {
        None
    }
}

fn resolve_party_id(space: &TribleSet, label: &str) -> Result<Option<Id>> {
    if let Some(id) = default_party_id(label) {
        return Ok(Some(id));
    }
    let label = normalize_label(label)?;
    let mut matches = Vec::new();
    for (party_id, shortname) in find!(
        (party_id: Id, shortname: String),
        pattern!(&space, [{
            ?party_id @
            metadata::tag: &KIND_PARTY_ID,
            metadata::shortname: ?shortname,
        }])
    ) {
        if shortname == label {
            matches.push(party_id);
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => bail!("multiple parties match label '{label}'"),
    }
}

fn resolve_or_create_party_id(
    space: &TribleSet,
    change: &mut TribleSet,
    label: &str,
) -> Result<Id> {
    if let Some(id) = default_party_id(label) {
        return Ok(id);
    }
    if let Some(id) = resolve_party_id(space, label)? {
        return Ok(id);
    }

    let label = normalize_label(label)?;
    let party_id = ufoid();
    *change += entity! { &party_id @
        metadata::tag: &KIND_PARTY_ID,
        metadata::shortname: label,
    };
    Ok(*party_id)
}

#[derive(Parser)]
#[command(name = "local-messages", about = "Local messaging faculty for the agent")]
struct Cli {
    /// Path to the pile file to use
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch name for local messages
    #[arg(long, default_value = DEFAULT_BRANCH, global = true)]
    branch: String,
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
        #[arg(value_name = "TEXT", num_args = 1.., trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// List recent messages (latest first)
    List {
        /// Reader label (defaults to "user").
        #[arg(default_value = "user")]
        reader: String,
        /// Only show messages unread by the reader
        #[arg(long)]
        unread: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Mark a message as read
    Ack {
        id: String,
        /// Reader label (defaults to "user").
        #[arg(default_value = "user")]
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

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
}

fn open_repo(
    path: &Path,
    branch_name: &str,
) -> Result<(Repository<Pile<valueschemas::Blake3>>, Id)> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create pile dir {}: {e}", parent.display()))?;
    }

    let mut pile = Pile::<valueschemas::Blake3>::open(path)
        .map_err(|e| anyhow::anyhow!("open pile {}: {e:?}", path.display()))?;
    pile.restore()
        .map_err(|e| anyhow::anyhow!("restore pile {}: {e:?}", path.display()))?;

    let existing = find_branch_by_name(&mut pile, branch_name)?;
    let signing_key = SigningKey::generate(&mut OsRng);
    let mut repo = Repository::new(pile, signing_key);
    let branch_id = match existing {
        Some(id) => id,
        None => repo
            .create_branch(branch_name, None)
            .map_err(|e| anyhow::anyhow!("create branch: {e:?}"))?
            .release(),
    };
    Ok((repo, branch_id))
}

fn find_branch_by_name(
    pile: &mut Pile<valueschemas::Blake3>,
    branch_name: &str,
) -> Result<Option<Id>> {
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

fn ensure_metadata(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<TribleSet> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
    let mut change = TribleSet::new();

    let mut existing_kinds: HashSet<Id> = find!(
        (kind: Id),
        pattern!(&space, [{ ?kind @ metadata::shortname: _?name }])
    )
    .into_iter()
    .map(|(kind,)| kind)
    .collect();

    for (id, label) in KIND_SPECS {
        if !existing_kinds.contains(&id) {
            change += entity! { ExclusiveId::force_ref(&id) @ metadata::shortname: label };
            existing_kinds.insert(id);
        }
    }

    let existing_parties: HashSet<Id> = find!(
        (party: Id),
        pattern!(&space, [{ ?party @ metadata::tag: &KIND_PARTY_ID }])
    )
    .into_iter()
    .map(|(party,)| party)
    .collect();

    let party_named: HashSet<Id> = find!(
        (party: Id),
        pattern!(&space, [{ ?party @ metadata::shortname: _?name }])
    )
    .into_iter()
    .map(|(party,)| party)
    .collect();

    for (id, label) in PARTY_SPECS {
        if !existing_parties.contains(&id) {
            change += entity! { ExclusiveId::force_ref(&id) @
                metadata::tag: &KIND_PARTY_ID,
                metadata::shortname: label,
            };
        } else if !party_named.contains(&id) {
            change += entity! { ExclusiveId::force_ref(&id) @ metadata::shortname: label };
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

fn load_text(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
    handle: TextHandle,
) -> Result<String> {
    let view: View<str> = ws.get(handle).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(view.as_ref().to_string())
}

fn cmd_send(pile: &Path, branch: &str, text: Vec<String>, from: String, to: String) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
    let mut change = ensure_metadata(&mut ws)?;

    let now = epoch_interval(now_epoch());
    let message_id = ufoid();
    let text = text.join(" ");
    let body_handle = ws.put::<blobschemas::LongString, _>(text.clone());
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
    let from_id = resolve_or_create_party_id(&space, &mut change, &from)?;
    let to_id = resolve_or_create_party_id(&space, &mut change, &to)?;
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
        from,
        to,
        truncate_single_line(&text, 120)
    );
    repo.close()
        .map_err(|e| anyhow::anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn cmd_ack(pile: &Path, branch: &str, id: String, by: String) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
    let mut change = ensure_metadata(&mut ws)?;

    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;
    let message_id = resolve_message_id(&space, &id)?;
    let reader_id = resolve_or_create_party_id(&space, &mut change, &by)?;

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
    println!(
        "Marked {} as read by {}.",
        id_prefix(message_id),
        by
    );
    repo.close()
        .map_err(|e| anyhow::anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn cmd_list(pile: &Path, branch: &str, reader: String, unread: bool, limit: usize) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;

    let mut party_names: HashMap<Id, String> = HashMap::new();
    for (party_id, shortname) in find!(
        (party_id: Id, shortname: String),
        pattern!(&space, [{
            ?party_id @
            metadata::tag: &KIND_PARTY_ID,
            metadata::shortname: ?shortname,
        }])
    ) {
        party_names.insert(party_id, shortname);
    }

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
    let reader_id = resolve_party_id(&space, &reader)?
        .ok_or_else(|| anyhow::anyhow!("unknown party label '{reader}'"))?;
    let mut shown = 0usize;

    for msg in messages {
        let read = reads.get(&(msg.id, reader_id)).copied();
        if unread && read.is_some() {
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
        let status = if read.is_some() { "read" } else { "unread" };
        let age = format_age(now_key, msg.created_at);
        println!(
            "[{}] {} {} -> {} ({}) {}",
            id_prefix(msg.id),
            age,
            from_label,
            to_label,
            status,
            truncate_single_line(&msg.body, 120)
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
    repo.close()
        .map_err(|e| anyhow::anyhow!("close pile: {e:?}"))?;
    Ok(())
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

    match cmd {
        Command::Send { text, from, to } => cmd_send(&cli.pile, &cli.branch, text, from, to),
        Command::List {
            reader,
            unread,
            limit,
        } => cmd_list(&cli.pile, &cli.branch, reader, unread, limit),
        Command::Ack { id, by } => cmd_ack(&cli.pile, &cli.branch, id, by),
    }
}

fn emit_schema_to_atlas(pile_path: &Path) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile_path, ATLAS_BRANCH)?;
    let mut metadata = build_local_metadata(repo.storage_mut())
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
    repo.close()
        .map_err(|e| anyhow::anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn build_local_metadata<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<valueschemas::Blake3>,
{
    let mut metadata = TribleSet::new();

    metadata.union(<valueschemas::GenId as metadata::ConstMetadata>::describe(blobs)?);
    metadata.union(<valueschemas::NsTAIInterval as metadata::ConstMetadata>::describe(blobs)?);
    metadata.union(
        <valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString> as metadata::ConstMetadata>::describe(
            blobs,
        )?,
    );
    metadata.union(<blobschemas::LongString as metadata::ConstMetadata>::describe(blobs)?);

    metadata.union(describe_attribute(blobs, &local::from, "local_from")?);
    metadata.union(describe_attribute(blobs, &local::to, "local_to")?);
    metadata.union(describe_attribute(blobs, &local::body, "local_body")?);
    metadata.union(describe_attribute(
        blobs,
        &local::created_at,
        "local_created_at",
    )?);
    metadata.union(describe_attribute(
        blobs,
        &local::about_message,
        "local_about_message",
    )?);
    metadata.union(describe_attribute(blobs, &local::reader, "local_reader")?);
    metadata.union(describe_attribute(blobs, &local::read_at, "local_read_at")?);

    metadata.union(describe_kind(
        blobs,
        &KIND_MESSAGE_ID,
        "local_message",
        "Local message kind.",
    )?);
    metadata.union(describe_kind(
        blobs,
        &KIND_READ_ID,
        "local_read",
        "Local read receipt kind.",
    )?);
    metadata.union(describe_kind(
        blobs,
        &KIND_PARTY_ID,
        "local_party",
        "Local party kind.",
    )?);
    metadata.union(describe_kind(
        blobs,
        &PARTY_USER_ID,
        "local_party_user",
        "Local party: user.",
    )?);
    metadata.union(describe_kind(
        blobs,
        &PARTY_AGENT_ID,
        "local_party_agent",
        "Local party: agent.",
    )?);

    Ok(metadata)
}

fn describe_attribute<B, S>(
    blobs: &mut B,
    attribute: &Attribute<S>,
    name: &str,
) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<valueschemas::Blake3>,
    S: ValueSchema,
{
    let mut tribles = metadata::Metadata::describe(attribute, blobs)?;
    let handle = blobs.put::<blobschemas::LongString, _>(name.to_owned())?;
    let attribute_id = metadata::Metadata::id(attribute);
    tribles += entity! { ExclusiveId::force_ref(&attribute_id) @
        metadata::shortname: name,
        metadata::name: handle,
    };
    Ok(tribles)
}

fn describe_kind<B>(
    blobs: &mut B,
    id: &Id,
    shortname: &str,
    description: &str,
) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<valueschemas::Blake3>,
{
    let handle = blobs.put::<blobschemas::LongString, _>(description.to_string())?;
    Ok(entity! { ExclusiveId::force_ref(id) @
        metadata::shortname: shortname,
        metadata::name: handle,
    })
}
