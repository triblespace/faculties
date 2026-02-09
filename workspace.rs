#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4"
//! rand_core = "0.6.4"
//! triblespace = "0.12.0"
//! ```

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Args, CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use triblespace::core::blob::{Blob, Bytes};
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::macros::id_hex;
use triblespace::prelude::blobschemas::{FileBytes, LongString};
use triblespace::prelude::valueschemas::{Blake3, GenId, Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

const DEFAULT_WORKSPACE_BRANCH: &str = "workspace";
const ATLAS_BRANCH: &str = "atlas";

mod playground_workspace {
    use super::*;

    attributes! {
        "E39FB34126FE01A32F1D4B3DAD0F1874" as pub kind: GenId;
        "A95E92FB35943C570BE45FF811B0BD07" as pub created_at: NsTAIInterval;
        "B667B02CEB4493232632473ECB782287" as pub root_path: Handle<Blake3, LongString>;
        "435869D280EC3123D391A32025C6F3CC" as pub label: Handle<Blake3, LongString>;
        "C69E168C68E317858A62BA51FC326E97" as pub entry: GenId;
        "1032F072E6730AB40A6F5F568C4C23EB" as pub path: Handle<Blake3, LongString>;
        "C91379DEDA545341C8C7A7B4DA65C8FE" as pub mode: U256BE;
        "5FBC9E963E2BA9E2CC9E7B7C12587FBB" as pub bytes: Handle<Blake3, FileBytes>;
        "6AD64B466D4AB7B7E14D8C28DFFC592F" as pub link_target: Handle<Blake3, LongString>;
    }

    #[allow(non_upper_case_globals)]
    pub const kind_snapshot: Id = id_hex!("1620AABF5A93D4897DFFE728308D358E");
    #[allow(non_upper_case_globals)]
    pub const kind_file: Id = id_hex!("4B8C79B3B6E84C2187C078C533737718");
    #[allow(non_upper_case_globals)]
    pub const kind_dir: Id = id_hex!("7010C177AE931A2E3116AE742914D23F");
    #[allow(non_upper_case_globals)]
    pub const kind_symlink: Id = id_hex!("486FCFF53CAD57EAD3DCFB7D903B245B");
}

#[derive(Parser)]
#[command(name = "workspace", about = "Workspace snapshot faculty")]
struct Cli {
    /// Path to the pile file to use
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch name for workspace snapshots
    #[arg(long, default_value = DEFAULT_WORKSPACE_BRANCH, global = true)]
    branch: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Capture a workspace snapshot into the pile.
    Capture(WorkspaceCaptureArgs),
    /// List available workspace snapshots.
    List,
    /// Restore a workspace snapshot to a target directory.
    Restore(WorkspaceRestoreArgs),
}

#[derive(Args)]
struct WorkspaceCaptureArgs {
    /// Capture mappings as <local_path> <vm_path> pairs.
    #[arg(value_name = "PATH", num_args = 2..)]
    paths: Vec<PathBuf>,
    /// Optional label for the snapshot
    #[arg(long)]
    label: Option<String>,
}

#[derive(Args)]
struct WorkspaceRestoreArgs {
    /// Snapshot id (hex). Defaults to the latest snapshot.
    #[arg(long)]
    snapshot: Option<String>,
    /// Target directory to restore into
    target: Option<PathBuf>,
    /// Overwrite existing files
    #[arg(long, default_value_t = false)]
    force: bool,
}

#[derive(Debug, Clone)]
struct SnapshotInfo {
    id: Id,
    created_at: Epoch,
    created_key: i128,
    label: Option<String>,
    root_path: Option<String>,
    entry_count: usize,
}

#[derive(Debug, Clone, Copy)]
enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone)]
struct CapturedEntry {
    path: String,
    kind: EntryKind,
    mode: Option<u32>,
    bytes: Option<Vec<u8>>,
    link_target: Option<String>,
}

const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".vscode",
    "node_modules",
    "target",
    "dist",
    "build",
    "tmp",
    "runtime",
    "state",
];

const EXCLUDED_FILES: &[&str] = &[".DS_Store", "Thumbs.db"];

fn main() -> Result<()> {
    let Cli {
        pile,
        branch,
        command,
    } = Cli::parse();
    if let Err(err) = emit_schema_to_atlas(&pile) {
        eprintln!("atlas emit: {err}");
    }
    let Some(cmd) = command else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };

    match cmd {
        Command::Capture(args) => cmd_capture(&pile, &branch, args),
        Command::List => cmd_list(&pile, &branch),
        Command::Restore(args) => cmd_restore(&pile, &branch, args),
    }
}

fn emit_schema_to_atlas(pile_path: &Path) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile_path, ATLAS_BRANCH)?;
    let metadata = build_workspace_metadata(repo.storage_mut())
        .map_err(|e| anyhow!("build workspace metadata: {e:?}"))?;

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
        .map_err(|err| anyhow!("close pile: {err:?}"))?;
    Ok(())
}

fn build_workspace_metadata<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<Blake3>,
{
    let mut metadata = TribleSet::new();

    metadata.union(<GenId as metadata::ConstMetadata>::describe(blobs)?);
    metadata.union(<NsTAIInterval as metadata::ConstMetadata>::describe(blobs)?);
    metadata.union(<U256BE as metadata::ConstMetadata>::describe(blobs)?);
    metadata.union(<Handle<Blake3, LongString> as metadata::ConstMetadata>::describe(blobs)?);
    metadata.union(<FileBytes as metadata::ConstMetadata>::describe(blobs)?);
    metadata.union(<LongString as metadata::ConstMetadata>::describe(blobs)?);

    metadata.union(describe_attribute(blobs, &playground_workspace::kind, "workspace_kind")?);
    metadata.union(describe_attribute(
        blobs,
        &playground_workspace::created_at,
        "workspace_created_at",
    )?);
    metadata.union(describe_attribute(
        blobs,
        &playground_workspace::root_path,
        "workspace_root_path",
    )?);
    metadata.union(describe_attribute(
        blobs,
        &playground_workspace::label,
        "workspace_label",
    )?);
    metadata.union(describe_attribute(
        blobs,
        &playground_workspace::entry,
        "workspace_entry",
    )?);
    metadata.union(describe_attribute(
        blobs,
        &playground_workspace::path,
        "workspace_path",
    )?);
    metadata.union(describe_attribute(
        blobs,
        &playground_workspace::mode,
        "workspace_mode",
    )?);
    metadata.union(describe_attribute(
        blobs,
        &playground_workspace::bytes,
        "workspace_bytes",
    )?);
    metadata.union(describe_attribute(
        blobs,
        &playground_workspace::link_target,
        "workspace_link_target",
    )?);

    metadata.union(describe_kind(
        blobs,
        &playground_workspace::kind_snapshot,
        "workspace_snapshot",
        "Workspace snapshot kind.",
    )?);
    metadata.union(describe_kind(
        blobs,
        &playground_workspace::kind_file,
        "workspace_file",
        "Workspace file entry kind.",
    )?);
    metadata.union(describe_kind(
        blobs,
        &playground_workspace::kind_dir,
        "workspace_dir",
        "Workspace directory entry kind.",
    )?);
    metadata.union(describe_kind(
        blobs,
        &playground_workspace::kind_symlink,
        "workspace_symlink",
        "Workspace symlink entry kind.",
    )?);

    Ok(metadata)
}

fn describe_attribute<B, S>(
    blobs: &mut B,
    attribute: &Attribute<S>,
    name: &str,
) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<Blake3>,
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
    id: &Id,
    name: &str,
    description: &str,
) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<Blake3>,
{

    let (blobs.put(description.to_owned())?) = blobs.put(description     metadata::name: (blobs.put(name.to_owned())?),
        metadata::description: description_handle,
    })
}

fn cmd_capture(pile: &Path, branch: &str, args: WorkspaceCaptureArgs) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let mappings = build_mappings(&args.paths)?;
    let snapshot_id = capture_snapshot(&mut repo, branch_id, &mappings, args.label.as_deref())?;
    println!("snapshot: {snapshot_id:x}");
    repo.close()
        .map_err(|err| anyhow!("close pile: {err:?}"))?;
    Ok(())
}

fn cmd_list(pile: &Path, branch: &str) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let snapshots = list_snapshots(&mut repo, branch_id)?;
    print_snapshots(&snapshots);
    repo.close()
        .map_err(|err| anyhow!("close pile: {err:?}"))?;
    Ok(())
}

fn cmd_restore(pile: &Path, branch: &str, args: WorkspaceRestoreArgs) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile, branch)?;
    let snapshot_id = parse_optional_hex_id(args.snapshot.as_deref())?;
    let target = args.target.unwrap_or_else(|| PathBuf::from("."));
    let restored = restore_snapshot(&mut repo, branch_id, snapshot_id, &target, args.force)?;
    match restored {
        Some(id) => println!("restored: {id:x}"),
        None => println!("no snapshots found"),
    }
    repo.close()
        .map_err(|err| anyhow!("close pile: {err:?}"))?;
    Ok(())
}

fn open_repo(path: &Path, branch_name: &str) -> Result<(Repository<Pile<Blake3>>, Id)> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }

    let mut pile = Pile::<Blake3>::open(path)
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

fn find_branch_by_name(pile: &mut Pile<Blake3>, branch_name: &str) -> Result<Option<Id>> {
    let reader = pile.reader().context("pile reader")?;
    let iter = pile.branches().context("list branches")?;
    let expected = LongString::from(branch_name)
        .to_blob()
        .get_handle::<Blake3>()
        .to_value();

    for branch in iter {
        let branch_id = branch.context("branch id")?;
        let Some(head) = pile.head(branch_id).context("branch head")? else {
            continue;
        };
        let metadata_set: TribleSet = reader.get(head).context("branch metadata")?;
        let mut names = find!(
            (handle: Value<Handle<Blake3, LongString>>),
            pattern!(&metadata_set, [{ metadata::name: ?handle }])
        )
        .into_iter();
        let Some((handle,)) = names.next() else {
            continue;
        };
        if names.next().is_some() {
            continue;
        }
        if handle == expected {
            return Ok(Some(branch_id));
        }
    }

    Ok(None)
}

fn capture_snapshot(
    repo: &mut Repository<Pile<Blake3>>,
    branch_id: Id,
    mappings: &[PathMapping],
    label: Option<&str>,
) -> Result<Id> {
    if mappings.is_empty() {
        return Err(anyhow!("no capture mappings provided"));
    }
    let mut ws = repo
        .pull(branch_id)
        .map_err(|err| anyhow!("pull workspace branch: {err:?}"))?;

    let (root_path, entries) = collect_mapped_entries(mappings).context("collect workspace entries")?;

    let snapshot_id = ufoid();
    let created_at = epoch_interval(now_epoch());
    let mut change = TribleSet::new();
    change += entity! { &snapshot_id @
        playground_workspace::kind: playground_workspace::kind_snapshot,
        playground_workspace::created_at: created_at,
    };
t_path);
    change += entity! { &snapshot_id @ playground_workspace::root_path: root_handle };

    if let Some(label) = label {
        let (ws.put(label.to_stri entity! { &snapshot_id @ playground_workspace::label: label_handle };
    }

    for entry in entries {
        let entry_id = ufoid();
        let entry_ref = *entry_id;
        change += entity! { &snapshot_id @ playground_workspace::entry: entry_ref };

        let (ws.put(entry.path)) = ws.put(entry.path);
        change :path: path_handle };

        let kind_id = match entry.kind {
            EntryKind::File => playground_workspace::kind_file,
            EntryKind::Dir => playground_workspace::kind_dir,
            EntryKind::Symlink => playground_workspace::kind_symlink,
        };
        change += entity! { &entry_id @ playground_workspace::kind: kind_id };

        if let Some(mode) = entry.mode {
            let mode_val: Value<U256BE> = (mode as u64).to_value();
            change += entity! { &entry_id @ playground_workspace::mode: mode_val };
        }

        if let Some(bytes) = entry.bytes {
            let handle = ws.put(Bytes::from_source(bytes));
            change += entity! { &entry_id @ playground_workspace::bytes: handle };
        }

        if let Some(target) = entry.link_target {
            let handle = ws.put(target);
            change += entity! { &entry_id @ playground_workspace::link_target: handle };
        }
    }

    ws.commit(change, None, Some("playground_workspace snapshot"));
    repo.push(&mut ws)
        .map_err(|err| anyhow!("push snapshot: {err:?}"))?;
    Ok(*snapshot_id)
}

#[derive(Clone, Debug)]
struct PathMapping {
    local: PathBuf,
    vm: PathBuf,
}

fn build_mappings(paths: &[PathBuf]) -> Result<Vec<PathMapping>> {
    if paths.is_empty() {
        return Err(anyhow!(
            "capture requires <local_path> <vm_path> pairs"
        ));
    }
    if paths.len() % 2 != 0 {
        return Err(anyhow!(
            "capture requires an even number of paths (local, vm pairs)"
        ));
    }
    let mut mappings = Vec::new();
    let mut iter = paths.iter();
    while let (Some(local), Some(vm)) = (iter.next(), iter.next()) {
        mappings.push(PathMapping {
            local: local.clone(),
            vm: vm.clone(),
        });
    }
    Ok(mappings)
}

fn collect_mapped_entries(
    mappings: &[PathMapping],
) -> Result<(String, Vec<CapturedEntry>)> {
    let mut entries = Vec::new();
    let mut per_map_entries = Vec::new();
    let mut vm_roots = Vec::new();
    let mut any_absolute = false;
    let mut any_relative = false;

    for mapping in mappings {
        let local = &mapping.local;
        if !local.exists() {
            return Err(anyhow!("local path does not exist: {}", local.display()));
        }
        let metadata = fs::symlink_metadata(local)
            .with_context(|| format!("read metadata {}", local.display()))?;
        let file_type = metadata.file_type();
        let (vm_target, vm_root) = if file_type.is_dir() {
            (mapping.vm.clone(), mapping.vm.clone())
        } else if file_type.is_file() || file_type.is_symlink() {
            (
                mapping.vm.clone(),
                mapping
                    .vm
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(PathBuf::new),
            )
        } else {
            return Err(anyhow!("unsupported local path: {}", local.display()));
        };

        if vm_target.is_absolute() {
            any_absolute = true;
        } else {
            any_relative = true;
        }
        vm_roots.push(vm_root);

        if file_type.is_dir() {
            let mut local_entries = Vec::new();
            collect_entries(local, local, &mut local_entries)
                .with_context(|| format!("collect entries {}", local.display()))?;
            per_map_entries.push((vm_target, local_entries));
        } else {
            let entry = capture_single_entry(local, String::new())
                .with_context(|| format!("capture {}", local.display()))?;
            per_map_entries.push((vm_target, vec![entry]));
        }
    }

    if any_absolute && any_relative {
        return Err(anyhow!("vm paths must be all absolute or all relative"));
    }

    let common_root = common_path_prefix(&vm_roots);
    let root_path = if common_root.as_os_str().is_empty() {
        ".".to_string()
    } else {
        common_root.to_string_lossy().to_string()
    };

    let mut seen = std::collections::HashSet::new();
    for (vm_target, local_entries) in per_map_entries {
        let vm_rel = if common_root.as_os_str().is_empty() {
            vm_target
        } else {
            vm_target
                .strip_prefix(&common_root)
                .map(PathBuf::from)
                .map_err(|_| {
                    anyhow!(
                        "vm path {} is not under common root {}",
                        vm_target.display(),
                        common_root.display()
                    )
                })?
        };
        for mut entry in local_entries {
            let rel_path = Path::new(&entry.path);
            let mut dest = vm_rel.clone();
            if !rel_path.as_os_str().is_empty() {
                dest.push(rel_path);
            }
            let dest_str = dest.to_string_lossy().to_string();
            if !seen.insert(dest_str.clone()) {
                return Err(anyhow!("duplicate vm path {}", dest_str));
            }
            entry.path = dest_str;
            entries.push(entry);
        }
    }

    Ok((root_path, entries))
}

fn common_path_prefix(paths: &[PathBuf]) -> PathBuf {
    if paths.is_empty() {
        return PathBuf::new();
    }
    let mut prefix: Vec<Component<'_>> = paths[0].components().collect();
    for path in &paths[1..] {
        let comps: Vec<Component<'_>> = path.components().collect();
        let mut next = Vec::new();
        for (a, b) in prefix.iter().zip(comps.iter()) {
            if a == b {
                next.push(*a);
            } else {
                break;
            }
        }
        prefix = next;
        if prefix.is_empty() {
            break;
        }
    }
    let mut out = PathBuf::new();
    for comp in prefix {
        out.push(comp.as_os_str());
    }
    out
}

fn capture_single_entry(path: &Path, rel_path: String) -> Result<CapturedEntry> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("read metadata {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Ok(CapturedEntry {
            path: rel_path,
            kind: EntryKind::Symlink,
            mode: mode_from_metadata(&metadata),
            bytes: None,
            link_target: Some(
                fs::read_link(path)
                    .with_context(|| format!("read link {}", path.display()))?
                    .to_string_lossy()
                    .to_string(),
            ),
        });
    }
    if file_type.is_file() {
        let bytes = fs::read(path).with_context(|| format!("read file {}", path.display()))?;
        return Ok(CapturedEntry {
            path: rel_path,
            kind: EntryKind::File,
            mode: mode_from_metadata(&metadata),
            bytes: Some(bytes),
            link_target: None,
        });
    }
    Err(anyhow!("unsupported capture path: {}", path.display()))
}

fn list_snapshots(
    repo: &mut Repository<Pile<Blake3>>,
    branch_id: Id,
) -> Result<Vec<SnapshotInfo>> {
    let mut ws = repo
        .pull(branch_id)
        .map_err(|err| anyhow!("pull workspace branch: {err:?}"))?;
    let catalog = ws.checkout(..).context("checkout workspace branch")?;

    let mut snapshots = Vec::new();
    for (snapshot_id, created_at) in find!(
        (snapshot_id: Id, created_at: Value<NsTAIInterval>),
        pattern!(&catalog, [{
            ?snapshot_id @
            playground_workspace::kind: playground_workspace::kind_snapshot,
            playground_workspace::created_at: ?created_at,
        }])
    ) {
        let label = load_string_attr(&mut ws, &catalog, snapshot_id, playground_workspace::label)?;
        let root_path =
            load_string_attr(&mut ws, &catalog, snapshot_id, playground_workspace::root_path)?;
        let entry_count = count_entries(&catalog, snapshot_id);
        let (lower, _): (Epoch, Epoch) = created_at.from_value();
        let created_key = interval_key(created_at);
        snapshots.push(SnapshotInfo {
            id: snapshot_id,
            created_at: lower,
            created_key,
            label,
            root_path,
            entry_count,
        });
    }

    snapshots.sort_by(|a, b| b.created_key.cmp(&a.created_key));
    Ok(snapshots)
}

fn restore_snapshot(
    repo: &mut Repository<Pile<Blake3>>,
    branch_id: Id,
    snapshot_id: Option<Id>,
    target_root: &Path,
    force: bool,
) -> Result<Option<Id>> {
    let mut ws = repo
        .pull(branch_id)
        .map_err(|err| anyhow!("pull workspace branch: {err:?}"))?;
    let catalog = ws.checkout(..).context("checkout workspace branch")?;

    let snapshot_id = match snapshot_id {
        Some(id) => Some(id),
        None => latest_snapshot(&catalog).context("find latest snapshot")?,
    };

    let Some(snapshot_id) = snapshot_id else {
        return Ok(None);
    };

    let entries = collect_snapshot_entries(&mut ws, &catalog, snapshot_id)?;
    restore_entries(target_root, &entries, force)?;

    Ok(Some(snapshot_id))
}

fn latest_snapshot(catalog: &TribleSet) -> Result<Option<Id>> {
    let mut latest: Option<(Id, Value<NsTAIInterval>)> = None;
    for (snapshot_id, created_at) in find!(
        (snapshot_id: Id, created_at: Value<NsTAIInterval>),
        pattern!(&catalog, [{
            ?snapshot_id @
            playground_workspace::kind: playground_workspace::kind_snapshot,
            playground_workspace::created_at: ?created_at,
        }])
    ) {
        let key = interval_key(created_at);
        match latest {
            Some((_, current)) if interval_key(current) >= key => {}
            _ => latest = Some((snapshot_id, created_at)),
        }
    }
    Ok(latest.map(|(id, _)| id))
}

fn collect_snapshot_entries(
    ws: &mut Workspace<Pile<Blake3>>,
    catalog: &TribleSet,
    snapshot_id: Id,
) -> Result<Vec<CapturedEntry>> {
    let mut entries = Vec::new();
    for (entry_id,) in find!(
        (entry_id: Id),
        pattern!(catalog, [{
            snapshot_id @
            playground_workspace::entry: ?entry_id,
        }])
    ) {
        entries.push(load_entry(ws, catalog, entry_id)?);
    }
    Ok(entries)
}

fn load_entry(
    ws: &mut Workspace<Pile<Blake3>>,
    catalog: &TribleSet,
    entry_id: Id,
) -> Result<CapturedEntry> {
    let kind = find!(
        (kind: Id),
        pattern!(catalog, [{
            entry_id @
            playground_workspace::kind: ?kind,
        }])
    )
    .into_iter()
    .next()
    .map(|(kind,)| kind)
    .ok_or_else(|| anyhow!("workspace entry missing kind: {entry_id:x}"))?;

    let path = load_string_attr(ws, catalog, entry_id, playground_workspace::path)?
        .ok_or_else(|| anyhow!("workspace entry missing path: {entry_id:x}"))?;

    let mode = load_u256_attr(catalog, entry_id, playground_workspace::mode)
        .and_then(u256be_to_u64)
        .and_then(|value| u32::try_from(value).ok());

    let bytes_handle = load_handle_attr::<FileBytes>(catalog, entry_id, playground_workspace::bytes);
    let bytes = match bytes_handle {
        Some(handle) => {
            let blob: Blob<FileBytes> = ws.get(handle).context("read workspace blob")?;
            Some(blob.bytes.as_ref().to_vec())
        }
        None => None,
    };

    let link_target =
        load_string_attr(ws, catalog, entry_id, playground_workspace::link_target)?;

    let entry_kind = if kind == playground_workspace::kind_file {
        EntryKind::File
    } else if kind == playground_workspace::kind_dir {
        EntryKind::Dir
    } else if kind == playground_workspace::kind_symlink {
        EntryKind::Symlink
    } else {
        return Err(anyhow!("unknown workspace entry kind: {kind:x}"));
    };

    Ok(CapturedEntry {
        path,
        kind: entry_kind,
        mode,
        bytes,
        link_target,
    })
}

fn restore_entries(target_root: &Path, entries: &[CapturedEntry], force: bool) -> Result<()> {
    if !target_root.exists() {
        fs::create_dir_all(target_root).with_context(|| {
            format!("create workspace root {}", target_root.display())
        })?;
    }

    for entry in entries {
        let rel = Path::new(entry.path.as_str());
        if rel
            .is_absolute()
            || rel
                .components()
                .any(|c| std::matches!(c, Component::ParentDir))
        {
            return Err(anyhow!("invalid snapshot path: {}", entry.path));
        }
        let target = target_root.join(rel);
        match entry.kind {
            EntryKind::Dir => {
                fs::create_dir_all(&target)
                    .with_context(|| format!("create dir {}", target.display()))?;
                if let Some(mode) = entry.mode {
                    set_mode(&target, mode)?;
                }
            }
            EntryKind::Symlink => {
                let target_parent = target
                    .parent()
                    .ok_or_else(|| anyhow!("invalid snapshot path: {}", entry.path))?;
                fs::create_dir_all(target_parent).with_context(|| {
                    format!("create dir {}", target_parent.display())
                })?;
                if target.exists() {
                    if !force {
                        return Err(anyhow!("path exists: {}", target.display()));
                    }
                    remove_existing(&target)?;
                }
                let link_target = entry
                    .link_target
                    .as_ref()
                    .ok_or_else(|| anyhow!("missing link target for {}", entry.path))?;
                create_symlink(Path::new(link_target), &target)?;
            }
            EntryKind::File => {
                let target_parent = target
                    .parent()
                    .ok_or_else(|| anyhow!("invalid snapshot path: {}", entry.path))?;
                fs::create_dir_all(target_parent).with_context(|| {
                    format!("create dir {}", target_parent.display())
                })?;
                if target.exists() && !force {
                    return Err(anyhow!("path exists: {}", target.display()));
                }
                let bytes = entry
                    .bytes
                    .as_ref()
                    .ok_or_else(|| anyhow!("missing file contents for {}", entry.path))?;
                if force {
                    fs::write(&target, bytes)
                        .with_context(|| format!("write file {}", target.display()))?;
                } else {
                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)
                        .with_context(|| format!("create file {}", target.display()))?;
                    use std::io::Write;
                    file.write_all(bytes)
                        .with_context(|| format!("write file {}", target.display()))?;
                }
                if let Some(mode) = entry.mode {
                    set_mode(&target, mode)?;
                }
            }
        }
    }

    Ok(())
}

fn collect_entries(root: &Path, path: &Path, entries: &mut Vec<CapturedEntry>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("read metadata {}", path.display()))?;
    let file_type = metadata.file_type();
    let is_root = path == root;

    if !is_root {
        if file_type.is_dir() {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                if EXCLUDED_DIRS.contains(&name) {
                    return Ok(());
                }
            }
        } else if should_exclude_file(path) {
            return Ok(());
        }
    }

    if file_type.is_symlink() {
        if !is_root {
            let rel = path.strip_prefix(root).context("strip prefix")?;
            entries.push(CapturedEntry {
                path: rel.to_string_lossy().to_string(),
                kind: EntryKind::Symlink,
                mode: mode_from_metadata(&metadata),
                bytes: None,
                link_target: Some(
                    fs::read_link(path)
                        .with_context(|| format!("read link {}", path.display()))?
                        .to_string_lossy()
                        .to_string(),
                ),
            });
        }
        return Ok(());
    }

    if file_type.is_dir() {
        if !is_root {
            let rel = path.strip_prefix(root).context("strip prefix")?;
            entries.push(CapturedEntry {
                path: rel.to_string_lossy().to_string(),
                kind: EntryKind::Dir,
                mode: mode_from_metadata(&metadata),
                bytes: None,
                link_target: None,
            });
        }
        for entry in fs::read_dir(path).with_context(|| format!("read dir {}", path.display()))? {
            let entry = entry.context("read dir entry")?;
            collect_entries(root, &entry.path(), entries)?;
        }
        return Ok(());
    }

    if file_type.is_file() && !is_root {
        let rel = path.strip_prefix(root).context("strip prefix")?;
        let bytes = fs::read(path).with_context(|| format!("read file {}", path.display()))?;
        entries.push(CapturedEntry {
            path: rel.to_string_lossy().to_string(),
            kind: EntryKind::File,
            mode: mode_from_metadata(&metadata),
            bytes: Some(bytes),
            link_target: None,
        });
    }

    Ok(())
}

fn should_exclude_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|name| name.to_str());
    if let Some(name) = name {
        if EXCLUDED_FILES.contains(&name) {
            return true;
        }
    }
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        if ext == "pile" {
            return true;
        }
    }
    false
}

fn count_entries(catalog: &TribleSet, snapshot_id: Id) -> usize {
    find!(
        (entry_id: Id),
        pattern!(catalog, [{
            snapshot_id @ playground_workspace::entry: ?entry_id,
        }])
    )
    .into_iter()
    .count()
}

fn load_string_attr(
    ws: &mut Workspace<Pile<Blake3>>,
    catalog: &TribleSet,
    entity_id: Id,
    attr: Attribute<Handle<Blake3, LongString>>,
) -> Result<Option<String>> {
    let handle = find!(
        (value: Value<Handle<Blake3, LongString>>),
        pattern!(catalog, [{
            entity_id @
            attr: ?value,
        }])
    )
    .into_iter()
    .next()
    .map(|(handle,)| handle);

    let Some(handle) = handle else {
        return Ok(None);
    };
    let view: View<str> = ws.get(handle).context("read text blob")?;
    Ok(Some(view.as_ref().to_string()))
}

fn load_u256_attr(
    catalog: &TribleSet,
    entity_id: Id,
    attr: Attribute<U256BE>,
) -> Option<Value<U256BE>> {
    find!(
        (value: Value<U256BE>),
        pattern!(catalog, [{
            entity_id @
            attr: ?value,
        }])
    )
    .into_iter()
    .next()
    .map(|(value,)| value)
}

fn load_handle_attr<S>(
    catalog: &TribleSet,
    entity_id: Id,
    attr: Attribute<Handle<Blake3, S>>,
) -> Option<Value<Handle<Blake3, S>>>
where
    S: BlobSchema,
{
    find!(
        (value: Value<Handle<Blake3, S>>),
        pattern!(catalog, [{
            entity_id @
            attr: ?value,
        }])
    )
    .into_iter()
    .next()
    .map(|(value,)| value)
}

fn u256be_to_u64(value: Value<U256BE>) -> Option<u64> {
    let raw = value.raw;
    if raw[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    let bytes: [u8; 8] = raw[24..32].try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

#[cfg(unix)]
fn mode_from_metadata(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode())
}

#[cfg(not(unix))]
fn mode_from_metadata(_: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("set permissions {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("create symlink {}", link.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link: &Path) -> Result<()> {
    Err(anyhow!("symlink creation unsupported on this platform"))
}

fn remove_existing(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("remove dir {}", path.display()))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("remove file {}", path.display()))?;
    }
    Ok(())
}

fn now_epoch() -> Epoch {
    Epoch::now().unwrap_or_else(|_| Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0))
}

fn epoch_interval(epoch: Epoch) -> Value<NsTAIInterval> {
    (epoch, epoch).to_value()
}

fn interval_key(interval: Value<NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.from_value();
    epoch_key(lower)
}

fn epoch_key(epoch: Epoch) -> i128 {
    epoch.to_tai_duration().total_nanoseconds()
}

fn print_snapshots(snapshots: &[SnapshotInfo]) {
    if snapshots.is_empty() {
        println!("no snapshots");
        return;
    }
    for snapshot in snapshots {
        let label = snapshot.label.as_deref().unwrap_or("-");
        let root = snapshot.root_path.as_deref().unwrap_or("-");
        println!(
            "{id:x}  {time}  entries={entries}  label={label}  root={root}",
            id = snapshot.id,
            time = snapshot.created_at,
            entries = snapshot.entry_count
        );
    }
}

fn parse_optional_hex_id(raw: Option<&str>) -> Result<Option<Id>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let id = Id::from_hex(raw).ok_or_else(|| anyhow!("invalid snapshot id {raw}"))?;
    Ok(Some(id))
}

// Snapshot is explicit via flag to avoid ambiguity; target stays positional.
