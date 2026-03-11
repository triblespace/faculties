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
const KIND_LINK_ID: Id = id_hex!("0224DB50FC250E122663455EE077F922");

// ── initial tag vocabulary ─────────────────────────────────────────────────
const TAG_HYPOTHESIS_ID: Id = id_hex!("1A7FB717FBFCA81CA3AA7D3D186ACC8F");
const TAG_CRITIQUE_ID: Id = id_hex!("72CE6B03E39A8AAC37BC0C4015ED54E2");
const TAG_FINDING_ID: Id = id_hex!("243AE22C5E020F61EBBC8C0481BF05A4");
const TAG_PAPER_ID: Id = id_hex!("8871C1709EBFCDD2588369003D3964DE");
const TAG_SOURCE_ID: Id = id_hex!("7D58EBA4E1E4A1EF868C3C4A58AEC22E");
const TAG_CONCEPT_ID: Id = id_hex!("C86BCF906D270403A0A2083BB95B3552");
const TAG_EXPERIMENT_ID: Id = id_hex!("F8172CC4E495817AB52D2920199EF4BD");

const TAG_SPECS: [(Id, &str); 9] = [
    (KIND_VERSION_ID, "version"),
    (KIND_LINK_ID, "link"),
    (TAG_HYPOTHESIS_ID, "hypothesis"),
    (TAG_CRITIQUE_ID, "critique"),
    (TAG_FINDING_ID, "finding"),
    (TAG_PAPER_ID, "paper"),
    (TAG_SOURCE_ID, "source"),
    (TAG_CONCEPT_ID, "concept"),
    (TAG_EXPERIMENT_ID, "experiment"),
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
        "2BA48B3A84DE38123121846933E82220" as source: valueschemas::GenId;
        "574FBC61140C5CFFB320CDC1FB260DC8" as target: valueschemas::GenId;
    }
}


// ── CLI ────────────────────────────────────────────────────────────────────
#[derive(Parser)]
#[command(name = "wiki", about = "A TribleSpace knowledge wiki faculty")]
struct Cli {
    /// Path to the pile file
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch id (hex). Overrides config.
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
        /// New content. Use @path for file input or @- for stdin.
        content: String,
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
    /// Create a typed link between two fragments
    Link {
        /// Source fragment id (prefix accepted)
        source: String,
        /// Target fragment id (prefix accepted)
        target: String,
        /// Relation tag name (e.g. supports, contradicts, cites, refines)
        relation: String,
    },
    /// Show links from/to a fragment
    Links {
        /// Fragment id (prefix accepted)
        id: String,
    },
    /// List fragments, optionally filtered by tag
    List {
        /// Filter by tag name
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Show version history for a fragment
    History {
        /// Fragment id (prefix accepted)
        id: String,
    },
    /// List all tags in use
    Tags,
    /// Search fragment titles and content (substring, case-insensitive)
    Search {
        /// Search query
        query: String,
        /// Also show matching context lines
        #[arg(long, short = 'c')]
        context: bool,
    },
    /// Mint and register a new tag
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

#[derive(Debug, Clone)]
struct LinkRecord {
    id: Id,
    source: Id,
    target: Id,
    relation_tags: Vec<Id>,
    created_at: i128,
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

fn with_repo<T>(
    pile: &Path,
    f: impl FnOnce(&mut Repository<Pile<valueschemas::Blake3>>) -> Result<T>,
) -> Result<T> {
    let mut repo = open_repo(pile)?;
    let result = f(&mut repo);
    let close_res = repo.close().map_err(|e| anyhow::anyhow!("close pile: {e:?}"));
    if let Err(err) = close_res {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }
    result
}

/// Resolve wiki branch ID: explicit flag or ensure_branch by name.
fn resolve_or_create_branch(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    explicit: Option<&str>,
) -> Result<Id> {
    if let Some(hex) = explicit {
        return Id::from_hex(hex.trim())
            .ok_or_else(|| anyhow::anyhow!("invalid branch id '{hex}'"));
    }

    repo.ensure_branch(WIKI_BRANCH_NAME, None)
        .map_err(|e| anyhow::anyhow!("ensure wiki branch: {e:?}"))
}

/// Ensure all tag/kind IDs have metadata::name entries.
fn ensure_tag_names(ws: &mut Workspace<Pile<valueschemas::Blake3>>) -> Result<TribleSet> {
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
    Ok(change)
}

/// Build a map from tag name → tag Id, including all named entities.
fn load_tag_names(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
) -> Result<HashMap<String, Id>> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout for tag names: {e:?}"))?;
    let mut map = HashMap::new();
    for (tag_id, handle) in find!(
        (tag_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?tag_id @ metadata::name: ?handle }])
    ) {
        let view: View<str> = ws
            .get(handle)
            .map_err(|e| anyhow::anyhow!("read tag name: {e:?}"))?;
        map.insert(view.as_ref().to_string(), tag_id);
    }
    Ok(map)
}

/// Resolve tag names to IDs, minting new ones for unknown names.
fn resolve_tags(
    tag_names: &mut HashMap<String, Id>,
    names: &[String],
    change: &mut TribleSet,
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
) -> Result<Vec<Id>> {
    let mut ids = Vec::new();
    for name in names {
        let name = name.trim().to_lowercase();
        if name.is_empty() {
            continue;
        }
        if let Some(&id) = tag_names.get(&name) {
            ids.push(id);
        } else {
            // Mint a new tag
            let tag_id = ufoid();
            let tag_ref = tag_id.id;
            let name_handle = ws.put(name.clone());
            *change += entity! { &tag_id @ metadata::name: name_handle };
            tag_names.insert(name, tag_ref);
            ids.push(tag_ref);
        }
    }
    Ok(ids)
}

/// Load all versions from the wiki branch.
fn load_versions(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
) -> Result<Vec<Version>> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;

    let mut versions: HashMap<Id, Version> = HashMap::new();

    // Core fields
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
        versions.insert(vid, Version {
            id: vid,
            fragment_id: frag,
            title,
            content_handle: content_h,
            created_at: interval_key(ts),
            tags: Vec::new(),
        });
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

/// Load all links from the wiki branch.
fn load_links(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
) -> Result<Vec<LinkRecord>> {
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow::anyhow!("checkout: {e:?}"))?;

    let mut links: HashMap<Id, LinkRecord> = HashMap::new();

    for (lid, src, tgt, ts) in find!(
        (lid: Id, src: Id, tgt: Id, ts: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{
            ?lid @
            metadata::tag: &KIND_LINK_ID,
            wiki::source: ?src,
            wiki::target: ?tgt,
            wiki::created_at: ?ts,
        }])
    ) {
        links.insert(lid, LinkRecord {
            id: lid,
            source: src,
            target: tgt,
            relation_tags: Vec::new(),
            created_at: interval_key(ts),
        });
    }

    // Collect relation tags
    for (lid, tag_id) in find!(
        (lid: Id, tag_id: Id),
        pattern!(&space, [{
            ?lid @
            metadata::tag: &KIND_LINK_ID,
            metadata::tag: ?tag_id,
        }])
    ) {
        if let Some(link) = links.get_mut(&lid) {
            if tag_id != KIND_LINK_ID {
                link.relation_tags.push(tag_id);
            }
        }
    }

    Ok(links.into_values().collect())
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

/// Resolve an id that could be either a fragment or version id.
fn resolve_fragment_or_version(input: &str, versions: &[Version]) -> Result<Id> {
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
    // Prefer fragment match
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

fn tag_name(id: Id, names: &HashMap<String, Id>) -> String {
    for (name, &tag_id) in names {
        if tag_id == id {
            return name.clone();
        }
    }
    id_prefix(id)
}

// ── commands ───────────────────────────────────────────────────────────────

fn cmd_create(
    pile: &Path,
    branch_id: Id,
    title: String,
    content: String,
    tags: Vec<String>,
) -> Result<()> {
    let title = load_value_or_file(&title, "title")?;
    let content = load_value_or_file(&content, "content")?;

    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        // Commit well-known tag names first so they're visible to load_tag_names.
        let tag_change = ensure_tag_names(&mut ws)?;
        if !tag_change.is_empty() {
            ws.commit(tag_change, "wiki: register tag names");
            repo.push(&mut ws)
                .map_err(|e| anyhow::anyhow!("push tag names: {e:?}"))?;
        }

        let mut change = TribleSet::new();
        let mut tag_names = load_tag_names(&mut ws)?;

        let fragment_id = ufoid();
        let fragment_ref = fragment_id.id;
        let version_id = ufoid();

        // Resolve tag names to IDs (+ KIND_VERSION)
        let mut tag_ids = resolve_tags(&mut tag_names, &tags, &mut change, &mut ws)?;
        tag_ids.push(KIND_VERSION_ID);
        tag_ids.sort();
        tag_ids.dedup();

        let title_handle = ws.put(title);
        let content_handle = ws.put(content);
        let now = now_tai();

        change += entity! { &version_id @
            wiki::fragment: &fragment_ref,
            wiki::title: title_handle,
            wiki::content: content_handle,
            wiki::created_at: now,
            metadata::tag*: tag_ids.iter(),
        };

        ws.commit(change, "wiki create");
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push: {e:?}"))?;

        println!("fragment {}", id_prefix(fragment_ref));
        println!("version  {}", id_prefix(version_id.id));
        Ok(())
    })
}

fn cmd_edit(
    pile: &Path,
    branch_id: Id,
    id: String,
    content: String,
    new_title: Option<String>,
    tags: Vec<String>,
) -> Result<()> {
    let content = load_value_or_file(&content, "content")?;
    let new_title = new_title
        .map(|t| load_value_or_file(&t, "title"))
        .transpose()?;

    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        let versions = load_versions(&mut ws)?;
        let resolved = resolve_fragment_or_version(&id, &versions)?;

        // Find the fragment id — resolved could be a fragment or version id
        let fragment_id = if versions.iter().any(|v| v.fragment_id == resolved) {
            resolved
        } else if let Some(v) = versions.iter().find(|v| v.id == resolved) {
            v.fragment_id
        } else {
            bail!("no fragment or version found for '{id}'");
        };

        let latest = latest_versions(&versions);
        let prev = latest
            .get(&fragment_id)
            .ok_or_else(|| anyhow::anyhow!("no versions for fragment {}", id_prefix(fragment_id)))?;

        let tag_change = ensure_tag_names(&mut ws)?;
        if !tag_change.is_empty() {
            ws.commit(tag_change, "wiki: register tag names");
            repo.push(&mut ws)
                .map_err(|e| anyhow::anyhow!("push tag names: {e:?}"))?;
        }

        let mut change = TribleSet::new();
        let mut tag_names = load_tag_names(&mut ws)?;

        // Resolve tags — if none given, inherit from previous version
        let mut tag_ids = if tags.is_empty() {
            prev.tags.clone()
        } else {
            resolve_tags(&mut tag_names, &tags, &mut change, &mut ws)?
        };
        tag_ids.push(KIND_VERSION_ID);
        tag_ids.sort();
        tag_ids.dedup();

        let title = new_title.unwrap_or_else(|| prev.title.clone());
        let title_handle = ws.put(title);
        let content_handle = ws.put(content);
        let now = now_tai();
        let version_id = ufoid();

        change += entity! { &version_id @
            wiki::fragment: &fragment_id,
            wiki::title: title_handle,
            wiki::content: content_handle,
            wiki::created_at: now,
            metadata::tag*: tag_ids.iter(),
        };

        ws.commit(change, "wiki edit");
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push: {e:?}"))?;

        println!("fragment {}", id_prefix(fragment_id));
        println!("version  {}", id_prefix(version_id.id));
        Ok(())
    })
}

fn cmd_show(pile: &Path, branch_id: Id, id: String) -> Result<()> {
    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        let versions = load_versions(&mut ws)?;
        let tag_names = load_tag_names(&mut ws)?;
        let resolved = resolve_fragment_or_version(&id, &versions)?;

        // Determine which version to show
        let version = if let Some(v) = versions.iter().find(|v| v.id == resolved) {
            // Direct version id match — show that specific version
            v
        } else {
            // Fragment id — show latest
            let latest = latest_versions(&versions);
            *latest
                .get(&resolved)
                .ok_or_else(|| anyhow::anyhow!("no versions for '{id}'"))?
        };

        let content: View<str> = ws
            .get(version.content_handle)
            .map_err(|e| anyhow::anyhow!("read content: {e:?}"))?;

        let tags: Vec<String> = version
            .tags
            .iter()
            .map(|t| tag_name(*t, &tag_names))
            .collect();

        println!("# {}", version.title);
        println!("fragment: {}  version: {}", id_prefix(version.fragment_id), id_prefix(version.id));
        if !tags.is_empty() {
            println!("tags: {}", tags.join(", "));
        }
        println!();
        print!("{}", content.as_ref());

        Ok(())
    })
}

fn cmd_link(
    pile: &Path,
    branch_id: Id,
    source: String,
    target: String,
    relation: String,
) -> Result<()> {
    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        let versions = load_versions(&mut ws)?;
        let source_id = resolve_fragment_or_version(&source, &versions)?;
        let target_id = resolve_fragment_or_version(&target, &versions)?;

        // Ensure both are fragment ids
        let source_frag = if versions.iter().any(|v| v.fragment_id == source_id) {
            source_id
        } else if let Some(v) = versions.iter().find(|v| v.id == source_id) {
            v.fragment_id
        } else {
            bail!("no fragment for source '{source}'");
        };

        let target_frag = if versions.iter().any(|v| v.fragment_id == target_id) {
            target_id
        } else if let Some(v) = versions.iter().find(|v| v.id == target_id) {
            v.fragment_id
        } else {
            bail!("no fragment for target '{target}'");
        };

        let tag_change = ensure_tag_names(&mut ws)?;
        if !tag_change.is_empty() {
            ws.commit(tag_change, "wiki: register tag names");
            repo.push(&mut ws)
                .map_err(|e| anyhow::anyhow!("push tag names: {e:?}"))?;
        }

        let mut change = TribleSet::new();
        let mut tag_names = load_tag_names(&mut ws)?;

        let relation_tags = resolve_tags(&mut tag_names, &[relation], &mut change, &mut ws)?;
        let mut all_tags = relation_tags;
        all_tags.push(KIND_LINK_ID);
        all_tags.sort();
        all_tags.dedup();

        let link_id = ufoid();
        let now = now_tai();

        change += entity! { &link_id @
            wiki::source: &source_frag,
            wiki::target: &target_frag,
            wiki::created_at: now,
            metadata::tag*: all_tags.iter(),
        };

        ws.commit(change, "wiki link");
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push: {e:?}"))?;

        let source_title = latest_versions(&versions)
            .get(&source_frag)
            .map(|v| v.title.as_str())
            .unwrap_or("?");
        let target_title = latest_versions(&versions)
            .get(&target_frag)
            .map(|v| v.title.as_str())
            .unwrap_or("?");
        println!(
            "{} ({}) → {} ({})",
            source_title,
            id_prefix(source_frag),
            target_title,
            id_prefix(target_frag),
        );
        Ok(())
    })
}

fn cmd_links(pile: &Path, branch_id: Id, id: String) -> Result<()> {
    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        let versions = load_versions(&mut ws)?;
        let tag_names = load_tag_names(&mut ws)?;
        let links = load_links(&mut ws)?;
        let resolved = resolve_fragment_or_version(&id, &versions)?;

        let fragment_id = if versions.iter().any(|v| v.fragment_id == resolved) {
            resolved
        } else if let Some(v) = versions.iter().find(|v| v.id == resolved) {
            v.fragment_id
        } else {
            bail!("no fragment for '{id}'");
        };

        let latest = latest_versions(&versions);
        let frag_title = latest
            .get(&fragment_id)
            .map(|v| v.title.as_str())
            .unwrap_or("?");

        let outgoing: Vec<&LinkRecord> = links.iter().filter(|l| l.source == fragment_id).collect();
        let incoming: Vec<&LinkRecord> = links.iter().filter(|l| l.target == fragment_id).collect();

        println!("# Links for: {} ({})", frag_title, id_prefix(fragment_id));

        if !outgoing.is_empty() {
            println!("\n→ outgoing:");
            for link in &outgoing {
                let rel: Vec<String> = link.relation_tags.iter().map(|t| tag_name(*t, &tag_names)).collect();
                let target_title = latest
                    .get(&link.target)
                    .map(|v| v.title.as_str())
                    .unwrap_or("?");
                println!(
                    "  [{}] → {} ({})",
                    rel.join(", "),
                    target_title,
                    id_prefix(link.target),
                );
            }
        }

        if !incoming.is_empty() {
            println!("\n← incoming:");
            for link in &incoming {
                let rel: Vec<String> = link.relation_tags.iter().map(|t| tag_name(*t, &tag_names)).collect();
                let source_title = latest
                    .get(&link.source)
                    .map(|v| v.title.as_str())
                    .unwrap_or("?");
                println!(
                    "  {} ({}) [{}] →",
                    source_title,
                    id_prefix(link.source),
                    rel.join(", "),
                );
            }
        }

        if outgoing.is_empty() && incoming.is_empty() {
            println!("\n(no links)");
        }

        Ok(())
    })
}

fn cmd_list(pile: &Path, branch_id: Id, filter_tags: Vec<String>) -> Result<()> {
    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        let versions = load_versions(&mut ws)?;
        let tag_names = load_tag_names(&mut ws)?;
        let latest = latest_versions(&versions);

        // Resolve filter tags to IDs
        let filter_ids: Vec<Id> = filter_tags
            .iter()
            .filter_map(|name| {
                let name = name.trim().to_lowercase();
                tag_names.get(&name).copied()
            })
            .collect();

        let mut entries: Vec<(Id, &Version)> = latest.iter().map(|(&k, &v)| (k, v)).collect();
        entries.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));

        for (frag_id, version) in entries {
            // Apply tag filter
            if !filter_ids.is_empty() && !filter_ids.iter().all(|ft| version.tags.contains(ft)) {
                continue;
            }

            let tags: Vec<String> = version
                .tags
                .iter()
                .map(|t| tag_name(*t, &tag_names))
                .collect();

            let tag_str = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(", "))
            };

            let n_versions = versions.iter().filter(|v| v.fragment_id == frag_id).count();
            let ver_str = if n_versions > 1 {
                format!(" (v{})", n_versions)
            } else {
                String::new()
            };

            println!(
                "{}  {}{}{}",
                id_prefix(frag_id),
                version.title,
                tag_str,
                ver_str,
            );
        }
        Ok(())
    })
}

fn cmd_history(pile: &Path, branch_id: Id, id: String) -> Result<()> {
    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        let versions = load_versions(&mut ws)?;
        let tag_names = load_tag_names(&mut ws)?;
        let resolved = resolve_fragment_or_version(&id, &versions)?;

        let fragment_id = if versions.iter().any(|v| v.fragment_id == resolved) {
            resolved
        } else if let Some(v) = versions.iter().find(|v| v.id == resolved) {
            v.fragment_id
        } else {
            bail!("no fragment for '{id}'");
        };

        let mut frag_versions: Vec<&Version> = versions
            .iter()
            .filter(|v| v.fragment_id == fragment_id)
            .collect();
        frag_versions.sort_by_key(|v| v.created_at);

        let latest_title = frag_versions
            .last()
            .map(|v| v.title.as_str())
            .unwrap_or("?");
        println!("# History: {} ({})", latest_title, id_prefix(fragment_id));
        println!();

        for (i, v) in frag_versions.iter().enumerate() {
            let tags: Vec<String> = v.tags.iter().map(|t| tag_name(*t, &tag_names)).collect();
            let tag_str = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(", "))
            };
            println!(
                "  v{}  {}  {}{}",
                i + 1,
                id_prefix(v.id),
                v.title,
                tag_str,
            );
        }
        Ok(())
    })
}

fn cmd_tags(pile: &Path, branch_id: Id) -> Result<()> {
    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        let tag_names = load_tag_names(&mut ws)?;
        let versions = load_versions(&mut ws)?;
        let links = load_links(&mut ws)?;

        // Count usage
        let mut counts: HashMap<Id, usize> = HashMap::new();
        for v in &versions {
            for t in &v.tags {
                *counts.entry(*t).or_default() += 1;
            }
        }
        for l in &links {
            for t in &l.relation_tags {
                *counts.entry(*t).or_default() += 1;
            }
        }

        let mut entries: Vec<(String, Id, usize)> = tag_names
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

fn cmd_mint(pile: &Path, branch_id: Id, name: String) -> Result<()> {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        bail!("tag name cannot be empty");
    }

    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        let tag_names = load_tag_names(&mut ws)?;
        if let Some(&existing) = tag_names.get(&name) {
            println!("tag '{}' already exists: {}", name, id_prefix(existing));
            return Ok(());
        }

        let tag_id = ufoid();
        let tag_ref = tag_id.id;
        let name_handle = ws.put(name.clone());
        let mut change = TribleSet::new();
        change += entity! { &tag_id @ metadata::name: name_handle };

        ws.commit(change, "wiki mint tag");
        repo.push(&mut ws)
            .map_err(|e| anyhow::anyhow!("push: {e:?}"))?;

        println!("{}  {}", id_prefix(tag_ref), name);
        Ok(())
    })
}

fn cmd_search(pile: &Path, branch_id: Id, query: String, show_context: bool) -> Result<()> {
    let query_lower = query.to_lowercase();

    with_repo(pile, |repo| {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow::anyhow!("pull workspace: {e:?}"))?;

        let versions = load_versions(&mut ws)?;
        let tag_names = load_tag_names(&mut ws)?;
        let latest = latest_versions(&versions);

        let mut hits: Vec<(Id, &Version, Vec<String>)> = Vec::new();

        for (&frag_id, &version) in &latest {
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
            let tags: Vec<String> = version
                .tags
                .iter()
                .map(|t| tag_name(*t, &tag_names))
                .collect();
            let tag_str = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(", "))
            };
            println!("{}  {}{}", id_prefix(*frag_id), version.title, tag_str);
            for line in context_lines {
                println!("    {}", line.trim());
            }
        }

        Ok(())
    })
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

    let branch_id = with_repo(&cli.pile, |repo| {
        resolve_or_create_branch(repo, cli.branch_id.as_deref())
    })?;

    match command {
        Command::Create { title, content, tag } => {
            cmd_create(&cli.pile, branch_id, title, content, tag)
        }
        Command::Edit { id, content, title, tag } => {
            cmd_edit(&cli.pile, branch_id, id, content, title, tag)
        }
        Command::Show { id } => cmd_show(&cli.pile, branch_id, id),
        Command::Link { source, target, relation } => {
            cmd_link(&cli.pile, branch_id, source, target, relation)
        }
        Command::Links { id } => cmd_links(&cli.pile, branch_id, id),
        Command::List { tag } => cmd_list(&cli.pile, branch_id, tag),
        Command::History { id } => cmd_history(&cli.pile, branch_id, id),
        Command::Tags => cmd_tags(&cli.pile, branch_id),
        Command::Search { query, context } => cmd_search(&cli.pile, branch_id, query, context),
        Command::Mint { name } => cmd_mint(&cli.pile, branch_id, name),
    }
}
