#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4.2.3"
//! rand_core = "0.6.4"
//! time = { version = "0.3.36", features = ["formatting", "macros"] }
//! triblespace = "0.16.0"
//! ```

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use time::macros::format_description;
use time::OffsetDateTime;
use triblespace::core::metadata;
use triblespace::core::repo::branch as branch_proto;
use triblespace::core::repo::{PushResult, Repository, Workspace};
use triblespace::macros::id_hex;
use triblespace::prelude::*;

const ATLAS_BRANCH: &str = "atlas";
const CONFIG_BRANCH_ID: Id = id_hex!("4790808CF044F979FC7C2E47FCCB4A64");
const CONFIG_KIND_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");
const KIND_GOAL_LABEL: &str = "goal";
const KIND_STATUS_LABEL: &str = "status";
const KIND_NOTE_LABEL: &str = "note";

const KIND_GOAL_ID: Id = id_hex!("83476541420F46402A6A9911F46FBA3B");
const KIND_STATUS_ID: Id = id_hex!("89602B3277495F4E214D4A417C8CF260");
const KIND_NOTE_ID: Id = id_hex!("D4E49A6F02A14E66B62076AE4C01715F");

const DEFAULT_STATUSES: [&str; 4] = ["todo", "doing", "blocked", "done"];

type TextHandle = Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>>;

const KIND_SPECS: [(Id, &str); 3] = [
    (KIND_GOAL_ID, KIND_GOAL_LABEL),
    (KIND_STATUS_ID, KIND_STATUS_LABEL),
    (KIND_NOTE_ID, KIND_NOTE_LABEL),
];

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
        "47351DF00B3DDA96CB305157CD53D781" as note: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
    }
}

mod config_schema {
    use super::*;
    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: valueschemas::GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: valueschemas::NsTAIInterval;
        "EDEFFF6AFF6318E44CCF6A602B012604" as compass_branch_id: valueschemas::GenId;
    }
}

#[derive(Parser)]
#[command(name = "compass", about = "A small TribleSpace kanban faculty")]
struct Cli {
    /// Path to the pile file to use
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch name for the board
    #[arg(long, default_value = "compass", global = true)]
    branch: String,
    /// Branch id for the board (hex). Overrides config.
    #[arg(long, global = true)]
    branch_id: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Add a new goal
    Add {
        title: String,
        #[arg(long, default_value = "todo")]
        status: String,
        /// Parent goal id (prefix accepted)
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// List goals in kanban columns (hides done by default)
    List {
        /// Show done goals too
        #[arg(long)]
        all: bool,
        #[arg(value_name = "STATUS")]
        status: Vec<String>,
    },
    /// Move a goal to a new status
    Move {
        id: String,
        status: String,
    },
    /// Add a note to a goal
    Note {
        id: String,
        note: String,
    },
    /// Show a goal with history and notes
    Show {
        id: String,
    },
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
struct NoteEvent {
    task: Id,
    note: String,
    at: String,
}

#[derive(Debug, Clone)]
struct BoardState {
    tasks: HashMap<Id, Task>,
    status_events: Vec<StatusEvent>,
    note_events: Vec<NoteEvent>,
}

#[derive(Debug, Clone, Default)]
struct ConfigBranches {
    compass_branch_id: Option<Id>,
}

fn now_stamp() -> String {
    let format = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    OffsetDateTime::now_utc()
        .format(&format)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn interval_key(interval: Value<valueschemas::NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.from_value();
    lower.to_tai_duration().total_nanoseconds()
}

fn validate_short(label: &str, value: &str) -> Result<()> {
    if value.as_bytes().len() > 32 {
        bail!("{label} exceeds 32 bytes: {value}");
    }
    if value.as_bytes().iter().any(|b| *b == 0) {
        bail!("{label} contains NUL bytes: {value}");
    }
    Ok(())
}

fn normalize_status(status: String) -> String {
    status.trim().to_lowercase()
}

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
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
        return Err(anyhow::anyhow!(
            "restore pile {}: {err:?}",
            path.display()
        ));
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

fn load_config_branches(repo: &mut Repository<Pile<valueschemas::Blake3>>) -> Result<ConfigBranches> {
    let Some(_) = repo
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
        if latest.as_ref().is_none_or(|(_, current)| key > *current) {
            latest = Some((config_id, key));
        }
    }
    let Some((config_id, _)) = latest else {
        return Ok(ConfigBranches::default());
    };
    let compass_branch_id = find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config_schema::compass_branch_id: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == config_id).then_some(value.from_value()));

    Ok(ConfigBranches { compass_branch_id })
}

fn resolve_branch_id(explicit_id: Option<Id>, configured_id: Option<Id>) -> Result<Id> {
    if let Some(id) = explicit_id {
        return Ok(id);
    }
    configured_id.ok_or_else(|| {
        anyhow::anyhow!(
            "missing compass branch id in config (set via `playground config set compass-branch-id <hex-id>`)"
        )
    })
}

fn find_branch_by_name(
    pile: &mut Pile<valueschemas::Blake3>,
    branch_name: &str,
) -> Result<Option<Id>> {
    let expected_name_handle = branch_name
        .to_owned()
        .to_blob()
        .get_handle::<valueschemas::Blake3>();
    let reader = pile
        .reader()
        .map_err(|e| anyhow::anyhow!("pile reader: {e:?}"))?;
    let iter = pile
        .branches()
        .map_err(|e| anyhow::anyhow!("list branches: {e:?}"))?;

    // Prefer branches that actually have a commit head set. This avoids
    // accidentally picking an empty duplicate branch when legacy metadata is
    // present in the pile.
    let mut best: Option<(bool, Id)> = None;

    for bid in iter {
        let bid = bid?;
        let Some(meta_handle) = pile.head(bid)? else {
            continue;
        };
        let meta: TribleSet = reader
            .get::<TribleSet, blobschemas::SimpleArchive>(meta_handle)
            .map_err(|e| anyhow::anyhow!("load branch metadata: {e:?}"))?;

        let matches = {
            let name = find!(
                (handle: TextHandle),
                pattern!(&meta, [{ metadata::name: ?handle }])
            )
            .into_iter()
            .map(|(handle,)| handle)
            .next();
            name == Some(expected_name_handle)
        };

        if !matches {
            continue;
        }

        let has_head = find!(
            (head: Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>>),
            pattern!(&meta, [{ triblespace::core::repo::head: ?head }])
        )
        .into_iter()
        .next()
        .is_some();

        match best {
            None => best = Some((has_head, bid)),
            Some((best_has_head, _)) if !best_has_head && has_head => best = Some((true, bid)),
            Some(_) => {}
        }
    }

    Ok(best.map(|(_, bid)| bid))
}

fn load_board(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<BoardState> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout board: {e:?}"))?;

    let mut tasks = HashMap::new();
    let task_rows: Vec<(Id, TextHandle, String)> = find!(
        (task: Id, title: TextHandle, created: String),
        pattern!(&space, [{
            ?task @
                metadata::tag: &KIND_GOAL_ID,
                board::title: ?title,
                board::created_at: ?created
        }])
    )
    .collect();

    for (task_id, title_handle, created_at) in task_rows {
        if tasks.contains_key(&task_id) {
            continue;
        }
        let title =
            read_text(ws, title_handle).map_err(|e| anyhow::anyhow!("read title: {e:?}"))?;
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
        status_events.push(StatusEvent {
            task: task_id,
            status,
            at,
        });
    }

    let mut note_events = Vec::new();
    for (task_id, note_handle, at) in find!(
        (task: Id, note: TextHandle, at: String),
        pattern!(&space, [{
            _?evt @
                metadata::tag: &KIND_NOTE_ID,
                board::task: ?task,
                board::note: ?note,
                board::at: ?at
        }])
    ) {
        let note = read_text(ws, note_handle).map_err(|e| anyhow::anyhow!("read note: {e:?}"))?;
        note_events.push(NoteEvent {
            task: task_id,
            note,
            at,
        });
    }

    Ok(BoardState {
        tasks,
        status_events,
        note_events,
    })
}

fn read_text(ws: &mut Workspace<Pile<valueschemas::Blake3>>, handle: TextHandle) -> Result<String> {
    let view: View<str> = ws
        .get::<View<str>, blobschemas::LongString>(handle)
        .map_err(|e| anyhow::anyhow!("load longstring: {e:?}"))?;
    Ok(view.to_string())
}

fn resolve_task_id(input: &str, tasks: &HashMap<Id, Task>) -> Result<Id> {
    let needle = input.trim().to_lowercase();
    if needle.is_empty() {
        bail!("goal id is empty");
    }
    let mut matches = Vec::new();
    for id in tasks.keys() {
        let hex = format!("{id:x}");
        if hex.starts_with(&needle) {
            matches.push(*id);
        }
    }
    match matches.len() {
        0 => bail!("no goal id matches '{input}'"),
        1 => Ok(matches[0]),
        _ => {
            let mut rendered: Vec<String> = matches.into_iter().map(|id| format!("{id:x}")).collect();
            rendered.sort();
            bail!("ambiguous goal id '{input}': {}", rendered.join(", "));
        }
    }
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

fn notes_by_task(events: &[NoteEvent]) -> HashMap<Id, Vec<NoteEvent>> {
    let mut notes = HashMap::<Id, Vec<NoteEvent>>::new();
    for event in events {
        notes.entry(event.task).or_default().push(event.clone());
    }
    notes
}

fn render_board(state: &BoardState, status_filter: &[String], show_done: bool) {
    let status_map = latest_status(&state.status_events);
    let note_map = notes_by_task(&state.note_events);
    let mut columns: HashMap<String, Vec<TaskRow>> = HashMap::new();

    for task in state.tasks.values() {
        let status_event = status_map.get(&task.id);
        let status = status_event
            .map(|ev| ev.status.clone())
            .unwrap_or_else(|| "todo".to_string());

        if status_filter.is_empty() {
            if !show_done && status == "done" {
                continue;
            }
        } else if !status_filter.iter().any(|s| s == &status) {
            continue;
        }

        let note_count = note_map.get(&task.id).map(|n| n.len()).unwrap_or(0);
        let status_at = status_event.map(|ev| ev.at.clone());

        columns
            .entry(status)
            .or_default()
            .push(TaskRow::from(task, status_at, note_count));
    }

    let mut ordered_statuses = Vec::new();
    for status in DEFAULT_STATUSES {
        if columns.contains_key(status) {
            ordered_statuses.push(status.to_string());
        }
    }
    let mut extras: Vec<String> = columns
        .keys()
        .filter(|s| !DEFAULT_STATUSES.contains(&s.as_str()))
        .cloned()
        .collect();
    extras.sort();
    ordered_statuses.extend(extras);

    if ordered_statuses.is_empty() {
        println!("No goals yet.");
        return;
    }

    for status in ordered_statuses {
        let rows = columns.remove(&status).unwrap_or_default();
        println!();
        println!("== {} ({}) ==", status.to_uppercase(), rows.len());
        let ordered = order_rows(rows);
        for (row, depth) in ordered {
            let indent = "  ".repeat(depth);
            println!(
                "{}- [{}] {}{}{}",
                indent,
                row.id_prefix,
                row.title,
                row.tag_suffix(),
                row.note_suffix()
            );
        }
    }
    println!();
}

#[derive(Debug, Clone)]
struct TaskRow {
    id: Id,
    id_prefix: String,
    title: String,
    tags: Vec<String>,
    created_at: String,
    status_at: Option<String>,
    note_count: usize,
    parent: Option<Id>,
}

impl TaskRow {
    fn from(task: &Task, status_at: Option<String>, note_count: usize) -> Self {
        let mut tags = task.tags.clone();
        tags.sort();
        tags.dedup();
        Self {
            id: task.id,
            id_prefix: id_prefix(task.id),
            title: task.title.clone(),
            tags,
            created_at: task.created_at.clone(),
            status_at,
            note_count,
            parent: task.parent,
        }
    }

    fn sort_key(&self) -> &str {
        self.status_at
            .as_deref()
            .unwrap_or(&self.created_at)
    }

    fn tag_suffix(&self) -> String {
        if self.tags.is_empty() {
            String::new()
        } else {
            format!(" {}", self.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "))
        }
    }

    fn note_suffix(&self) -> String {
        if self.note_count == 0 {
            String::new()
        } else if self.note_count == 1 {
            " (1 note)".to_string()
        } else {
            format!(" ({} notes)", self.note_count)
        }
    }
}

fn order_rows(rows: Vec<TaskRow>) -> Vec<(TaskRow, usize)> {
    let mut by_id: HashMap<Id, TaskRow> = HashMap::new();
    for row in rows {
        by_id.insert(row.id, row);
    }
    let ids: HashSet<Id> = by_id.keys().copied().collect();
    let mut children: HashMap<Id, Vec<Id>> = HashMap::new();
    let mut roots = Vec::new();

    for (id, row) in &by_id {
        if let Some(parent) = row.parent {
            if ids.contains(&parent) {
                children.entry(parent).or_default().push(*id);
                continue;
            }
        }
        roots.push(*id);
    }

    let sort_ids = |items: &mut Vec<Id>| {
        items.sort_by(|a, b| {
            let a_key = by_id.get(a).map(|row| row.sort_key()).unwrap_or("");
            let b_key = by_id.get(b).map(|row| row.sort_key()).unwrap_or("");
            b_key.cmp(a_key)
        });
    };

    sort_ids(&mut roots);
    for kids in children.values_mut() {
        sort_ids(kids);
    }

    let mut ordered = Vec::new();
    let mut visited = HashSet::new();

    fn walk(
        id: Id,
        depth: usize,
        by_id: &HashMap<Id, TaskRow>,
        children: &HashMap<Id, Vec<Id>>,
        visited: &mut HashSet<Id>,
        out: &mut Vec<(TaskRow, usize)>,
    ) {
        if !visited.insert(id) {
            return;
        }
        let Some(row) = by_id.get(&id) else {
            return;
        };
        out.push((row.clone(), depth));
        if let Some(kids) = children.get(&id) {
            for kid in kids {
                walk(*kid, depth + 1, by_id, children, visited, out);
            }
        }
    }

    for root in roots {
        walk(root, 0, &by_id, &children, &mut visited, &mut ordered);
    }

    for id in by_id.keys() {
        if !visited.contains(id) {
            walk(*id, 0, &by_id, &children, &mut visited, &mut ordered);
        }
    }

    ordered
}

fn ensure_kind_entities(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<TribleSet> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout board: {e:?}"))?;
    let existing: HashSet<Id> = find!(
        (kind: Id),
        pattern!(&space, [{ ?kind @ metadata::name: _?handle }])
    )
    .map(|(kind,)| kind)
    .collect();

    let mut change = TribleSet::new();
    for (id, label) in KIND_SPECS {
        if existing.contains(&id) {
            continue;
        }
        let name_handle = label
            .to_owned()
            .to_blob()
            .get_handle::<valueschemas::Blake3>();
        change += entity! { ExclusiveId::force_ref(&id) @ metadata::name: name_handle };
    }
    Ok(change)
}

fn cmd_add(
    pile: &Path,
    branch_name: &str,
    branch_id: Id,
    title: String,
    status: String,
    parent: Option<String>,
    tags: Vec<String>,
    note: Option<String>,
) -> Result<()> {
    let status = normalize_status(status);
    let tags: Vec<String> = tags.into_iter().map(|t| t.trim().to_string()).collect();
    validate_short("status", &status)?;
    for tag in &tags {
        validate_short("tag", tag)?;
    }

    let task_ref = with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch_name)?;
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
        let parent_id = match parent {
            Some(parent_input) => {
                let board = load_board(&mut ws)?;
                Some(resolve_task_id(&parent_input, &board.tasks)?)
            }
            None => None,
        };
        let task_id = ufoid();
        let task_ref = task_id.id;
        let now = now_stamp();

        let mut change = TribleSet::new();
        change += ensure_kind_entities(&mut ws)?;
        change += entity! { &task_id @
            metadata::tag: &KIND_GOAL_ID,
            board::title: ws.put(title),
            board::created_at: now.as_str(),
        };
        if let Some(parent_id) = parent_id {
            change += entity! { &task_id @ board::parent: &parent_id };
        }

        for tag in tags {
            change += entity! { &task_id @ board::tag: tag.as_str() };
        }

        let status_id = ufoid();
        change += entity! { &status_id @
            metadata::tag: &KIND_STATUS_ID,
            board::task: &task_ref,
            board::status: status.as_str(),
            board::at: now.as_str(),
        };

        if let Some(note) = note {
            let note_id = ufoid();
            change += entity! { &note_id @
                metadata::tag: &KIND_NOTE_ID,
                board::task: &task_ref,
                board::note: ws.put(note),
                board::at: now.as_str(),
            };
        }

        ws.commit(change, None, Some("add goal"));
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push goal: {e:?}"))?;
        Ok(task_ref)
    })?;
    println!("Added goal {:x}", task_ref);
    Ok(())
}

fn cmd_list(
    pile: &Path,
    branch_name: &str,
    branch_id: Id,
    status_filter: Vec<String>,
    show_done: bool,
) -> Result<()> {
    let status_filter: Vec<String> = status_filter.into_iter().map(normalize_status).collect();
    for status in &status_filter {
        validate_short("status", status)?;
    }

    with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch_name)?;
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
        let board = load_board(&mut ws)?;
        render_board(&board, &status_filter, show_done);
        Ok(())
    })
}

fn cmd_move(
    pile: &Path,
    branch_name: &str,
    branch_id: Id,
    id: String,
    status: String,
) -> Result<()> {
    let status = normalize_status(status);
    validate_short("status", &status)?;

    let task_id = with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch_name)?;
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
        let board = load_board(&mut ws)?;
        let task_id = resolve_task_id(&id, &board.tasks)?;
        let now = now_stamp();

        let status_id = ufoid();
        let mut change = TribleSet::new();
        change += ensure_kind_entities(&mut ws)?;
        change += entity! { &status_id @
            metadata::tag: &KIND_STATUS_ID,
            board::task: &task_id,
            board::status: status.as_str(),
            board::at: now.as_str(),
        };

        ws.commit(change, None, Some("move goal"));
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push status: {e:?}"))?;
        Ok(task_id)
    })?;
    println!("Moved goal {:x} to {}", task_id, status);
    Ok(())
}

fn cmd_note(pile: &Path, branch_name: &str, branch_id: Id, id: String, note: String) -> Result<()> {
    let task_id = with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch_name)?;
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
        let board = load_board(&mut ws)?;
        let task_id = resolve_task_id(&id, &board.tasks)?;
        let now = now_stamp();

        let note_id = ufoid();
        let mut change = TribleSet::new();
        change += ensure_kind_entities(&mut ws)?;
        change += entity! { &note_id @
            metadata::tag: &KIND_NOTE_ID,
            board::task: &task_id,
            board::note: ws.put(note),
            board::at: now.as_str(),
        };

        ws.commit(change, None, Some("add goal note"));
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push note: {e:?}"))?;
        Ok(task_id)
    })?;
    println!("Noted goal {:x}", task_id);
    Ok(())
}

fn cmd_show(pile: &Path, branch_name: &str, branch_id: Id, id: String) -> Result<()> {
    with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch_name)?;
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
        let board = load_board(&mut ws)?;
        let task_id = resolve_task_id(&id, &board.tasks)?;

        let task = board
            .tasks
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("goal missing"))?;
        let status_events: Vec<_> = board
            .status_events
            .iter()
            .filter(|ev| ev.task == task_id)
            .cloned()
            .collect();
        let note_events: Vec<_> = board
            .note_events
            .iter()
            .filter(|ev| ev.task == task_id)
            .cloned()
            .collect();

        let current_status = status_events
            .iter()
            .max_by(|a, b| a.at.cmp(&b.at))
            .cloned();

        println!("Goal {:x}", task_id);
        println!("Title: {}", task.title);
        println!("Created: {}", task.created_at);
        if let Some(status) = current_status {
            println!("Status: {} (since {})", status.status, status.at);
        }
        if !task.tags.is_empty() {
            let mut tags = task.tags.clone();
            tags.sort();
            tags.dedup();
            println!("Tags: {}", tags.join(", "));
        }
        if let Some(parent_id) = task.parent {
            let parent_prefix = id_prefix(parent_id);
            let parent_label = board
                .tasks
                .get(&parent_id)
                .map(|parent| format!("{} ({parent_prefix})", parent.title))
                .unwrap_or_else(|| parent_prefix.clone());
            println!("Parent: {}", parent_label);
        }

        if !status_events.is_empty() {
            let mut history = status_events;
            history.sort_by(|a, b| a.at.cmp(&b.at));
            println!();
            println!("Status history:");
            for ev in history {
                println!("- {} {}", ev.at, ev.status);
            }
        }

        if !note_events.is_empty() {
            let mut notes = note_events;
            notes.sort_by(|a, b| a.at.cmp(&b.at));
            println!();
            println!("Notes:");
            for ev in notes {
                println!("- {} {}", ev.at, ev.note);
            }
        }
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
    let explicit_branch_id = parse_optional_hex_id(cli.branch_id.as_deref(), "branch id")?;
    let cfg = with_repo(&cli.pile, load_config_branches)?;
    let branch_id = resolve_branch_id(explicit_branch_id, cfg.compass_branch_id)?;

    match cmd {
        Command::Add {
            title,
            status,
            parent,
            tag,
            note,
        } => cmd_add(
            &cli.pile,
            &cli.branch,
            branch_id,
            title,
            status,
            parent,
            tag,
            note,
        ),
        Command::List { status, all } => cmd_list(&cli.pile, &cli.branch, branch_id, status, all),
        Command::Move { id, status } => cmd_move(&cli.pile, &cli.branch, branch_id, id, status),
        Command::Note { id, note } => cmd_note(&cli.pile, &cli.branch, branch_id, id, note),
        Command::Show { id } => cmd_show(&cli.pile, &cli.branch, branch_id, id),
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
        let metadata = build_compass_metadata(repo.storage_mut())
            .map_err(|e| anyhow::anyhow!("build compass metadata: {e:?}"))?;

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

fn build_compass_metadata<B>(
    blobs: &mut B,
) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<valueschemas::Blake3>,
{
    let mut metadata = TribleSet::new();

    metadata += <valueschemas::GenId as metadata::ConstDescribe>::describe(blobs)?;
    metadata += <valueschemas::ShortString as metadata::ConstDescribe>::describe(blobs)?;
    metadata +=
        <valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString> as metadata::ConstDescribe>::describe(
            blobs,
        )?;
    metadata += <blobschemas::LongString as metadata::ConstDescribe>::describe(blobs)?;

    metadata += metadata::Describe::describe(&board::title, blobs)?;
    metadata += metadata::Describe::describe(&board::created_at, blobs)?;
    metadata += metadata::Describe::describe(&board::tag, blobs)?;
    metadata += metadata::Describe::describe(&board::parent, blobs)?;
    metadata += metadata::Describe::describe(&board::task, blobs)?;
    metadata += metadata::Describe::describe(&board::status, blobs)?;
    metadata += metadata::Describe::describe(&board::at, blobs)?;
    metadata += metadata::Describe::describe(&board::note, blobs)?;

    metadata += describe_kind(
        blobs,
        &KIND_GOAL_ID,
        "compass_goal",
        "Compass goal kind.",
    )?;
    metadata += describe_kind(
        blobs,
        &KIND_STATUS_ID,
        "compass_status_kind",
        "Compass status kind.",
    )?;
    metadata += describe_kind(
        blobs,
        &KIND_NOTE_ID,
        "compass_note_kind",
        "Compass note kind.",
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
