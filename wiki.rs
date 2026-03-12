#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4.2.3"
//! rand_core = "0.6.4"
//! triblespace = "0.18"
//! ```

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use hifitime::efmt::Formatter;
use hifitime::efmt::consts::ISO8601_DATE;
use rand_core::OsRng;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use triblespace::core::metadata;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::macros::id_hex;
use triblespace::prelude::*;

// ── wiki branch name ──────────────────────────────────────────────────────
const WIKI_BRANCH_NAME: &str = "wiki";

// ── kinds ──────────────────────────────────────────────────────────────────
const KIND_VERSION_ID: Id = id_hex!("1AA0310347EDFED7874E8BFECC6438CF");

// ── tag vocabulary ────────────────────────────────────────────────────────
const TAG_ARCHIVED_ID: Id = id_hex!("480CB6A663C709478A26A8B49F366C3F");

const TAG_SPECS: [(Id, &str); 9] = [
    (KIND_VERSION_ID, "version"),
    (id_hex!("1A7FB717FBFCA81CA3AA7D3D186ACC8F"), "hypothesis"),
    (id_hex!("72CE6B03E39A8AAC37BC0C4015ED54E2"), "critique"),
    (id_hex!("243AE22C5E020F61EBBC8C0481BF05A4"), "finding"),
    (id_hex!("8871C1709EBFCDD2588369003D3964DE"), "paper"),
    (id_hex!("7D58EBA4E1E4A1EF868C3C4A58AEC22E"), "source"),
    (id_hex!("C86BCF906D270403A0A2083BB95B3552"), "concept"),
    (id_hex!("F8172CC4E495817AB52D2920199EF4BD"), "experiment"),
    (TAG_ARCHIVED_ID, "archived"),
];

type TextHandle = Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>>;

// ── wiki attributes ────────────────────────────────────────────────────────
mod wiki {
    use super::*;
    attributes! {
        "EBFC56D50B748E38A14F5FC768F1B9C1" as fragment: valueschemas::GenId;
        "6DBBE746B7DD7A4793CA098AB882F553" as content: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "476F6E26FCA65A0B49E38CC44CF31467" as created_at: valueschemas::NsTAIInterval;
        "78BABEF1792531A2E51A372D96FE5F3E" as title: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
    }
}

// ── CLI ────────────────────────────────────────────────────────────────────
#[derive(Parser)]
#[command(name = "wiki", about = "A TribleSpace knowledge wiki faculty")]
struct Cli {
    /// Path to the pile file
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch id (hex). Overrides name-based lookup.
    #[arg(long, global = true)]
    branch_id: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new fragment with its first version
    Create {
        /// Fragment title
        title: String,
        /// Content text. Use @path for file input or @- for stdin.
        content: String,
        /// Tags (by name). Unknown tags are minted automatically.
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Create a new version of an existing fragment
    Edit {
        /// Fragment or version id (prefix accepted)
        id: String,
        /// New content (optional; inherits previous if omitted). Use @path for file input or @- for stdin.
        content: Option<String>,
        /// New title (optional, inherits previous if omitted)
        #[arg(long)]
        title: Option<String>,
        /// Tags (replaces previous version's tags)
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Show a fragment (latest version) or a specific version
    Show {
        /// Fragment or version id (prefix accepted)
        id: String,
    },
    /// Print raw content without metadata header
    Export {
        /// Fragment or version id (prefix accepted)
        id: String,
    },
    /// Compare two versions of a fragment
    Diff {
        /// Fragment id (prefix accepted)
        id: String,
        /// First version number (1-based, default: second-to-last)
        #[arg(long)]
        from: Option<usize>,
        /// Second version number (1-based, default: latest)
        #[arg(long)]
        to: Option<usize>,
    },
    /// Soft-delete a fragment (adds #archived tag)
    Archive {
        /// Fragment id (prefix accepted)
        id: String,
    },
    /// Restore an archived fragment (removes #archived tag)
    Restore {
        /// Fragment id (prefix accepted)
        id: String,
    },
    /// Revert a fragment to a previous version
    Revert {
        /// Fragment id (prefix accepted)
        id: String,
        /// Version number to revert to (1-based)
        #[arg(long)]
        to: usize,
    },
    /// Show links from/to a fragment (extracted from content `id:<hex>` references)
    Links {
        /// Fragment id (prefix accepted)
        id: String,
    },
    /// List fragments, optionally filtered by tag
    List {
        /// Filter by tag name
        #[arg(long)]
        tag: Vec<String>,
        /// Include archived fragments
        #[arg(long)]
        all: bool,
    },
    /// Show version history for a fragment
    History {
        /// Fragment id (prefix accepted)
        id: String,
    },
    /// Tag management: add, remove, list, mint
    Tag {
        #[command(subcommand)]
        command: TagCommand,
    },
    /// Search fragment titles and content (substring, case-insensitive)
    Search {
        /// Search query
        query: String,
        /// Also show matching context lines
        #[arg(long, short = 'c')]
        context: bool,
        /// Include archived fragments
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum TagCommand {
    /// Add a tag to a fragment (creates a new version)
    Add {
        /// Fragment id (prefix accepted)
        id: String,
        /// Tag name
        name: String,
    },
    /// Remove a tag from a fragment (creates a new version)
    Remove {
        /// Fragment id (prefix accepted)
        id: String,
        /// Tag name
        name: String,
    },
    /// List all tags with usage counts
    List,
    /// Mint and register a new tag name
    Mint {
        /// Tag name
        name: String,
    },
}

// ── data types ─────────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
struct Version {
    id: Id,
    fragment_id: Id,
    title: String,
    content_handle: TextHandle,
    created_at: i128,
    tags: Vec<Id>,
}

/// Bidirectional tag name / id index.
struct TagIndex {
    by_name: HashMap<String, Id>,
    by_id: HashMap<Id, String>,
}

impl TagIndex {
    fn load(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<Self> {
        let space = ws
            .checkout(..)
            .map_err(|e| anyhow::anyhow!("checkout for tag names: {e:?}"))?;
        let mut by_name = HashMap::new();
        let mut by_id = HashMap::new();
        for (tag_id, handle) in find!(
            (tag_id: Id, handle: TextHandle),
            pattern!(&space, [{ ?tag_id @ metadata::name: ?handle }])
        ) {
            let view: View<str> = ws
                .get(handle)
                .map_err(|e| anyhow::anyhow!("read tag name: {e:?}"))?;
            let name = view.as_ref().to_string();
            by_name.insert(name.clone(), tag_id);
            by_id.insert(tag_id, name);
        }
        Ok(Self { by_name, by_id })
    }

    fn name(&self, id: Id) -> String {
        self.by_id
            .get(&id)
            .cloned()
            .unwrap_or_else(|| id_prefix(id))
    }

    fn format_tags(&self, tags: &[Id]) -> String {
        let names: Vec<String> = tags.iter().map(|t| self.name(*t)).collect();
        if names.is_empty() {
            String::new()
        } else {
            format!(" [{}]", names.join(", "))
        }
    }

    fn resolve_or_mint(
        &mut self,
        names: &[String],
        change: &mut TribleSet,
        ws: &mut Workspace<Pile<valueschemas::Blake3>>,
    ) -> Result<Vec<Id>> {
        let mut ids = Vec::new();
        for raw in names {
            let name = raw.trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            if let Some(&id) = self.by_name.get(&name) {
                ids.push(id);
            } else {
                let tag_id = ufoid();
                let tag_ref = tag_id.id;
                let name_handle = ws.put(name.clone());
                *change += entity! { &tag_id @ metadata::name: name_handle };
                self.by_name.insert(name.clone(), tag_ref);
                self.by_id.insert(tag_ref, name);
                ids.push(tag_ref);
            }
        }
        Ok(ids)
    }
}

// ── helpers ────────────────────────────────────────────────────────────────
fn now_tai() -> Value<valueschemas::NsTAIInterval> {
    let now = Epoch::now().unwrap_or(Epoch::from_unix_seconds(0.0));
    (now, now).to_value()
}

fn interval_key(interval: Value<valueschemas::NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.from_value();
    lower.to_tai_duration().total_nanoseconds()
}

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
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

/// Format TAI nanoseconds as ISO 8601 date (e.g. "2026-03-11").
fn format_date(tai_ns: i128) -> String {
    // Inverse of Duration::total_nanoseconds().
    const NANOS_PER_CENTURY: i128 = 3_155_760_000_000_000_000;
    let centuries = (tai_ns / NANOS_PER_CENTURY) as i16;
    let nanos = (tai_ns % NANOS_PER_CENTURY) as u64;
    let dur = hifitime::Duration::from_parts(centuries, nanos);
    let epoch = Epoch::from_tai_duration(dur);
    Formatter::new(epoch, ISO8601_DATE).to_string()
}

/// Extract `id:<hex>` references from content, resolving prefixes to fragment IDs.
fn extract_references(content: &str, versions: &[Version]) -> Vec<Id> {
    let mut refs = Vec::new();
    let mut rest = content;
    while let Some(pos) = rest.find("id:") {
        let after = &rest[pos + 3..];
        let hex: String = after.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() >= 4 {
            if let Ok(frag_id) = resolve_to_fragment_id(&hex, versions) {
                refs.push(frag_id);
            }
        }
        rest = &after[hex.len().max(1)..];
    }
    refs.sort();
    refs.dedup();
    refs
}

fn open_repo(path: &Path) -> Result<Repository<Pile<valueschemas::Blake3>>> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create pile dir {}: {e}", parent.display()))?;
    }
    let mut pile = Pile::<valueschemas::Blake3>::open(path)
        .map_err(|e| anyhow::anyhow!("open pile {}: {e:?}", path.display()))?;
    if let Err(err) = pile.restore() {
        let _ = pile.close();
        return Err(anyhow::anyhow!("restore pile {}: {err:?}", path.display()));
    }
    let signing_key = SigningKey::generate(&mut OsRng);
    Repository::new(pile, signing_key, TribleSet::new())
        .map_err(|err| anyhow::anyhow!("create repository: {err:?}"))
}

/// Open repo, resolve wiki branch, pull workspace, run closure, close.
fn with_wiki<T>(
    pile: &Path,
    explicit_branch: Option<&str>,
    f: impl FnOnce(
        &mut Repository<Pile<valueschemas::Blake3>>,
        &mut Workspace<Pile<valueschemas::Blake3>>,
    ) -> Result<T>,
) -> Result<T> {
    let mut repo = open_repo(pile)?;
    let branch_id = if let Some(hex) = explicit_branch {
        Id::from_hex(hex.trim()).ok_or_else(|| anyhow::anyhow!("invalid branch id '{hex}'"))?
    } else {
        repo.ensure_branch(WIKI_BRANCH_NAME, None)
            .map_err(|e| anyhow::anyhow!("ensure wiki branch: {e:?}"))?
    };
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow::anyhow!("pull wiki workspace: {e:?}"))?;
    let result = f(&mut repo, &mut ws);
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

/// Ensure all built-in tag/kind IDs have metadata::name entries.
fn ensure_tag_vocabulary(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
) -> Result<()> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout for tag names: {e:?}"))?;
    let existing: std::collections::HashSet<Id> = find!(
        (kind: Id),
        pattern!(&space, [{ ?kind @ metadata::name: _?handle }])
    )
    .map(|(kind,)| kind)
    .collect();

    let mut change = TribleSet::new();
    for (id, label) in TAG_SPECS {
        if existing.contains(&id) {
            continue;
        }
        let name_handle = ws.put(label.to_owned());
        change += entity! { ExclusiveId::force_ref(&id) @ metadata::name: name_handle };
    }

    if !change.is_empty() {
        ws.commit(change, "wiki: register tag names");
        repo.push(ws)
            .map_err(|e| anyhow::anyhow!("push tag names: {e:?}"))?;
    }
    Ok(())
}

/// Create a new version entity, commit, and push.
fn commit_version(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
    mut change: TribleSet,
    fragment_id: Id,
    title: &str,
    content: TextHandle,
    tags: &[Id],
    message: &str,
) -> Result<Id> {
    let mut tag_ids = tags.to_vec();
    tag_ids.push(KIND_VERSION_ID);
    tag_ids.sort();
    tag_ids.dedup();

    let title_handle = ws.put(title.to_owned());
    let version_id = ufoid();

    change += entity! { &version_id @
        wiki::fragment: &fragment_id,
        wiki::title: title_handle,
        wiki::content: content,
        wiki::created_at: now_tai(),
        metadata::tag*: tag_ids.iter(),
    };

    ws.commit(change, message);
    repo.push(ws).map_err(|e| anyhow::anyhow!("push: {e:?}"))?;
    Ok(version_id.id)
}

/// Versions of a single fragment, sorted oldest-first.
fn fragment_history<'a>(versions: &'a [Version], fragment_id: Id) -> Vec<&'a Version> {
    let mut history: Vec<&Version> = versions
        .iter()
        .filter(|v| v.fragment_id == fragment_id)
        .collect();
    history.sort_by_key(|v| v.created_at);
    history
}

/// Outgoing and incoming content-derived links for a fragment.
fn find_links(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
    fragment_id: Id,
    versions: &[Version],
    latest: &HashMap<Id, &Version>,
) -> Result<(Vec<Id>, Vec<Id>)> {
    let content: View<str> = ws
        .get(
            latest
                .get(&fragment_id)
                .ok_or_else(|| anyhow::anyhow!("no versions"))?
                .content_handle,
        )
        .map_err(|e| anyhow::anyhow!("read content: {e:?}"))?;
    let outgoing: Vec<Id> = extract_references(content.as_ref(), versions)
        .into_iter()
        .filter(|&id| id != fragment_id)
        .collect();

    let mut incoming = Vec::new();
    for (&frag_id, &v) in latest {
        if frag_id == fragment_id {
            continue;
        }
        let c: View<str> = ws
            .get(v.content_handle)
            .map_err(|e| anyhow::anyhow!("read content: {e:?}"))?;
        if extract_references(c.as_ref(), versions).contains(&fragment_id) {
            incoming.push(frag_id);
        }
    }
    incoming.sort();
    incoming.dedup();

    Ok((outgoing, incoming))
}

/// Load all versions from the wiki branch.
fn load_versions(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<Vec<Version>> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;

    let mut versions: HashMap<Id, Version> = HashMap::new();

    for (vid, frag, title_h, content_h, ts) in find!(
        (vid: Id, frag: Id, title_h: TextHandle, content_h: TextHandle, ts: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{
            ?vid @
            metadata::tag: &KIND_VERSION_ID,
            wiki::fragment: ?frag,
            wiki::title: ?title_h,
            wiki::content: ?content_h,
            wiki::created_at: ?ts,
        }])
    ) {
        let title: String = {
            let view: View<str> = ws
                .get(title_h)
                .map_err(|e| anyhow::anyhow!("read title: {e:?}"))?;
            view.as_ref().to_string()
        };
        versions.insert(
            vid,
            Version {
                id: vid,
                fragment_id: frag,
                title,
                content_handle: content_h,
                created_at: interval_key(ts),
                tags: Vec::new(),
            },
        );
    }

    // Tags (multi-valued)
    for (vid, tag_id) in find!(
        (vid: Id, tag_id: Id),
        pattern!(&space, [{
            ?vid @
            metadata::tag: &KIND_VERSION_ID,
            metadata::tag: ?tag_id,
        }])
    ) {
        if let Some(v) = versions.get_mut(&vid) {
            if tag_id != KIND_VERSION_ID {
                v.tags.push(tag_id);
            }
        }
    }

    Ok(versions.into_values().collect())
}

/// Get the latest version for each fragment.
fn latest_versions(versions: &[Version]) -> HashMap<Id, &Version> {
    let mut latest: HashMap<Id, &Version> = HashMap::new();
    for v in versions {
        if let Some(current) = latest.get(&v.fragment_id) {
            if v.created_at > current.created_at {
                latest.insert(v.fragment_id, v);
            }
        } else {
            latest.insert(v.fragment_id, v);
        }
    }
    latest
}

/// Resolve an id prefix, returning whichever ID matched (fragment or version).
fn resolve_id(input: &str, versions: &[Version]) -> Result<Id> {
    let needle = input.trim().to_lowercase();
    let mut version_matches = Vec::new();
    let mut fragment_matches = Vec::new();
    for v in versions {
        let vid_hex = format!("{:x}", v.id);
        let fid_hex = format!("{:x}", v.fragment_id);
        if vid_hex.starts_with(&needle) {
            version_matches.push(v.id);
        }
        if fid_hex.starts_with(&needle) {
            fragment_matches.push(v.fragment_id);
        }
    }
    fragment_matches.sort();
    fragment_matches.dedup();
    if fragment_matches.len() == 1 {
        return Ok(fragment_matches[0]);
    }
    if version_matches.len() == 1 {
        return Ok(version_matches[0]);
    }
    let total = fragment_matches.len() + version_matches.len();
    if total == 0 {
        bail!("no id matches '{input}'");
    }
    bail!("ambiguous id '{input}' ({total} matches)");
}

/// Resolve an id prefix to a fragment ID (maps version IDs to their fragment).
fn resolve_to_fragment_id(input: &str, versions: &[Version]) -> Result<Id> {
    let id = resolve_id(input, versions)?;
    if versions.iter().any(|v| v.fragment_id == id) {
        return Ok(id);
    }
    if let Some(v) = versions.iter().find(|v| v.id == id) {
        return Ok(v.fragment_id);
    }
    bail!("no fragment for '{input}'");
}

/// Resolve an id and pick the version to display (specific version if version
/// ID matched, otherwise latest for the fragment).
fn resolve_version_to_show<'a>(
    input: &str,
    versions: &'a [Version],
) -> Result<&'a Version> {
    let id = resolve_id(input, versions)?;
    // Direct version match — show that exact version.
    if let Some(v) = versions.iter().find(|v| v.id == id) {
        return Ok(v);
    }
    // Fragment match — show latest.
    let latest = latest_versions(versions);
    latest
        .get(&id)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("no versions for '{input}'"))
}

// ── commands ───────────────────────────────────────────────────────────────

fn cmd_create(
    pile: &Path,
    branch: Option<&str>,
    title: String,
    content: String,
    tags: Vec<String>,
) -> Result<()> {
    let title = load_value_or_file(&title, "title")?;
    let content = load_value_or_file(&content, "content")?;

    with_wiki(pile, branch, |repo, ws| {
        ensure_tag_vocabulary(repo, ws)?;
        let mut change = TribleSet::new();
        let mut tag_index = TagIndex::load(ws)?;
        let tag_ids = tag_index.resolve_or_mint(&tags, &mut change, ws)?;

        let fragment_id = ufoid();
        let fragment_ref = fragment_id.id;
        let content_handle = ws.put(content);
        let vid = commit_version(
            repo, ws, change, fragment_ref, &title, content_handle, &tag_ids, "wiki create",
        )?;

        println!("fragment {}", id_prefix(fragment_ref));
        println!("version  {}", id_prefix(vid));
        Ok(())
    })
}

fn cmd_edit(
    pile: &Path,
    branch: Option<&str>,
    id: String,
    content: Option<String>,
    new_title: Option<String>,
    tags: Vec<String>,
) -> Result<()> {
    let content = content.map(|c| load_value_or_file(&c, "content")).transpose()?;
    let new_title = new_title.map(|t| load_value_or_file(&t, "title")).transpose()?;
    if content.is_none() && new_title.is_none() && tags.is_empty() {
        bail!("nothing to change — provide content, --title, or --tag");
    }

    with_wiki(pile, branch, |repo, ws| {
        let versions = load_versions(ws)?;
        let fragment_id = resolve_to_fragment_id(&id, &versions)?;
        let prev = *latest_versions(&versions)
            .get(&fragment_id)
            .ok_or_else(|| anyhow::anyhow!("no versions for fragment {}", id_prefix(fragment_id)))?;

        ensure_tag_vocabulary(repo, ws)?;
        let mut change = TribleSet::new();
        let mut tag_index = TagIndex::load(ws)?;
        let tag_ids = if tags.is_empty() {
            prev.tags.clone()
        } else {
            tag_index.resolve_or_mint(&tags, &mut change, ws)?
        };

        let title = new_title.unwrap_or_else(|| prev.title.clone());
        let content_handle = match content {
            Some(text) => ws.put(text),
            None => prev.content_handle,
        };
        let vid = commit_version(
            repo, ws, change, fragment_id, &title, content_handle, &tag_ids, "wiki edit",
        )?;

        println!("fragment {}", id_prefix(fragment_id));
        println!("version  {}", id_prefix(vid));
        Ok(())
    })
}

fn cmd_show(pile: &Path, branch: Option<&str>, id: String) -> Result<()> {
    with_wiki(pile, branch, |_repo, ws| {
        let versions = load_versions(ws)?;
        let tag_index = TagIndex::load(ws)?;
        let version = resolve_version_to_show(&id, &versions)?;
        let fragment_id = version.fragment_id;

        let content: View<str> = ws
            .get(version.content_handle)
            .map_err(|e| anyhow::anyhow!("read content: {e:?}"))?;

        println!("# {}", version.title);
        println!(
            "fragment: {}  version: {}  date: {}",
            id_prefix(fragment_id),
            id_prefix(version.id),
            format_date(version.created_at),
        );
        let tag_str = tag_index.format_tags(&version.tags);
        if !tag_str.is_empty() {
            println!("tags:{tag_str}");
        }
        println!();
        print!("{}", content.as_ref());

        let latest = latest_versions(&versions);
        let (outgoing, incoming) = find_links(ws, fragment_id, &versions, &latest)?;
        if !outgoing.is_empty() || !incoming.is_empty() {
            println!("\n---");
        }
        for target in &outgoing {
            let title = latest.get(target).map(|v| v.title.as_str()).unwrap_or("?");
            println!("→ {} ({})", title, id_prefix(*target));
        }
        for source in &incoming {
            let title = latest.get(source).map(|v| v.title.as_str()).unwrap_or("?");
            println!("← {} ({})", title, id_prefix(*source));
        }

        Ok(())
    })
}

fn cmd_export(pile: &Path, branch: Option<&str>, id: String) -> Result<()> {
    with_wiki(pile, branch, |_repo, ws| {
        let versions = load_versions(ws)?;
        let version = resolve_version_to_show(&id, &versions)?;

        let content: View<str> = ws
            .get(version.content_handle)
            .map_err(|e| anyhow::anyhow!("read content: {e:?}"))?;
        print!("{}", content.as_ref());
        Ok(())
    })
}

fn cmd_diff(
    pile: &Path,
    branch: Option<&str>,
    id: String,
    from: Option<usize>,
    to: Option<usize>,
) -> Result<()> {
    with_wiki(pile, branch, |_repo, ws| {
        let versions = load_versions(ws)?;
        let tag_index = TagIndex::load(ws)?;
        let fragment_id = resolve_to_fragment_id(&id, &versions)?;
        let frag_versions = fragment_history(&versions, fragment_id);
        let n = frag_versions.len();
        if n < 2 {
            bail!(
                "fragment {} has only {n} version(s), need at least 2 to diff",
                id_prefix(fragment_id)
            );
        }

        let from_idx = from.map(|v| v.saturating_sub(1)).unwrap_or(n - 2);
        let to_idx = to.map(|v| v.saturating_sub(1)).unwrap_or(n - 1);
        if from_idx >= n || to_idx >= n {
            bail!("version index out of range (fragment has {n} versions)");
        }

        let old = frag_versions[from_idx];
        let new = frag_versions[to_idx];

        let old_content: View<str> = ws
            .get(old.content_handle)
            .map_err(|e| anyhow::anyhow!("read old content: {e:?}"))?;
        let new_content: View<str> = ws
            .get(new.content_handle)
            .map_err(|e| anyhow::anyhow!("read new content: {e:?}"))?;

        println!(
            "--- v{} {}  {}",
            from_idx + 1,
            id_prefix(old.id),
            old.title,
        );
        println!(
            "+++ v{} {}  {}",
            to_idx + 1,
            id_prefix(new.id),
            new.title,
        );

        let old_tags = tag_index.format_tags(&old.tags);
        let new_tags = tag_index.format_tags(&new.tags);
        if old_tags != new_tags {
            println!("- tags:{old_tags}");
            println!("+ tags:{new_tags}");
        }

        let old_lines: Vec<&str> = old_content.as_ref().lines().collect();
        let new_lines: Vec<&str> = new_content.as_ref().lines().collect();
        let hunks = unified_diff(&old_lines, &new_lines, 3);

        if hunks.is_empty() && old_tags == new_tags && old.title == new.title {
            println!("(no changes)");
        }
        for line in hunks {
            println!("{line}");
        }

        Ok(())
    })
}

fn cmd_archive(pile: &Path, branch: Option<&str>, id: String) -> Result<()> {
    with_wiki(pile, branch, |repo, ws| {
        let versions = load_versions(ws)?;
        let fragment_id = resolve_to_fragment_id(&id, &versions)?;
        let prev = *latest_versions(&versions)
            .get(&fragment_id)
            .ok_or_else(|| anyhow::anyhow!("no versions for fragment {}", id_prefix(fragment_id)))?;

        if prev.tags.contains(&TAG_ARCHIVED_ID) {
            println!("already archived: {} ({})", prev.title, id_prefix(fragment_id));
            return Ok(());
        }

        ensure_tag_vocabulary(repo, ws)?;
        let mut tags = prev.tags.clone();
        tags.push(TAG_ARCHIVED_ID);
        commit_version(
            repo, ws, TribleSet::new(), fragment_id, &prev.title, prev.content_handle, &tags,
            "wiki archive",
        )?;

        println!("archived: {} ({})", prev.title, id_prefix(fragment_id));
        Ok(())
    })
}

fn cmd_restore(pile: &Path, branch: Option<&str>, id: String) -> Result<()> {
    with_wiki(pile, branch, |repo, ws| {
        let versions = load_versions(ws)?;
        let fragment_id = resolve_to_fragment_id(&id, &versions)?;
        let prev = *latest_versions(&versions)
            .get(&fragment_id)
            .ok_or_else(|| anyhow::anyhow!("no versions for fragment {}", id_prefix(fragment_id)))?;

        if !prev.tags.contains(&TAG_ARCHIVED_ID) {
            println!("not archived: {} ({})", prev.title, id_prefix(fragment_id));
            return Ok(());
        }

        let tags: Vec<Id> = prev.tags.iter().copied().filter(|t| *t != TAG_ARCHIVED_ID).collect();
        commit_version(
            repo, ws, TribleSet::new(), fragment_id, &prev.title, prev.content_handle, &tags,
            "wiki restore",
        )?;

        println!("restored: {} ({})", prev.title, id_prefix(fragment_id));
        Ok(())
    })
}

fn cmd_revert(pile: &Path, branch: Option<&str>, id: String, to: usize) -> Result<()> {
    if to == 0 {
        bail!("version number is 1-based");
    }

    with_wiki(pile, branch, |repo, ws| {
        let versions = load_versions(ws)?;
        let fragment_id = resolve_to_fragment_id(&id, &versions)?;
        let history = fragment_history(&versions, fragment_id);

        let idx = to - 1;
        if idx >= history.len() {
            bail!(
                "fragment {} has {} version(s), cannot revert to v{to}",
                id_prefix(fragment_id), history.len(),
            );
        }

        let target = history[idx];
        let vid = commit_version(
            repo, ws, TribleSet::new(), fragment_id, &target.title, target.content_handle,
            &target.tags, "wiki revert",
        )?;

        println!("reverted {} ({}) to v{to}: {}", id_prefix(fragment_id), id_prefix(vid), target.title);
        Ok(())
    })
}

fn cmd_links(pile: &Path, branch: Option<&str>, id: String) -> Result<()> {
    with_wiki(pile, branch, |_repo, ws| {
        let versions = load_versions(ws)?;
        let fragment_id = resolve_to_fragment_id(&id, &versions)?;
        let latest = latest_versions(&versions);
        let frag_title = latest.get(&fragment_id).map(|v| v.title.as_str()).unwrap_or("?");
        let (outgoing, incoming) = find_links(ws, fragment_id, &versions, &latest)?;

        println!("# Links for: {} ({})", frag_title, id_prefix(fragment_id));

        if !outgoing.is_empty() {
            println!("\n→ outgoing:");
            for target in &outgoing {
                let title = latest.get(target).map(|v| v.title.as_str()).unwrap_or("?");
                println!("  → {} ({})", title, id_prefix(*target));
            }
        }
        if !incoming.is_empty() {
            println!("\n← incoming:");
            for source in &incoming {
                let title = latest.get(source).map(|v| v.title.as_str()).unwrap_or("?");
                println!("  ← {} ({})", title, id_prefix(*source));
            }
        }
        if outgoing.is_empty() && incoming.is_empty() {
            println!("\n(no links)");
        }

        Ok(())
    })
}

fn cmd_list(
    pile: &Path,
    branch: Option<&str>,
    filter_tags: Vec<String>,
    show_all: bool,
) -> Result<()> {
    with_wiki(pile, branch, |_repo, ws| {
        let versions = load_versions(ws)?;
        let tag_index = TagIndex::load(ws)?;
        let latest = latest_versions(&versions);

        let filter_ids: Vec<Id> = filter_tags
            .iter()
            .filter_map(|name| {
                let name = name.trim().to_lowercase();
                tag_index.by_name.get(&name).copied()
            })
            .collect();

        let mut entries: Vec<(Id, &Version)> = latest.iter().map(|(&k, &v)| (k, v)).collect();
        entries.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));

        for (frag_id, version) in entries {
            if !show_all && version.tags.contains(&TAG_ARCHIVED_ID) {
                continue;
            }
            if !filter_ids.is_empty() && !filter_ids.iter().all(|ft| version.tags.contains(ft)) {
                continue;
            }

            let tag_str = tag_index.format_tags(&version.tags);
            let n_versions = versions.iter().filter(|v| v.fragment_id == frag_id).count();
            let ver_str = if n_versions > 1 {
                format!(" (v{})", n_versions)
            } else {
                String::new()
            };

            println!(
                "{}  {}  {}{}{}",
                id_prefix(frag_id),
                format_date(version.created_at),
                version.title,
                tag_str,
                ver_str,
            );

            // Content preview: first non-empty line, truncated
            if let Ok(view) = ws.get(version.content_handle) {
                let view: View<str> = view;
                if let Some(line) = view.as_ref().lines().find(|l| !l.trim().is_empty()) {
                    let preview = line.trim();
                    if preview.len() > 80 {
                        println!("    {}...", &preview[..77]);
                    } else {
                        println!("    {preview}");
                    }
                }
            }
        }
        Ok(())
    })
}

fn cmd_history(pile: &Path, branch: Option<&str>, id: String) -> Result<()> {
    with_wiki(pile, branch, |_repo, ws| {
        let versions = load_versions(ws)?;
        let tag_index = TagIndex::load(ws)?;
        let fragment_id = resolve_to_fragment_id(&id, &versions)?;
        let history = fragment_history(&versions, fragment_id);

        let latest_title = history.last().map(|v| v.title.as_str()).unwrap_or("?");
        println!("# History: {} ({})", latest_title, id_prefix(fragment_id));
        println!();

        for (i, v) in history.iter().enumerate() {
            println!(
                "  v{}  {}  {}  {}{}",
                i + 1,
                id_prefix(v.id),
                format_date(v.created_at),
                v.title,
                tag_index.format_tags(&v.tags),
            );
        }
        Ok(())
    })
}

fn cmd_tag_add(pile: &Path, branch: Option<&str>, id: String, name: String) -> Result<()> {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        bail!("tag name cannot be empty");
    }

    with_wiki(pile, branch, |repo, ws| {
        let versions = load_versions(ws)?;
        let fragment_id = resolve_to_fragment_id(&id, &versions)?;
        let prev = *latest_versions(&versions)
            .get(&fragment_id)
            .ok_or_else(|| anyhow::anyhow!("no versions for fragment {}", id_prefix(fragment_id)))?;

        ensure_tag_vocabulary(repo, ws)?;
        let mut change = TribleSet::new();
        let mut tag_index = TagIndex::load(ws)?;
        let new_tag = tag_index.resolve_or_mint(&[name.clone()], &mut change, ws)?[0];

        if prev.tags.contains(&new_tag) {
            println!("already tagged: #{name}");
            return Ok(());
        }

        let mut tags = prev.tags.clone();
        tags.push(new_tag);
        commit_version(
            repo, ws, change, fragment_id, &prev.title, prev.content_handle, &tags,
            "wiki tag add",
        )?;

        println!("added #{name} to {} ({})", prev.title, id_prefix(fragment_id));
        Ok(())
    })
}

fn cmd_tag_remove(pile: &Path, branch: Option<&str>, id: String, name: String) -> Result<()> {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        bail!("tag name cannot be empty");
    }

    with_wiki(pile, branch, |repo, ws| {
        let versions = load_versions(ws)?;
        let fragment_id = resolve_to_fragment_id(&id, &versions)?;
        let prev = *latest_versions(&versions)
            .get(&fragment_id)
            .ok_or_else(|| anyhow::anyhow!("no versions for fragment {}", id_prefix(fragment_id)))?;

        let tag_index = TagIndex::load(ws)?;
        let tag_id = tag_index.by_name.get(&name)
            .ok_or_else(|| anyhow::anyhow!("unknown tag '{name}'"))?;
        if !prev.tags.contains(tag_id) {
            println!("not tagged: #{name}");
            return Ok(());
        }

        let tags: Vec<Id> = prev.tags.iter().copied().filter(|t| t != tag_id).collect();
        commit_version(
            repo, ws, TribleSet::new(), fragment_id, &prev.title, prev.content_handle, &tags,
            "wiki tag remove",
        )?;

        println!("removed #{name} from {} ({})", prev.title, id_prefix(fragment_id));
        Ok(())
    })
}

fn cmd_tag_list(pile: &Path, branch: Option<&str>) -> Result<()> {
    with_wiki(pile, branch, |_repo, ws| {
        let tag_index = TagIndex::load(ws)?;
        let versions = load_versions(ws)?;

        let mut counts: HashMap<Id, usize> = HashMap::new();
        for v in &versions {
            for t in &v.tags {
                *counts.entry(*t).or_default() += 1;
            }
        }

        let mut entries: Vec<(String, Id, usize)> = tag_index
            .by_name
            .iter()
            .map(|(name, &id)| (name.clone(), id, counts.get(&id).copied().unwrap_or(0)))
            .collect();
        entries.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

        for (name, id, count) in entries {
            println!("{}  {}  ({})", id_prefix(id), name, count);
        }
        Ok(())
    })
}

fn cmd_tag_mint(pile: &Path, branch: Option<&str>, name: String) -> Result<()> {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        bail!("tag name cannot be empty");
    }

    with_wiki(pile, branch, |repo, ws| {
        let tag_index = TagIndex::load(ws)?;
        if let Some(&existing) = tag_index.by_name.get(&name) {
            println!("tag '{}' already exists: {}", name, id_prefix(existing));
            return Ok(());
        }

        let tag_id = ufoid();
        let tag_ref = tag_id.id;
        let name_handle = ws.put(name.clone());
        let mut change = TribleSet::new();
        change += entity! { &tag_id @ metadata::name: name_handle };

        ws.commit(change, "wiki mint tag");
        repo.push(ws)
            .map_err(|e| anyhow::anyhow!("push: {e:?}"))?;

        println!("{}  {}", id_prefix(tag_ref), name);
        Ok(())
    })
}

fn cmd_search(
    pile: &Path,
    branch: Option<&str>,
    query: String,
    show_context: bool,
    show_all: bool,
) -> Result<()> {
    let query_lower = query.to_lowercase();

    with_wiki(pile, branch, |_repo, ws| {
        let versions = load_versions(ws)?;
        let tag_index = TagIndex::load(ws)?;
        let latest = latest_versions(&versions);

        let mut hits: Vec<(Id, &Version, Vec<String>)> = Vec::new();

        for (&frag_id, &version) in &latest {
            if !show_all && version.tags.contains(&TAG_ARCHIVED_ID) {
                continue;
            }
            let content: View<str> = ws
                .get(version.content_handle)
                .map_err(|e| anyhow::anyhow!("read content: {e:?}"))?;
            let content_str = content.as_ref();

            let title_match = version.title.to_lowercase().contains(&query_lower);
            let content_lower = content_str.to_lowercase();
            let content_match = content_lower.contains(&query_lower);

            if title_match || content_match {
                let mut context_lines = Vec::new();
                if show_context && content_match {
                    for line in content_str.lines() {
                        if line.to_lowercase().contains(&query_lower) {
                            context_lines.push(line.to_string());
                        }
                    }
                }
                hits.push((frag_id, version, context_lines));
            }
        }

        hits.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));

        if hits.is_empty() {
            println!("no matches for '{query}'");
            return Ok(());
        }

        for (frag_id, version, context_lines) in &hits {
            println!(
                "{}  {}  {}{}",
                id_prefix(*frag_id),
                format_date(version.created_at),
                version.title,
                tag_index.format_tags(&version.tags),
            );
            for line in context_lines {
                println!("    {}", line.trim());
            }
        }

        Ok(())
    })
}

// ── diff engine ────────────────────────────────────────────────────────────

enum DiffOp<'a> {
    Equal(&'a str),
    Add(&'a str),
    Remove(&'a str),
}

/// Produce unified-style diff lines with `context` lines of surrounding context.
fn unified_diff<'a>(old: &[&'a str], new: &[&'a str], context: usize) -> Vec<String> {
    let table = lcs_table(old, new);

    // Walk LCS table backwards to produce diff ops.
    let mut ops: Vec<DiffOp<'a>> = Vec::new();
    let (mut i, mut j) = (old.len(), new.len());
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old[i - 1] == new[j - 1] {
            ops.push(DiffOp::Equal(old[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || table[i][j - 1] >= table[i - 1][j]) {
            ops.push(DiffOp::Add(new[j - 1]));
            j -= 1;
        } else {
            ops.push(DiffOp::Remove(old[i - 1]));
            i -= 1;
        }
    }
    ops.reverse();

    // Mark which ops are near a change and should be shown.
    let change_indices: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !std::matches!(op, DiffOp::Equal(_)))
        .map(|(i, _)| i)
        .collect();

    if change_indices.is_empty() {
        return Vec::new();
    }

    let mut shown = vec![false; ops.len()];
    for &ci in &change_indices {
        let start = ci.saturating_sub(context);
        let end = (ci + context + 1).min(ops.len());
        for idx in start..end {
            shown[idx] = true;
        }
    }

    let mut lines = Vec::new();
    let mut in_hunk = false;
    for (idx, op) in ops.iter().enumerate() {
        if shown[idx] {
            if !in_hunk && idx > 0 {
                lines.push("---".to_string());
            }
            in_hunk = true;
            match op {
                DiffOp::Equal(line) => lines.push(format!(" {line}")),
                DiffOp::Add(line) => lines.push(format!("+{line}")),
                DiffOp::Remove(line) => lines.push(format!("-{line}")),
            }
        } else {
            in_hunk = false;
        }
    }

    lines
}

fn lcs_table(old: &[&str], new: &[&str]) -> Vec<Vec<usize>> {
    let (m, n) = (old.len(), new.len());
    let mut table = vec![vec![0usize; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            table[i][j] = if old[i - 1] == new[j - 1] {
                table[i - 1][j - 1] + 1
            } else {
                table[i - 1][j].max(table[i][j - 1])
            };
        }
    }
    table
}

// ── main ───────────────────────────────────────────────────────────────────
fn main() -> Result<()> {
    let cli = Cli::parse();

    let Some(command) = cli.command else {
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    };

    let branch = cli.branch_id.as_deref();

    match command {
        Command::Create { title, content, tag } => {
            cmd_create(&cli.pile, branch, title, content, tag)
        }
        Command::Edit {
            id,
            content,
            title,
            tag,
        } => cmd_edit(&cli.pile, branch, id, content, title, tag),
        Command::Show { id } => cmd_show(&cli.pile, branch, id),
        Command::Export { id } => cmd_export(&cli.pile, branch, id),
        Command::Diff { id, from, to } => cmd_diff(&cli.pile, branch, id, from, to),
        Command::Archive { id } => cmd_archive(&cli.pile, branch, id),
        Command::Restore { id } => cmd_restore(&cli.pile, branch, id),
        Command::Revert { id, to } => cmd_revert(&cli.pile, branch, id, to),
        Command::Links { id } => cmd_links(&cli.pile, branch, id),
        Command::List { tag, all } => cmd_list(&cli.pile, branch, tag, all),
        Command::History { id } => cmd_history(&cli.pile, branch, id),
        Command::Tag { command: tag_cmd } => match tag_cmd {
            TagCommand::Add { id, name } => cmd_tag_add(&cli.pile, branch, id, name),
            TagCommand::Remove { id, name } => cmd_tag_remove(&cli.pile, branch, id, name),
            TagCommand::List => cmd_tag_list(&cli.pile, branch),
            TagCommand::Mint { name } => cmd_tag_mint(&cli.pile, branch, name),
        },
        Command::Search { query, context, all } => {
            cmd_search(&cli.pile, branch, query, context, all)
        }
    }
}
