#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! rand_core = "0.6.4"
//! triblespace = "0.18"
//! ```

use anyhow::{Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::core::metadata;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::*;

const DEFAULT_BRANCH: &str = "relations";

const KIND_PERSON_ID: Id = id_hex!("D8ADDE47121F4E7868017463EC860726");

type TextHandle = Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>>;

mod relations {
    use super::*;
    attributes! {
        "8F162B593D390E1424394DBF6883A72C" as alias: valueschemas::ShortString;
        "299E28A10114DC8C3B1661CD90CB8DF6" as label_norm: valueschemas::ShortString;
        "3E8812F6D22B2C93E2BCF0CE3C8C1979" as alias_norm: valueschemas::ShortString;
        "32B22FBA3EC2ADC3FFEB48483FE8961F" as affinity: valueschemas::ShortString;
        "F0AD0BBFAC4C4C899637573DC965622E" as first_name: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "764DD765142B3F4725B614BD3B9118EC" as last_name: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "DC0916CB5F640984EFE359A33105CA9A" as display_name: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "9B3329149D54CB9A8E8075E4AA862649" as teams_user_id: valueschemas::ShortString;
        "B563A063474CBE62ED25A8D0E9A1853C" as email: valueschemas::ShortString;
    }
}

#[derive(Parser)]
#[command(name = "relations", about = "Relationship/contacts faculty")]
struct Cli {
    /// Path to the pile file to use
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch name for relations data
    #[arg(long, default_value = DEFAULT_BRANCH, global = true)]
    branch: String,
    /// Branch id for relations data (hex). Overrides ensure_branch.
    #[arg(long, global = true)]
    branch_id: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Add a person
    Add {
        /// Canonical short label
        label: String,
        /// Explicit person id (hex)
        #[arg(long)]
        id: Option<String>,
        /// First name
        #[arg(long)]
        first_name: Option<String>,
        /// Last name
        #[arg(long)]
        last_name: Option<String>,
        /// Display name
        #[arg(long)]
        display_name: Option<String>,
        /// Affinity / relationship note (short)
        #[arg(long)]
        affinity: Option<String>,
        /// Note (long)
        #[arg(long)]
        note: Option<String>,
        /// Alias (repeatable)
        #[arg(long)]
        alias: Vec<String>,
        /// Teams user id (GUID)
        #[arg(long)]
        teams_user_id: Option<String>,
        /// Email address
        #[arg(long)]
        email: Option<String>,
    },
    /// Update a person
    Set {
        /// Person id (hex)
        id: String,
        /// New canonical short label
        #[arg(long)]
        label: Option<String>,
        /// First name
        #[arg(long)]
        first_name: Option<String>,
        /// Last name
        #[arg(long)]
        last_name: Option<String>,
        /// Display name
        #[arg(long)]
        display_name: Option<String>,
        /// Affinity / relationship note (short)
        #[arg(long)]
        affinity: Option<String>,
        /// Note (long)
        #[arg(long)]
        note: Option<String>,
        /// Alias (repeatable)
        #[arg(long)]
        alias: Vec<String>,
        /// Teams user id (GUID)
        #[arg(long)]
        teams_user_id: Option<String>,
        /// Email address
        #[arg(long)]
        email: Option<String>,
    },
    /// List people
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show a person
    Show { id: String },
}

#[derive(Debug, Clone)]
struct PersonRecord {
    id: Id,
    label: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    display_name: Option<String>,
    affinity: Option<String>,
    note: Option<String>,
    aliases: Vec<String>,
    teams_user_id: Option<String>,
    email: Option<String>,
}

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
}

fn normalize_label(label: &str) -> Result<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        bail!("label is empty");
    }
    Ok(trimmed.to_string())
}

fn normalize_lookup_key(value: &str) -> Result<String> {
    Ok(normalize_label(value)?.to_ascii_lowercase())
}

fn normalize_aliases(aliases: Vec<String>) -> Vec<String> {
    aliases
        .into_iter()
        .map(|alias| alias.trim().to_string())
        .filter(|alias| !alias.is_empty())
        .collect()
}

fn normalize_alias_lookup_keys(aliases: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for alias in aliases {
        let key = alias.trim().to_ascii_lowercase();
        if key.is_empty() || !seen.insert(key.clone()) {
            continue;
        }
        out.push(key);
    }
    out
}

fn parse_hex_id(raw: &str, label: &str) -> Result<Id> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{label} is empty");
    }
    Id::from_hex(trimmed).ok_or_else(|| anyhow!("invalid {label} {trimmed}"))
}

fn resolve_person_id(space: &TribleSet, raw: &str) -> Result<Id> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("person id is empty");
    }

    let prefix = trimmed.to_lowercase();
    if !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("person id must be hex (got '{trimmed}')");
    }

    if prefix.len() == 32 {
        let id = Id::from_hex(&prefix).ok_or_else(|| anyhow!("invalid person id {trimmed}"))?;
        for (person_id,) in find!(
            (person_id: Id),
            pattern!(&space, [{ ?person_id @ metadata::tag: &KIND_PERSON_ID }])
        ) {
            if person_id == id {
                return Ok(id);
            }
        }
        bail!("unknown person id {trimmed}");
    }

    let mut matches = Vec::new();
    for (person_id,) in find!(
        (person_id: Id),
        pattern!(&space, [{ ?person_id @ metadata::tag: &KIND_PERSON_ID }])
    ) {
        let hex = format!("{person_id:x}");
        if hex.starts_with(&prefix) {
            matches.push(person_id);
        }
    }

    match matches.len() {
        0 => bail!("no person id matches prefix '{trimmed}'"),
        1 => Ok(matches[0]),
        _ => bail!("multiple people match id prefix '{trimmed}'"),
    }
}

fn read_text(ws: &mut Workspace<Pile<valueschemas::Blake3>>, handle: TextHandle) -> Result<String> {
    let view: View<str> = ws
        .get(handle)
        .map_err(|e| anyhow!("load longstring: {e:?}"))?;
    Ok(view.to_string())
}

fn open_repo(path: &Path) -> Result<Repository<Pile<valueschemas::Blake3>>> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }
    let mut pile = Pile::<valueschemas::Blake3>::open(path)
        .map_err(|e| anyhow!("open pile {}: {e:?}", path.display()))?;
    if let Err(err) = pile.restore() {
        // Avoid Drop warnings on early errors.
        let _ = pile.close();
        return Err(anyhow!("restore pile {}: {err:?}", path.display()));
    }

    let signing_key = SigningKey::generate(&mut OsRng);
    Repository::new(pile, signing_key, TribleSet::new())
        .map_err(|err| anyhow!("create repository: {err:?}"))
}

fn with_repo<T>(
    pile: &Path,
    f: impl FnOnce(&mut Repository<Pile<valueschemas::Blake3>>) -> Result<T>,
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

fn ensure_kind_entities(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<TribleSet> {
    let space = ws.checkout(..).map_err(|e| anyhow!("checkout: {e:?}"))?;
    let existing: HashMap<Id, TextHandle> = find!(
        (kind: Id, name: TextHandle),
        pattern!(&space, [{ ?kind @ metadata::name: ?name }])
    )
    .into_iter()
    .collect();
    let mut change = TribleSet::new();
    if !existing.contains_key(&KIND_PERSON_ID) {
        let name_handle = "person"
            .to_owned()
            .to_blob()
            .get_handle::<valueschemas::Blake3>();
        change += entity! { ExclusiveId::force_ref(&KIND_PERSON_ID) @ metadata::name: name_handle };
    }
    Ok(change)
}

fn load_people(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<Vec<PersonRecord>> {
    let space = ws.checkout(..).map_err(|e| anyhow!("checkout: {e:?}"))?;
    let mut people: HashMap<Id, PersonRecord> = HashMap::new();

    for (person_id,) in find!(
        (person_id: Id),
        pattern!(&space, [{ ?person_id @ metadata::tag: &KIND_PERSON_ID }])
    ) {
        people.insert(
            person_id,
            PersonRecord {
                id: person_id,
                label: None,
                first_name: None,
                last_name: None,
                display_name: None,
                affinity: None,
                note: None,
                aliases: Vec::new(),
                teams_user_id: None,
                email: None,
            },
        );
    }

    for (person_id, handle) in find!(
        (person_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?person_id @ metadata::name: ?handle }])
    ) {
        if let Some(person) = people.get_mut(&person_id) {
            let value = read_text(ws, handle)?;
            if person.label.is_none() {
                person.label = Some(value);
            }
        }
    }

    for (person_id, handle) in find!(
        (person_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?person_id @ relations::display_name: ?handle }])
    ) {
        if let Some(person) = people.get_mut(&person_id) {
            let value = read_text(ws, handle)?;
            if person.display_name.is_none() {
                person.display_name = Some(value);
            }
        }
    }

    for (person_id, handle) in find!(
        (person_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?person_id @ relations::first_name: ?handle }])
    ) {
        if let Some(person) = people.get_mut(&person_id) {
            let value = read_text(ws, handle)?;
            if person.first_name.is_none() {
                person.first_name = Some(value);
            }
        }
    }

    for (person_id, handle) in find!(
        (person_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?person_id @ relations::last_name: ?handle }])
    ) {
        if let Some(person) = people.get_mut(&person_id) {
            let value = read_text(ws, handle)?;
            if person.last_name.is_none() {
                person.last_name = Some(value);
            }
        }
    }

    for (person_id, value) in find!(
        (person_id: Id, value: String),
        pattern!(&space, [{ ?person_id @ relations::affinity: ?value }])
    ) {
        if let Some(person) = people.get_mut(&person_id) {
            if person.affinity.is_none() {
                person.affinity = Some(value);
            }
        }
    }

    for (person_id, handle) in find!(
        (person_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?person_id @ metadata::description: ?handle }])
    ) {
        if let Some(person) = people.get_mut(&person_id) {
            let value = read_text(ws, handle)?;
            if person.note.is_none() {
                person.note = Some(value);
            }
        }
    }

    for (person_id, value) in find!(
        (person_id: Id, value: String),
        pattern!(&space, [{ ?person_id @ relations::teams_user_id: ?value }])
    ) {
        if let Some(person) = people.get_mut(&person_id) {
            if person.teams_user_id.is_none() {
                person.teams_user_id = Some(value);
            }
        }
    }

    for (person_id, value) in find!(
        (person_id: Id, value: String),
        pattern!(&space, [{ ?person_id @ relations::email: ?value }])
    ) {
        if let Some(person) = people.get_mut(&person_id) {
            if person.email.is_none() {
                person.email = Some(value);
            }
        }
    }

    for (person_id, value) in find!(
        (person_id: Id, value: String),
        pattern!(&space, [{ ?person_id @ relations::alias: ?value }])
    ) {
        if let Some(person) = people.get_mut(&person_id) {
            person.aliases.push(value);
        }
    }

    Ok(people.into_values().collect())
}

fn find_people_by_lookup_key(space: &TribleSet, key: &str) -> HashSet<Id> {
    let mut matches = HashSet::new();
    for (person_id,) in find!(
        (person_id: Id),
        pattern!(&space, [{
            ?person_id @
            metadata::tag: &KIND_PERSON_ID,
            relations::label_norm: key,
        }])
    ) {
        matches.insert(person_id);
    }
    for (person_id,) in find!(
        (person_id: Id),
        pattern!(&space, [{
            ?person_id @
            metadata::tag: &KIND_PERSON_ID,
            relations::alias_norm: key,
        }])
    ) {
        matches.insert(person_id);
    }
    matches
}

fn cmd_add(
    pile: &Path,
    branch_name: &str,
    branch_id: Id,
    label: String,
    id: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    display_name: Option<String>,
    affinity: Option<String>,
    note: Option<String>,
    aliases: Vec<String>,
    teams_user_id: Option<String>,
    email: Option<String>,
) -> Result<()> {
    let label = normalize_label(&label)?;
    let label_lookup = normalize_lookup_key(&label)?;
    let person_id = match id {
        Some(raw) => parse_hex_id(&raw, "person id")?,
        None => ufoid().id,
    };

    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
        let mut change = ensure_kind_entities(&mut ws)?;
        let space = ws.checkout(..).map_err(|e| anyhow!("checkout: {e:?}"))?;

        let aliases = normalize_aliases(aliases);
        let alias_lookup = normalize_alias_lookup_keys(&aliases);

        for existing in find_people_by_lookup_key(&space, &label_lookup) {
            if existing != person_id {
                bail!(
                    "lookup key '{label_lookup}' already belongs to person {}",
                    id_prefix(existing)
                );
            }
        }
        for key in &alias_lookup {
            for existing in find_people_by_lookup_key(&space, key) {
                if existing != person_id {
                    bail!(
                        "lookup key '{key}' already belongs to person {}",
                        id_prefix(existing)
                    );
                }
            }
        }

        let label_handle = ws.put(label.clone());
        let display_name_handle = display_name.map(|value| ws.put(value));
        let first_name_handle = first_name.map(|value| ws.put(value));
        let last_name_handle = last_name.map(|value| ws.put(value));
        let note_handle = note.map(|value| ws.put(value));
        change += entity! { ExclusiveId::force_ref(&person_id) @
            metadata::tag: &KIND_PERSON_ID,
            metadata::name: label_handle,
            relations::label_norm: label_lookup.as_str(),
            relations::display_name?: display_name_handle,
            relations::first_name?: first_name_handle,
            relations::last_name?: last_name_handle,
            relations::affinity?: affinity,
            metadata::description?: note_handle,
            relations::teams_user_id?: teams_user_id,
            relations::email?: email,
            relations::alias*: aliases.iter().map(String::as_str),
            relations::alias_norm*: alias_lookup.iter().map(String::as_str),
        };

        ws.commit(change, "relations add");
        repo.push(&mut ws)
            .map_err(|e| anyhow!("push person: {e:?}"))?;
        Ok(())
    })?;
    println!("Added {} ({label}).", format!("{person_id:x}"));
    Ok(())
}

fn cmd_set(
    pile: &Path,
    branch_name: &str,
    branch_id: Id,
    id: String,
    label: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    display_name: Option<String>,
    affinity: Option<String>,
    note: Option<String>,
    aliases: Vec<String>,
    teams_user_id: Option<String>,
    email: Option<String>,
) -> Result<()> {
    let label = label.map(|l| normalize_label(&l)).transpose()?;
    let label_lookup = label.as_deref().map(normalize_lookup_key).transpose()?;

    let person_id = with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
        let mut change = ensure_kind_entities(&mut ws)?;
        let space = ws.checkout(..).map_err(|e| anyhow!("checkout: {e:?}"))?;

        let person_id = resolve_person_id(&space, &id)?;

        let aliases = normalize_aliases(aliases);
        let alias_lookup = normalize_alias_lookup_keys(&aliases);

        if let Some(key) = label_lookup.as_deref() {
            for existing in find_people_by_lookup_key(&space, key) {
                if existing != person_id {
                    bail!(
                        "lookup key '{key}' already belongs to person {}",
                        id_prefix(existing)
                    );
                }
            }
        }
        for key in &alias_lookup {
            for existing in find_people_by_lookup_key(&space, key) {
                if existing != person_id {
                    bail!(
                        "lookup key '{key}' already belongs to person {}",
                        id_prefix(existing)
                    );
                }
            }
        }

        let label_handle = label.map(|value| ws.put(value));
        let display_name_handle = display_name.map(|value| ws.put(value));
        let first_name_handle = first_name.map(|value| ws.put(value));
        let last_name_handle = last_name.map(|value| ws.put(value));
        let note_handle = note.map(|value| ws.put(value));
        let has_updates = label_handle.is_some()
            || label_lookup.is_some()
            || display_name_handle.is_some()
            || first_name_handle.is_some()
            || last_name_handle.is_some()
            || affinity.is_some()
            || note_handle.is_some()
            || teams_user_id.is_some()
            || email.is_some()
            || !aliases.is_empty();

        if has_updates {
            change += entity! { ExclusiveId::force_ref(&person_id) @
                metadata::name?: label_handle,
                relations::label_norm?: label_lookup.as_deref(),
                relations::display_name?: display_name_handle,
                relations::first_name?: first_name_handle,
                relations::last_name?: last_name_handle,
                relations::affinity?: affinity,
                metadata::description?: note_handle,
                relations::teams_user_id?: teams_user_id,
                relations::email?: email,
                relations::alias*: aliases.iter().map(String::as_str),
                relations::alias_norm*: alias_lookup.iter().map(String::as_str),
            };
        }

        if !change.is_empty() {
            ws.commit(change, "relations set");
            repo.push(&mut ws)
                .map_err(|e| anyhow!("push person: {e:?}"))?;
        }
        Ok(person_id)
    })?;
    println!("Updated {}.", format!("{person_id:x}"));
    Ok(())
}

fn cmd_list(pile: &Path, branch_name: &str, branch_id: Id, limit: usize) -> Result<()> {
    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
        let mut people = load_people(&mut ws)?;
        people.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));

        if people.is_empty() {
            println!("No people.");
        } else {
            for person in people.into_iter().take(limit) {
                let label = person
                    .label
                    .clone()
                    .unwrap_or_else(|| "<unnamed>".to_string());
                let mut line = format!("[{}] {}", id_prefix(person.id), label);
                let fallback_name = match (&person.first_name, &person.last_name) {
                    (Some(first), Some(last)) => Some(format!("{first} {last}")),
                    (Some(first), None) => Some(first.clone()),
                    (None, Some(last)) => Some(last.clone()),
                    (None, None) => None,
                };
                let display = person.display_name.as_ref().or(fallback_name.as_ref());
                if let Some(display) = display {
                    line.push_str(&format!(" ({display})"));
                }
                if let Some(affinity) = &person.affinity {
                    line.push_str(&format!(" [{affinity}]"));
                }
                println!("{line}");
            }
        }
        Ok(())
    })
}

fn cmd_show(pile: &Path, branch_name: &str, branch_id: Id, id: String) -> Result<()> {
    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
        let space = ws.checkout(..).map_err(|e| anyhow!("checkout: {e:?}"))?;
        let person_id = resolve_person_id(&space, &id)?;
        let people = load_people(&mut ws)?;
        let Some(person) = people.into_iter().find(|p| p.id == person_id) else {
            bail!("unknown person id {id}");
        };

        println!("id: {:x}", person.id);
        if let Some(label) = person.label {
            println!("label: {label}");
        }
        if let Some(first) = person.first_name {
            println!("first_name: {first}");
        }
        if let Some(last) = person.last_name {
            println!("last_name: {last}");
        }
        if let Some(display) = person.display_name {
            println!("display_name: {display}");
        }
        if let Some(affinity) = person.affinity {
            println!("affinity: {affinity}");
        }
        if let Some(value) = person.teams_user_id {
            println!("teams_user_id: {value}");
        }
        if let Some(value) = person.email {
            println!("email: {value}");
        }
        if !person.aliases.is_empty() {
            println!("aliases:");
            for alias in person.aliases {
                println!("- {alias}");
            }
        }
        if let Some(note) = person.note {
            println!("note:");
            println!("{note}");
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
                .ok_or_else(|| anyhow!("invalid branch id '{hex}'"));
        }
        repo.ensure_branch(&cli.branch, None)
            .map_err(|e| anyhow!("ensure relations branch: {e:?}"))
    })?;

    match cmd {
        Command::Add {
            label,
            id,
            first_name,
            last_name,
            display_name,
            affinity,
            note,
            alias,
            teams_user_id,
            email,
        } => cmd_add(
            &cli.pile,
            &cli.branch,
            branch_id,
            label,
            id,
            first_name,
            last_name,
            display_name,
            affinity,
            note,
            alias,
            teams_user_id,
            email,
        ),
        Command::Set {
            id,
            label,
            first_name,
            last_name,
            display_name,
            affinity,
            note,
            alias,
            teams_user_id,
            email,
        } => cmd_set(
            &cli.pile,
            &cli.branch,
            branch_id,
            id,
            label,
            first_name,
            last_name,
            display_name,
            affinity,
            note,
            alias,
            teams_user_id,
            email,
        ),
        Command::List { limit } => cmd_list(&cli.pile, &cli.branch, branch_id, limit),
        Command::Show { id } => cmd_show(&cli.pile, &cli.branch, branch_id, id),
    }
}
