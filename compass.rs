#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive", "env"] }
//! ed25519-dalek = "2.1.1"
//! rand_core = "0.6.4"
//! time = { version = "0.3.36", features = ["formatting", "macros"] }
//! triblespace = "0.18"
//! ```

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use time::macros::format_description;
use time::OffsetDateTime;
use triblespace::core::metadata;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::macros::id_hex;
use triblespace::prelude::*;

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

#[derive(Parser)]
#[command(name = "compass", about = "A small TribleSpace kanban faculty")]
struct Cli {
    /// Path to the pile file to use
    #[arg(long, env = "PILE", global = true)]
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
        #[arg(help = "Goal title. Use @path for file input or @- for stdin.")]
        title: String,
        #[arg(long, default_value = "todo")]
        status: String,
        /// Parent goal id (prefix accepted)
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long, help = "Initial note. Use @path for file input or @- for stdin.")]
        note: Option<String>,
    },
    /// List goals in kanban columns (hides done by default)
    List {
        /// Show done goals too
        #[arg(long)]
        all: bool,
        /// Filter by tag (repeatable, shows goals matching any)
        #[arg(long)]
        tag: Vec<String>,
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
        #[arg(help = "Note text. Use @path for file input or @- for stdin.")]
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

fn now_stamp() -> String {
    let format = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    OffsetDateTime::now_utc()
        .format(&format)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
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

fn fmt_id(id: Id) -> String {
    format!("{id:x}")
}

/// Extract `[text](faculty:<hex>)` markdown link references from text.
/// Returns (faculty, hex_string) pairs.
fn extract_references(text: &str) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    let mut rest = text;
    while let Some(paren) = rest.find("](") {
        let after = &rest[paren + 2..];
        let end = after.find(')').unwrap_or(after.len());
        let link = &after[..end];
        if let Some(colon) = link.find(':') {
            let faculty = &link[..colon];
            let hex: String = link[colon + 1..]
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if hex.len() >= 4
                && !faculty.is_empty()
                && faculty
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                refs.push((faculty.to_string(), hex));
            }
        }
        rest = &after[end.min(after.len()).max(1)..];
    }
    refs.sort();
    refs.dedup();
    refs
}

fn load_value_or_file(raw: &str, label: &str) -> Result<String> {
    if let Some(path) = raw.strip_prefix('@') {
        if path == "-" {
            let mut value = String::new();
            std::io::stdin()
                .read_to_string(&mut value)
                .with_context(|| format!("read {label} from stdin"))?;
            return Ok(value);
        }
        return fs::read_to_string(path).with_context(|| format!("read {label} from {path}"));
    }
    Ok(raw.to_string())
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
    Repository::new(pile, signing_key, TribleSet::new())
        .map_err(|err| anyhow::anyhow!("create repository: {err:?}"))
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

fn render_board(state: &BoardState, status_filter: &[String], tag_filter: &[String], show_done: bool) {
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

        if !tag_filter.is_empty() && !task.tags.iter().any(|t| tag_filter.contains(t)) {
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
                row.id_hex,
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
    id_hex: String,
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
            id_hex: fmt_id(task.id),
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
    _branch_name: &str,
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
        let title_handle = ws.put(title);

        let mut change = TribleSet::new();
        change += ensure_kind_entities(&mut ws)?;
        change += entity! { &task_id @
            metadata::tag: &KIND_GOAL_ID,
            board::title: title_handle,
            board::created_at: now.as_str(),
            board::parent?: parent_id.as_ref(),
            board::tag*: tags.iter().map(|tag| tag.as_str()),
        };

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

        ws.commit(change, "add goal");
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push goal: {e:?}"))?;
        Ok(task_ref)
    })?;
    println!("Added goal {:x}", task_ref);
    Ok(())
}

fn cmd_list(
    pile: &Path,
    _branch_name: &str,
    branch_id: Id,
    status_filter: Vec<String>,
    tag_filter: Vec<String>,
    show_done: bool,
) -> Result<()> {
    let status_filter: Vec<String> = status_filter.into_iter().map(normalize_status).collect();
    for status in &status_filter {
        validate_short("status", status)?;
    }

    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;
        let board = load_board(&mut ws)?;
        render_board(&board, &status_filter, &tag_filter, show_done);
        Ok(())
    })
}

fn cmd_move(
    pile: &Path,
    _branch_name: &str,
    branch_id: Id,
    id: String,
    status: String,
) -> Result<()> {
    let status = normalize_status(status);
    validate_short("status", &status)?;

    let task_id = with_repo(pile, |repo| {
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

        ws.commit(change, "move goal");
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push status: {e:?}"))?;
        Ok(task_id)
    })?;
    println!("Moved goal {:x} to {}", task_id, status);
    Ok(())
}

fn cmd_note(pile: &Path, _branch_name: &str, branch_id: Id, id: String, note: String) -> Result<()> {
    let task_id = with_repo(pile, |repo| {
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

        ws.commit(change, "add goal note");
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push note: {e:?}"))?;
        Ok(task_id)
    })?;
    println!("Noted goal {:x}", task_id);
    Ok(())
}

fn cmd_show(pile: &Path, _branch_name: &str, branch_id: Id, id: String) -> Result<()> {
    with_repo(pile, |repo| {
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
            let parent_hex = fmt_id(parent_id);
            let parent_label = board
                .tasks
                .get(&parent_id)
                .map(|parent| format!("{} ({parent_hex})", parent.title))
                .unwrap_or_else(|| parent_hex.clone());
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
            for ev in &notes {
                println!("- {} {}", ev.at, ev.note);
            }

            // Collect references from all notes.
            let mut all_refs = Vec::new();
            for ev in &notes {
                all_refs.extend(extract_references(&ev.note));
            }
            all_refs.sort();
            all_refs.dedup();
            if !all_refs.is_empty() {
                println!();
                println!("References:");
                for (faculty, hex) in &all_refs {
                    println!("  ⇢ {faculty}:{hex}");
                }
            }
        }
        Ok(())
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(cmd) = cli.command else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };
    let branch_id = with_repo(&cli.pile, |repo| {
        if let Some(hex) = cli.branch_id.as_deref() {
            return Id::from_hex(hex.trim())
                .ok_or_else(|| anyhow::anyhow!("invalid branch id '{hex}'"));
        }
        repo.ensure_branch(&cli.branch, None)
            .map_err(|e| anyhow::anyhow!("ensure branch '{}': {e:?}", cli.branch))
    })?;

    match cmd {
        Command::Add {
            title,
            status,
            parent,
            tag,
            note,
        } => {
            let title = load_value_or_file(&title, "goal title")?;
            let note = note
                .as_deref()
                .map(|value| load_value_or_file(value, "goal note"))
                .transpose()?;
            cmd_add(
                &cli.pile,
                &cli.branch,
                branch_id,
                title,
                status,
                parent,
                tag,
                note,
            )
        }
        Command::List { status, tag, all } => cmd_list(&cli.pile, &cli.branch, branch_id, status, tag, all),
        Command::Move { id, status } => cmd_move(&cli.pile, &cli.branch, branch_id, id, status),
        Command::Note { id, note } => {
            let note = load_value_or_file(&note, "goal note")?;
            cmd_note(&cli.pile, &cli.branch, branch_id, id, note)
        }
        Command::Show { id } => cmd_show(&cli.pile, &cli.branch, branch_id, id),
    }
}
