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
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::core::metadata;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::*;

const DEFAULT_BRANCH: &str = "relations";
const ATLAS_BRANCH: &str = "atlas";

const KIND_PERSON_ID: Id = id_hex!("D8ADDE47121F4E7868017463EC860726");

type TextHandle = Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>>;

mod relations {
    use super::*;
    attributes! {
        "8F162B593D390E1424394DBF6883A72C" as alias: valueschemas::ShortString;
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
    Show {
        id: String,
    },
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
    let view: View<str> = ws.get(handle).map_err(|e| anyhow!("load longstring: {e:?}"))?;
    Ok(view.to_string())
}

fn open_repo(path: &Path, branch_name: &str) -> Result<(Repository<Pile<valueschemas::Blake3>>, Id)> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }
    let mut pile = Pile::<valueschemas::Blake3>::open(path)
        .map_err(|e| anyhow!("open pile {}: {e:?}", path.display()))?;
    pile.restore()
        .map_err(|e| anyhow!("restore pile {}: {e:?}", path.display()))?;

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

fn find_branch_by_name(
    pile: &mut Pile<valueschemas::Blake3>,
    branch_name: &str,
) -> Result<Option<Id>> {
    let name_handle = branch_name
        .to_owned()
        .to_blob()
        .get_handle::<valueschemas::Blake3>();
    let reader = pile.reader().map_err(|e| anyhow!("pile reader: {e:?}"))?;
    let iter = pile.branches().map_err(|e| anyhow!("list branches: {e:?}"))?;
    for branch in iter {
        let branch_id = branch.map_err(|e| anyhow!("branch id: {e:?}"))?;
        let Some(head) = pile.head(branch_id).map_err(|e| anyhow!("branch head: {e:?}"))? else {
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

fn find_person_by_label(space: &TribleSet, label: &str) -> Result<Option<Id>> {
    let label = normalize_label(label)?;
    let mut matches = Vec::new();
    let label_handle = label.to_owned().to_blob().get_handle::<valueschemas::Blake3>();
    for (person_id,) in find!(
        (person_id: Id),
        pattern!(&space, [{
            ?person_id @
            metadata::tag: &KIND_PERSON_ID,
            metadata::name: label_handle,
        }])
    ) {
        matches.push(person_id);
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => bail!("multiple people match label '{label}'"),
    }
}

fn cmd_add(
    pile: &Path,
    branch: &str,
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
    let person_id = match id {
        Some(raw) => parse_hex_id(&raw, "person id")?,
        None => ufoid().id,
    };

    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
    let mut change = ensure_kind_entities(&mut ws)?;
    let space = ws.checkout(..).map_err(|e| anyhow!("checkout: {e:?}"))?;

    if let Some(existing) = find_person_by_label(&space, &label)? {
        if existing != person_id {
            bail!(
                "label '{label}' already belongs to person {}",
                id_prefix(existing)
            );
        }
    }

    let label_handle = ws.put(label.clone());
    change += entity! { ExclusiveId::force_ref(&person_id) @
        metadata::tag: &KIND_PERSON_ID,
        metadata::name: label_handle,
    };

    if let Some(display) = display_name {
        let handle = ws.put(display);
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::display_name: handle };
    }
    if let Some(value) = first_name {
        let handle = ws.put(value);
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::first_name: handle };
    }
    if let Some(value) = last_name {
        let handle = ws.put(value);
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::last_name: handle };
    }
    if let Some(value) = affinity {
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::affinity: value };
    }
    if let Some(value) = note {
        let handle = ws.put(value);
        change += entity! { ExclusiveId::force_ref(&person_id) @ metadata::description: handle };
    }
    if let Some(value) = teams_user_id {
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::teams_user_id: value };
    }
    if let Some(value) = email {
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::email: value };
    }
    for alias in aliases {
        let alias = alias.trim();
        if !alias.is_empty() {
            change += entity! { ExclusiveId::force_ref(&person_id) @ relations::alias: alias.to_string() };
        }
    }

    ws.commit(change, None, Some("relations add"));
    repo.push(&mut ws).map_err(|e| anyhow!("push person: {e:?}"))?;
    drop(ws);
    repo.close().map_err(|e| anyhow!("close pile: {e:?}"))?;
    println!("Added {} ({label}).", format!("{person_id:x}"));
    Ok(())
}

fn cmd_set(
    pile: &Path,
    branch: &str,
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

    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
    let mut change = ensure_kind_entities(&mut ws)?;
    let space = ws.checkout(..).map_err(|e| anyhow!("checkout: {e:?}"))?;

    let person_id = resolve_person_id(&space, &id)?;

    if let Some(label) = label {
        if let Some(existing) = find_person_by_label(&space, &label)? {
            if existing != person_id {
                bail!(
                    "label '{label}' already belongs to person {}",
                    id_prefix(existing)
                );
            }
        }
        let handle = ws.put(label);
        change += entity! { ExclusiveId::force_ref(&person_id) @ metadata::name: handle };
    }
    if let Some(display) = display_name {
        let handle = ws.put(display);
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::display_name: handle };
    }
    if let Some(value) = first_name {
        let handle = ws.put(value);
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::first_name: handle };
    }
    if let Some(value) = last_name {
        let handle = ws.put(value);
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::last_name: handle };
    }
    if let Some(value) = affinity {
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::affinity: value };
    }
    if let Some(value) = note {
        let handle = ws.put(value);
        change += entity! { ExclusiveId::force_ref(&person_id) @ metadata::description: handle };
    }
    if let Some(value) = teams_user_id {
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::teams_user_id: value };
    }
    if let Some(value) = email {
        change += entity! { ExclusiveId::force_ref(&person_id) @ relations::email: value };
    }
    for alias in aliases {
        let alias = alias.trim();
        if !alias.is_empty() {
            change += entity! { ExclusiveId::force_ref(&person_id) @ relations::alias: alias.to_string() };
        }
    }

    if !change.is_empty() {
        ws.commit(change, None, Some("relations set"));
        repo.push(&mut ws).map_err(|e| anyhow!("push person: {e:?}"))?;
    }
    drop(ws);
    repo.close().map_err(|e| anyhow!("close pile: {e:?}"))?;
    println!("Updated {}.", format!("{person_id:x}"));
    Ok(())
}

fn cmd_list(pile: &Path, branch: &str, limit: usize) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
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

    drop(ws);
    repo.close().map_err(|e| anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn cmd_show(pile: &Path, branch: &str, id: String) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
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

    drop(ws);
    repo.close().map_err(|e| anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn emit_schema_to_atlas(pile_path: &Path) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile_path, ATLAS_BRANCH)?;
    let mut metadata = TribleSet::new();

    metadata.union(<valueschemas::GenId as metadata::ConstMetadata>::describe(
        repo.storage_mut(),
    )?);
    metadata.union(<valueschemas::ShortString as metadata::ConstMetadata>::describe(
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

    metadata.union(describe_attribute(repo.storage_mut(), &metadata::name, "name")?);
    metadata.union(describe_attribute(repo.storage_mut(), &relations::alias, "relations_alias")?);
    metadata.union(describe_attribute(repo.storage_mut(), &relations::affinity, "relations_affinity")?);
    metadata.union(describe_attribute(
        repo.storage_mut(),
        &relations::first_name,
        "relations_first_name",
    )?);
    metadata.union(describe_attribute(
        repo.storage_mut(),
        &relations::last_name,
        "relations_last_name",
    )?);
    metadata.union(describe_attribute(
        repo.storage_mut(),
        &relations::display_name,
        "relations_display_name",
    )?);
    metadata.union(describe_attribute(repo.storage_mut(), &metadata::description, "description")?);
    metadata.union(describe_attribute(repo.storage_mut(), &relations::teams_user_id, "relations_teams_user_id")?);
    metadata.union(describe_attribute(repo.storage_mut(), &relations::email, "relations_email")?);

    metadata.union(describe_kind(
        repo.storage_mut(),
        &KIND_PERSON_ID,
        "person",
        "Relationship person entity.",
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
    let handle = blobs.put(name.to_owned())?;
    let attribute_id = metadata::Metadata::id(attribute);
    tribles += entity! { ExclusiveId::force_ref(&attribute_id) @
        metadata::name: handle,
    };
    Ok(tribles)
}

fn describe_kind<B>(
    blobs: &mut B,
    kind: &Id,
    name: &str,
    description: &str,
) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<valueschemas::Blake3>,
{
    let mut tribles = TribleSet::new();
    let name_handle = blobs.put(name.to_string())?;

    tribles += entity! { ExclusiveId::force_ref(kind) @
        metadata::name: name_handle,
        metadata::description: blobs.put(description.to_string())?,
    };
    Ok(tribles)
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
        Command::List { limit } => cmd_list(&cli.pile, &cli.branch, limit),
        Command::Show { id } => cmd_show(&cli.pile, &cli.branch, id),
    }
}
