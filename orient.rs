#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4.2.3"
//! rand_core = "0.6.4"
//! triblespace = "0.12.0"
//! ```

use anyhow::{Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand};
use hifitime::Epoch;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::core::metadata;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::*;

const DEFAULT_COMPASS_BRANCH: &str = "compass";
const DEFAULT_LOCAL_BRANCH: &str = "local-messages";
const DEFAULT_RELATIONS_BRANCH: &str = "relations";
const ATLAS_BRANCH: &str = "atlas";
const CONFIG_BRANCH: &str = "config";

const KIND_MESSAGE_ID: Id = id_hex!("A3556A66B00276797FCE8A2742AB850F");
const KIND_READ_ID: Id = id_hex!("B663C15BB6F2BF591EA870386DD48537");
const KIND_PERSON_ID: Id = id_hex!("D8ADDE47121F4E7868017463EC860726");

const KIND_GOAL_ID: Id = id_hex!("83476541420F46402A6A9911F46FBA3B");
const KIND_STATUS_ID: Id = id_hex!("89602B3277495F4E214D4A417C8CF260");


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
        "D1DC11B303725409AB8A30C6B59DB2D7" as persona_id: valueschemas::GenId;
    }
}

const CONFIG_KIND_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");

mod board {
    use super::*;
    attributes! {
        "EE18CEC15C18438A2FAB670E2E46E00C" as title: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "F9B56611861316B31A6C510B081C30B3" as created_at: valueschemas::ShortString;
        "5FF4941DCC3F6C35E9B3FD57216F69ED" as tag: valueschemas::ShortString;
        "9D2B6EBDA67E9BB6BE6215959D182041" as parent: valueschemas::GenId;

        "C1EAAA039DA7F486E4A54CC87D42E72C" as task: valueschemas::GenId;
        "61C44E0F8A73443ED592A713151E99A4" as status: valueschemas::ShortString;
        "8200ADEDC8D4D3D6D01CDC7396DF9AEC" as at: valueschemas::ShortString;
    }
}

#[derive(Parser)]
#[command(name = "orient", about = "Orient the agent with recent messages and goals")]
struct Cli {
    /// Path to the pile file to use
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Compass branch name
    #[arg(long, default_value = DEFAULT_COMPASS_BRANCH, global = true)]
    compass_branch: String,
    /// Local messages branch name
    #[arg(long, default_value = DEFAULT_LOCAL_BRANCH, global = true)]
    local_branch: String,
    /// Relations branch name
    #[arg(long, default_value = DEFAULT_RELATIONS_BRANCH, global = true)]
    relations_branch: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show an orientation snapshot
    Show {
        /// Max local messages to show
        #[arg(long, default_value_t = 10)]
        message_limit: usize,
        /// Max doing goals to show
        #[arg(long, default_value_t = 5)]
        doing_limit: usize,
        /// Max todo goals to show
        #[arg(long, default_value_t = 5)]
        todo_limit: usize,
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

#[derive(Debug, Clone)]
struct Task {
    id: Id,
    title: String,
    created_at: String,
    tags: Vec<String>,
    parent: Option<Id>,
}

#[derive(Debug, Clone)]
struct StatusEvent {
    task: Id,
    status: String,
    at: String,
}

#[derive(Debug, Clone)]
struct BoardState {
    tasks: HashMap<Id, Task>,
    status_events: Vec<StatusEvent>,
}

#[derive(Debug, Clone, Default)]
struct ConfigIdentity {
    persona_id: Option<Id>,
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
    } else if delta_s < 60 * 60 * 24 {
        format!("{}h", delta_s / (60 * 60))
    } else {
        format!("{}d", delta_s / (60 * 60 * 24))
    }
}

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
}

fn load_relations_labels(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch: &str,
) -> Result<HashMap<Id, String>> {
    let Some(branch_id) = find_branch_by_name(repo.storage_mut(), branch)? else {
        bail!("missing relations branch '{branch}' (create with relations faculty)");
    };
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull relations workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout relations: {e:?}"))?;
    let mut labels = HashMap::new();
    for (person_id, handle) in find!(
        (person_id: Id, handle: TextHandle),
        pattern!(&space, [{
            ?person_id @
            metadata::tag: &KIND_PERSON_ID,
            metadata::name: ?handle,
        }])
    ) {
        let Ok(label) = read_text(&mut ws, handle) else {
            continue;
        };
        labels.insert(person_id, label);
    }
    Ok(labels)
}

fn read_text(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
    handle: TextHandle,
) -> Result<String> {
    let view: View<str> = ws
        .get::<View<str>, blobschemas::LongString>(handle)
        .map_err(|e| anyhow!("load longstring: {e:?}"))?;
    Ok(view.to_string())
}

fn load_messages(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
    space: &TribleSet,
) -> Result<Vec<MessageRow>> {
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
        let body_text = read_text(ws, body)?;
        messages.push(MessageRow {
            id: message_id,
            from,
            to,
            body: body_text,
            created_at: interval_key(created_at),
        });
    }
    messages.sort_by_key(|msg| msg.created_at);
    messages.reverse();
    Ok(messages)
}

fn load_reads(space: &TribleSet) -> HashMap<(Id, Id), i128> {
    let mut reads = HashMap::new();
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
    reads
}

fn load_board(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<BoardState> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout board: {e:?}"))?;

    let mut tasks = HashMap::new();
    for (task_id, title_handle, created_at) in find!(
        (task: Id, title: TextHandle, created: String),
        pattern!(&space, [{
            ?task @
                metadata::tag: &KIND_GOAL_ID,
                board::title: ?title,
                board::created_at: ?created
        }])
    ) {
        if tasks.contains_key(&task_id) {
            continue;
        }
        let title = read_text(ws, title_handle)?;
        tasks.insert(
            task_id,
            Task {
                id: task_id,
                title,
                created_at,
                tags: Vec::new(),
                parent: None,
            },
        );
    }

    for (task_id, tag) in find!(
        (task: Id, tag: String),
        pattern!(&space, [{ ?task @ metadata::tag: &KIND_GOAL_ID, board::tag: ?tag }])
    ) {
        if let Some(task) = tasks.get_mut(&task_id) {
            task.tags.push(tag);
        }
    }

    for (task_id, parent_id) in find!(
        (task: Id, parent: Id),
        pattern!(&space, [{ ?task @ metadata::tag: &KIND_GOAL_ID, board::parent: ?parent }])
    ) {
        if let Some(task) = tasks.get_mut(&task_id) {
            task.parent = Some(parent_id);
        }
    }

    let mut status_events = Vec::new();
    for (task_id, status, at) in find!(
        (task: Id, status: String, at: String),
        pattern!(&space, [{
            _?evt @
                metadata::tag: &KIND_STATUS_ID,
                board::task: ?task,
                board::status: ?status,
                board::at: ?at
        }])
    ) {
        status_events.push(StatusEvent { task: task_id, status, at });
    }

    Ok(BoardState { tasks, status_events })
}

fn latest_status(events: &[StatusEvent]) -> HashMap<Id, StatusEvent> {
    let mut latest = HashMap::new();
    for event in events {
        latest
            .entry(event.task)
            .and_modify(|current: &mut StatusEvent| {
                if event.at > current.at {
                    *current = event.clone();
                }
            })
            .or_insert_with(|| event.clone());
    }
    latest
}

fn load_config_identity(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
) -> Result<ConfigIdentity> {
    let Some(branch_id) = find_branch_by_name(repo.storage_mut(), CONFIG_BRANCH)? else {
        return Ok(ConfigIdentity::default());
    };
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull config workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout config: {e:?}"))?;

    let mut latest: Option<(Id, Value<valueschemas::NsTAIInterval>)> = None;
    for (config_id, updated_at) in find!(
        (config_id: Id, updated_at: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{
            ?config_id @
            config_schema::kind: &CONFIG_KIND_ID,
            config_schema::updated_at: ?updated_at,
        }])
    ) {
        let key = interval_key(updated_at);
        match latest {
            Some((_, current)) if interval_key(current) >= key => {}
            _ => latest = Some((config_id, updated_at)),
        }
    }

    let Some((config_id, _)) = latest else {
        return Ok(ConfigIdentity::default());
    };

    let persona_id = find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config_schema::persona_id: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == config_id).then_some(Id::from_value(&value)));

    Ok(ConfigIdentity { persona_id })
}

fn cmd_show(
    pile: &Path,
    compass_branch: &str,
    local_branch: &str,
    relations_branch: &str,
    message_limit: usize,
    doing_limit: usize,
    todo_limit: usize,
) -> Result<()> {
    let mut repo = open_repo(pile)?;
    let local_branch_id = ensure_branch(&mut repo, local_branch)?;
    let compass_branch_id = ensure_branch(&mut repo, compass_branch)?;

    let mut local_ws = repo
        .pull(local_branch_id)
        .map_err(|e| anyhow!("pull local workspace: {e:?}"))?;
    let local_space = local_ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout local: {e:?}"))?;
    let config_identity = load_config_identity(&mut repo)?;
    let reader_id = config_identity
        .persona_id
        .ok_or_else(|| anyhow!("missing persona_id in config (set via `playground config set persona-id <hex-id>`)"))?;
    let party_names = load_relations_labels(&mut repo, relations_branch)?;
    if !party_names.contains_key(&reader_id) {
        bail!("persona_id {:x} missing from relations (add via relations faculty)", reader_id);
    }
    let reads = load_reads(&local_space);
    let mut messages = load_messages(&mut local_ws, &local_space)?;

    let mut unread: Vec<MessageRow> = messages
        .iter()
        .filter(|msg| msg.to == reader_id && !reads.contains_key(&(msg.id, reader_id)))
        .cloned()
        .collect();
    let use_unread = !unread.is_empty();
    if use_unread {
        unread.truncate(message_limit);
        messages = unread;
    } else {
        messages.retain(|msg| msg.to == reader_id || msg.from == reader_id);
        messages.truncate(message_limit);
    }

    let now_key = interval_key(epoch_interval(now_epoch()));

    println!("Orient");
    let reader_label = party_names
        .get(&reader_id)
        .cloned()
        .unwrap_or_else(|| id_prefix(reader_id));
    println!(
        "Local messages ({} for {}):",
        if use_unread { "unread" } else { "recent" },
        reader_label
    );
    if messages.is_empty() {
        println!("- None");
    } else {
        for msg in &messages {
            let from_label = party_names
                .get(&msg.from)
                .cloned()
                .unwrap_or_else(|| id_prefix(msg.from));
            let to_label = party_names
                .get(&msg.to)
                .cloned()
                .unwrap_or_else(|| id_prefix(msg.to));
            let age = format_age(now_key, msg.created_at);
            let status = if reads.contains_key(&(msg.id, reader_id)) {
                "read"
            } else {
                "unread"
            };
            println!(
                "- [{}] {} {} -> {} ({}) {}",
                id_prefix(msg.id),
                age,
                from_label,
                to_label,
                status,
                truncate_single_line(&msg.body, 120)
            );
        }
    }

    drop(local_ws);

    let mut compass_ws = repo
        .pull(compass_branch_id)
        .map_err(|e| anyhow!("pull compass workspace: {e:?}"))?;
    let board = load_board(&mut compass_ws)?;
    let latest = latest_status(&board.status_events);

    let mut doing = Vec::new();
    let mut todo = Vec::new();
    for task in board.tasks.values() {
        let status = latest
            .get(&task.id)
            .map(|ev| ev.status.to_lowercase())
            .unwrap_or_else(|| "todo".to_string());
        let status_at = latest.get(&task.id).map(|ev| ev.at.clone());
        let sort_key = status_at.as_deref().unwrap_or(&task.created_at);
        if status == "doing" {
            doing.push((sort_key.to_string(), task.clone()));
        } else if status == "todo" {
            todo.push((sort_key.to_string(), task.clone()));
        }
    }

    let sort_tasks = |tasks: &mut Vec<(String, Task)>| {
        tasks.sort_by(|a, b| b.0.cmp(&a.0));
    };
    sort_tasks(&mut doing);
    sort_tasks(&mut todo);

    println!();
    println!("Compass:");
    if doing.is_empty() && todo.is_empty() {
        println!("- No goals.");
    } else {
        println!("Doing:");
        if doing.is_empty() {
            println!("- None");
        } else {
            for (_key, task) in doing.into_iter().take(doing_limit) {
                let tag_suffix = render_tags(&task.tags);
                println!("- [{}] {}{}", id_prefix(task.id), task.title, tag_suffix);
            }
        }
        println!("Todo:");
        if todo.is_empty() {
            println!("- None");
        } else {
            for (_key, task) in todo.into_iter().take(todo_limit) {
                let tag_suffix = render_tags(&task.tags);
                println!("- [{}] {}{}", id_prefix(task.id), task.title, tag_suffix);
            }
        }
    }

    drop(compass_ws);
    repo.close()
        .map_err(|e| anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn render_tags(tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut sorted = tags.to_vec();
    sorted.sort();
    sorted.dedup();
    format!(
        " {}",
        sorted
            .iter()
            .map(|tag| {
                if tag.starts_with('#') {
                    tag.to_string()
                } else {
                    format!("#{}", tag)
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn truncate_single_line(text: &str, max_len: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' {
            break;
        }
        out.push(ch);
        if out.len() >= max_len {
            out.push_str("...");
            break;
        }
    }
    out
}

fn open_repo(path: &Path) -> Result<Repository<Pile<valueschemas::Blake3>>> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }

    let mut pile = Pile::<valueschemas::Blake3>::open(path)
        .map_err(|e| anyhow!("open pile {}: {e:?}", path.display()))?;
    pile.restore()
        .map_err(|e| anyhow!("restore pile {}: {e:?}", path.display()))?;
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
    Ok(Repository::new(pile, signing_key))
}

fn ensure_branch(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_name: &str,
) -> Result<Id> {
    if let Some(branch_id) = find_branch_by_name(repo.storage_mut(), branch_name)? {
        return Ok(branch_id);
    }
    repo.create_branch(branch_name, None)
        .map_err(|e| anyhow!("create branch: {e:?}"))
        .map(|branch| branch.release())
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
        .map_err(|e| anyhow!("pile reader: {e:?}"))?;
    let iter = pile
        .branches()
        .map_err(|e| anyhow!("list branches: {e:?}"))?;

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

fn emit_schema_to_atlas(pile_path: &Path) -> Result<()> {
    let mut repo = open_repo(pile_path)?;
    let branch_id = ensure_branch(&mut repo, ATLAS_BRANCH)?;
    let mut metadata = TribleSet::new();

    metadata.union(<valueschemas::GenId as metadata::ConstMetadata>::describe(
        repo.storage_mut(),
    )?);
    metadata.union(
        <valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString> as metadata::ConstMetadata>::describe(
            repo.storage_mut(),
        )?,
    );
    metadata.union(<blobschemas::LongString as metadata::ConstMetadata>::describe(
        repo.storage_mut(),
    )?);
    metadata.union(<valueschemas::NsTAIInterval as metadata::ConstMetadata>::describe(
        repo.storage_mut(),
    )?);
    metadata.union(<valueschemas::ShortString as metadata::ConstMetadata>::describe(
        repo.storage_mut(),
    )?);

    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull atlas workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout atlas workspace: {e:?}"))?;
    let delta = metadata.difference(&space);
    if !delta.is_empty() {
        ws.commit(delta, None, Some("atlas schema metadata"));
        repo.push(&mut ws)
            .map_err(|e| anyhow!("push atlas metadata: {e:?}"))?;
    }
    repo.close()
        .map_err(|e| anyhow!("close pile: {e:?}"))?;
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
        Command::Show {
            message_limit,
            doing_limit,
            todo_limit,
        } => cmd_show(
            &cli.pile,
            &cli.compass_branch,
            &cli.local_branch,
            &cli.relations_branch,
            message_limit,
            doing_limit,
            todo_limit,
        ),
    }
}
