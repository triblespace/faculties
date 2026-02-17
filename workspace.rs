#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4"
//! rand_core = "0.6.4"
//! triblespace = "0.16.0"
//! ```

use std::fs;
use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use triblespace::core::blob::{Blob, Bytes};
use triblespace::core::metadata;
use triblespace::core::repo::branch as branch_proto;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{PushResult, Repository, Workspace};
use triblespace::macros::id_hex;
use triblespace::prelude::blobschemas::{FileBytes, LongString, SimpleArchive};
use triblespace::prelude::valueschemas::{Blake3, GenId, Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

const DEFAULT_WORKSPACE_BRANCH: &str = "workspace";
const ATLAS_BRANCH: &str = "atlas";
const CONFIG_BRANCH_ID: Id = id_hex!("4790808CF044F979FC7C2E47FCCB4A64");
const CONFIG_KIND_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");

mod config_schema {
    use super::*;
    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: NsTAIInterval;
        "20D37D92C2AEF5C98899C4C35AA1E35E" as workspace_branch_id: GenId;
    }
}

mod playground_workspace {
    use super::*;

    attributes! {
        "E39FB34126FE01A32F1D4B3DAD0F1874" as pub kind: GenId;
        "A95E92FB35943C570BE45FF811B0BD07" as pub created_at: NsTAIInterval;
        "5D36AA8480B30F62394911A003F20DDF" as pub parent_snapshot: GenId;
        "B667B02CEB4493232632473ECB782287" as pub root_path: Handle<Blake3, LongString>;
        "813B3BFA590103FFAD324FC72CDDC3F5" as pub state: Handle<Blake3, SimpleArchive>;
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
    /// Branch id for workspace snapshots (hex). Overrides config.
    #[arg(long, global = true)]
    branch_id: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Capture a workspace snapshot into the pile.
    Capture(WorkspaceCaptureArgs),
    /// List available workspace snapshots.
    List,
    /// Diff two snapshots.
    Diff(WorkspaceDiffArgs),
    /// Merge two snapshots with a common base.
    Merge(WorkspaceMergeArgs),
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

#[derive(Args)]
struct WorkspaceDiffArgs {
    /// Left snapshot id (hex) or 'latest'
    left: String,
    /// Right snapshot id (hex) or 'latest'
    right: String,
}

#[derive(Args)]
struct WorkspaceMergeArgs {
    /// Base snapshot id (hex) or 'latest'
    base: String,
    /// Ours snapshot id (hex) or 'latest'
    ours: String,
    /// Theirs snapshot id (hex) or 'latest'
    theirs: String,
    /// Optional label for the merged snapshot
    #[arg(long)]
    label: Option<String>,
    /// Conflict policy when both sides changed differently vs base
    #[arg(long, value_enum, default_value_t = ConflictPolicy::Fail)]
    conflicts: ConflictPolicy,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
#[value(rename_all = "kebab-case")]
enum ConflictPolicy {
    Fail,
    Ours,
    Theirs,
}

#[derive(Debug, Clone)]
struct SnapshotInfo {
    id: Id,
    created_at: Epoch,
    created_key: i128,
    label: Option<String>,
    root_path: Option<String>,
    state: Option<Value<Handle<Blake3, SimpleArchive>>>,
    parents: Vec<Id>,
    entry_count: usize,
}

#[derive(Debug, Clone)]
struct SnapshotData {
    id: Id,
    root_path: String,
    entries: Vec<CapturedEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedEntry {
    path: String,
    kind: EntryKind,
    mode: Option<u32>,
    bytes: Option<Vec<u8>>,
    link_target: Option<String>,
}

#[derive(Debug, Clone)]
struct MaterializedEntry {
    path: String,
    kind: Id,
    mode: Option<Value<U256BE>>,
    path_handle: Value<Handle<Blake3, LongString>>,
    bytes_handle: Option<Value<Handle<Blake3, FileBytes>>>,
    link_target_handle: Option<Value<Handle<Blake3, LongString>>>,
}

#[derive(Debug, Clone, Default)]
struct ConfigBranches {
    workspace_branch_id: Option<Id>,
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
        branch_id,
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
    let explicit_branch_id = parse_optional_hex_id_labeled(branch_id.as_deref(), "branch id")?;
    let cfg = with_repo(&pile, load_config_branches)?;
    let workspace_branch_id = resolve_branch_id(
        explicit_branch_id,
        cfg.workspace_branch_id,
        DEFAULT_WORKSPACE_BRANCH,
    )?;

    match cmd {
        Command::Capture(args) => cmd_capture(&pile, &branch, workspace_branch_id, args),
        Command::List => cmd_list(&pile, &branch, workspace_branch_id),
        Command::Diff(args) => cmd_diff(&pile, &branch, workspace_branch_id, args),
        Command::Merge(args) => cmd_merge(&pile, &branch, workspace_branch_id, args),
        Command::Restore(args) => cmd_restore(&pile, &branch, workspace_branch_id, args),
    }
}

fn emit_schema_to_atlas(pile_path: &Path) -> Result<()> {
    with_repo(pile_path, |repo| {
        let branch_id = if let Some(id) = find_branch_by_name(repo.storage_mut(), ATLAS_BRANCH)? {
            id
        } else {
            repo.create_branch(ATLAS_BRANCH, None)
                .map_err(|e| anyhow!("create branch: {e:?}"))?
                .release()
        };
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
        Ok(())
    })
}

fn build_workspace_metadata<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<Blake3>,
{
    let mut metadata = TribleSet::new();

    metadata += <GenId as metadata::ConstDescribe>::describe(blobs)?;
    metadata += <NsTAIInterval as metadata::ConstDescribe>::describe(blobs)?;
    metadata += <U256BE as metadata::ConstDescribe>::describe(blobs)?;
    metadata += <Handle<Blake3, LongString> as metadata::ConstDescribe>::describe(blobs)?;
    metadata += <Handle<Blake3, SimpleArchive> as metadata::ConstDescribe>::describe(blobs)?;
    metadata += <FileBytes as metadata::ConstDescribe>::describe(blobs)?;
    metadata += <SimpleArchive as metadata::ConstDescribe>::describe(blobs)?;
    metadata += <LongString as metadata::ConstDescribe>::describe(blobs)?;

    metadata += describe_attribute(blobs, &playground_workspace::kind, "workspace_kind")?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::created_at,
        "workspace_created_at",
    )?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::parent_snapshot,
        "workspace_parent_snapshot",
    )?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::root_path,
        "workspace_root_path",
    )?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::state,
        "workspace_state",
    )?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::label,
        "workspace_label",
    )?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::entry,
        "workspace_entry",
    )?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::path,
        "workspace_path",
    )?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::mode,
        "workspace_mode",
    )?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::bytes,
        "workspace_bytes",
    )?;
    metadata += describe_attribute(
        blobs,
        &playground_workspace::link_target,
        "workspace_link_target",
    )?;

    metadata += describe_kind(
        blobs,
        &playground_workspace::kind_snapshot,
        "workspace_snapshot",
        "Workspace snapshot kind.",
    )?;
    metadata += describe_kind(
        blobs,
        &playground_workspace::kind_file,
        "workspace_file",
        "Workspace file entry kind.",
    )?;
    metadata += describe_kind(
        blobs,
        &playground_workspace::kind_dir,
        "workspace_dir",
        "Workspace directory entry kind.",
    )?;
    metadata += describe_kind(
        blobs,
        &playground_workspace::kind_symlink,
        "workspace_symlink",
        "Workspace symlink entry kind.",
    )?;

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
    let mut tribles = TribleSet::new();
    tribles += metadata::Describe::describe(attribute, blobs)?;
    let handle = blobs.put(name.to_owned())?;
    let attribute_id = attribute.id();
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
    let mut tribles = TribleSet::new();
    let name_handle = blobs.put(name.to_string())?;
    let description_handle = blobs.put(description.to_string())?;

    tribles += entity! { ExclusiveId::force_ref(id) @
        metadata::name: name_handle,
        metadata::description: description_handle,
    };
    Ok(tribles)
}

fn cmd_capture(pile: &Path, branch: &str, branch_id: Id, args: WorkspaceCaptureArgs) -> Result<()> {
    let mappings = build_mappings(&args.paths)?;
    let snapshot_id = with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch)?;
        capture_snapshot(repo, branch_id, &mappings, args.label.as_deref())
    })?;
    println!("snapshot: {snapshot_id:x}");
    Ok(())
}

fn cmd_list(pile: &Path, branch: &str, branch_id: Id) -> Result<()> {
    let snapshots = with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch)?;
        list_snapshots(repo, branch_id)
    })?;
    print_snapshots(&snapshots);
    Ok(())
}

fn cmd_diff(pile: &Path, branch: &str, branch_id: Id, args: WorkspaceDiffArgs) -> Result<()> {
    with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch)?;
        let mut ws = repo
            .pull(branch_id)
            .map_err(|err| anyhow!("pull workspace branch: {err:?}"))?;
        let catalog = ws.checkout(..).context("checkout workspace branch")?;

        let left_id = resolve_snapshot_id(&catalog, args.left.as_str())?;
        let right_id = resolve_snapshot_id(&catalog, args.right.as_str())?;
        let left = load_snapshot_data(&mut ws, &catalog, left_id)?;
        let right = load_snapshot_data(&mut ws, &catalog, right_id)?;

        if left.root_path != right.root_path {
            println!(
                "root differs: left={} right={}",
                left.root_path, right.root_path
            );
        }

        let left_map = build_entry_map(&left.entries)?;
        let right_map = build_entry_map(&right.entries)?;
        let mut paths = BTreeSet::new();
        for path in left_map.keys() {
            paths.insert(path.clone());
        }
        for path in right_map.keys() {
            paths.insert(path.clone());
        }

        let mut added = 0usize;
        let mut removed = 0usize;
        let mut modified = 0usize;
        for path in paths {
            let l = left_map.get(path.as_str());
            let r = right_map.get(path.as_str());
            match (l, r) {
                (None, Some(_)) => {
                    added += 1;
                    println!("A {path}");
                }
                (Some(_), None) => {
                    removed += 1;
                    println!("D {path}");
                }
                (Some(a), Some(b)) if a != b => {
                    modified += 1;
                    println!("M {path}");
                }
                _ => {}
            }
        }

        println!(
            "summary: +{added} -{removed} ~{modified} (left={:x} right={:x})",
            left.id, right.id
        );
        Ok(())
    })
}

fn cmd_merge(pile: &Path, branch: &str, branch_id: Id, args: WorkspaceMergeArgs) -> Result<()> {
    with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch)?;
        let mut ws = repo
            .pull(branch_id)
            .map_err(|err| anyhow!("pull workspace branch: {err:?}"))?;
        let catalog = ws.checkout(..).context("checkout workspace branch")?;

        let base_id = resolve_snapshot_id(&catalog, args.base.as_str())?;
        let ours_id = resolve_snapshot_id(&catalog, args.ours.as_str())?;
        let theirs_id = resolve_snapshot_id(&catalog, args.theirs.as_str())?;

        let base = load_snapshot_data(&mut ws, &catalog, base_id)?;
        let ours = load_snapshot_data(&mut ws, &catalog, ours_id)?;
        let theirs = load_snapshot_data(&mut ws, &catalog, theirs_id)?;

        let merged_root = merge_root_path(
            base.root_path.as_str(),
            ours.root_path.as_str(),
            theirs.root_path.as_str(),
            args.conflicts,
        )?;

        let base_map = build_entry_map(&base.entries)?;
        let ours_map = build_entry_map(&ours.entries)?;
        let theirs_map = build_entry_map(&theirs.entries)?;
        let mut all_paths = BTreeSet::new();
        for path in base_map.keys() {
            all_paths.insert(path.clone());
        }
        for path in ours_map.keys() {
            all_paths.insert(path.clone());
        }
        for path in theirs_map.keys() {
            all_paths.insert(path.clone());
        }

        let mut merged_entries = Vec::new();
        let mut conflicts = Vec::new();
        for path in all_paths {
            let base_entry = base_map.get(path.as_str());
            let ours_entry = ours_map.get(path.as_str());
            let theirs_entry = theirs_map.get(path.as_str());
            match merge_path(base_entry, ours_entry, theirs_entry, args.conflicts) {
                Ok(Some(entry)) => merged_entries.push(entry),
                Ok(None) => {}
                Err(_) => conflicts.push(path),
            }
        }

        if !conflicts.is_empty() && args.conflicts == ConflictPolicy::Fail {
            let preview = conflicts.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
            return Err(anyhow!(
                "merge conflicts on {} path(s): {}{}",
                conflicts.len(),
                preview,
                if conflicts.len() > 8 { ", ..." } else { "" }
            ));
        }

        merged_entries.sort_by(|a, b| a.path.cmp(&b.path));
        let label = args.label.unwrap_or_else(|| {
            format!(
                "merge:{}+{}<-{}",
                id_short(ours_id),
                id_short(theirs_id),
                id_short(base_id),
            )
        });
        let snapshot_id = write_snapshot(
            &mut ws,
            merged_root.as_str(),
            &merged_entries,
            Some(label.as_str()),
            &[ours_id, theirs_id],
        );
        repo.push(&mut ws)
            .map_err(|err| anyhow!("push merged snapshot: {err:?}"))?;

        println!("snapshot: {snapshot_id:x}");
        println!(
            "merged entries={} conflicts={} policy={:?}",
            merged_entries.len(),
            conflicts.len(),
            args.conflicts
        );
        Ok(())
    })
}

fn cmd_restore(pile: &Path, branch: &str, branch_id: Id, args: WorkspaceRestoreArgs) -> Result<()> {
    let snapshot_id = parse_optional_hex_id(args.snapshot.as_deref())?;
    let target = args.target.unwrap_or_else(|| PathBuf::from("."));
    let restored = with_repo(pile, |repo| {
        ensure_branch_with_id(repo, branch_id, branch)?;
        restore_snapshot(repo, branch_id, snapshot_id, &target, args.force)
    })?;
    match restored {
        Some(id) => println!("restored: {id:x}"),
        None => println!("no snapshots found"),
    }
    Ok(())
}

fn open_repo(path: &Path) -> Result<Repository<Pile<Blake3>>> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }

    let mut pile = Pile::<Blake3>::open(path)
        .map_err(|e| anyhow!("open pile {}: {e:?}", path.display()))?;
    if let Err(err) = pile.restore() {
        // Avoid Drop warnings on early errors.
        let _ = pile.close();
        return Err(anyhow!("restore pile {}: {err:?}", path.display()));
    }
    let signing_key = SigningKey::generate(&mut OsRng);
    Ok(Repository::new(pile, signing_key))
}

fn with_repo<T>(
    pile: &Path,
    f: impl FnOnce(&mut Repository<Pile<Blake3>>) -> Result<T>,
) -> Result<T> {
    let mut repo = open_repo(pile)?;
    let result = f(&mut repo);
    let close_res = repo.close().map_err(|err| anyhow!("close pile: {err:?}"));
    if let Err(err) = close_res {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }
    result
}

fn ensure_branch_with_id(
    repo: &mut Repository<Pile<Blake3>>,
    branch_id: Id,
    branch_name: &str,
) -> Result<()> {
    if repo
        .storage_mut()
        .head(branch_id)
        .map_err(|e| anyhow!("branch head {branch_name}: {e:?}"))?
        .is_some()
    {
        return Ok(());
    }
    let name_blob = branch_name.to_owned().to_blob();
    let name_handle = name_blob.get_handle::<Blake3>();
    repo.storage_mut()
        .put(name_blob)
        .map_err(|e| anyhow!("store branch name {branch_name}: {e:?}"))?;
    let metadata = branch_proto::branch_unsigned(branch_id, name_handle, None);
    let metadata_handle = repo
        .storage_mut()
        .put(metadata.to_blob())
        .map_err(|e| anyhow!("store branch metadata {branch_name}: {e:?}"))?;
    let result = repo
        .storage_mut()
        .update(branch_id, None, Some(metadata_handle))
        .map_err(|e| anyhow!("create branch {branch_name} ({branch_id:x}): {e:?}"))?;
    match result {
        PushResult::Success() | PushResult::Conflict(_) => Ok(()),
    }
}

fn load_config_branches(repo: &mut Repository<Pile<Blake3>>) -> Result<ConfigBranches> {
    let Some(_) = repo
        .storage_mut()
        .head(CONFIG_BRANCH_ID)
        .map_err(|e| anyhow!("config branch head: {e:?}"))?
    else {
        return Ok(ConfigBranches::default());
    };

    let mut ws = repo
        .pull(CONFIG_BRANCH_ID)
        .map_err(|e| anyhow!("pull config workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout config workspace: {e:?}"))?;

    let mut latest: Option<(Id, i128)> = None;
    for (config_id, updated_at) in find!(
        (config_id: Id, updated_at: Value<NsTAIInterval>),
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

    let workspace_branch_id = find!(
        (entity: Id, value: Value<GenId>),
        pattern!(&space, [{ ?entity @ config_schema::workspace_branch_id: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == config_id).then_some(value.from_value()));

    Ok(ConfigBranches { workspace_branch_id })
}

fn resolve_branch_id(explicit_id: Option<Id>, configured_id: Option<Id>, branch_name: &str) -> Result<Id> {
    if let Some(id) = explicit_id {
        return Ok(id);
    }
    configured_id.ok_or_else(|| {
        anyhow!(
            "missing {branch_name} branch id in config (set via `playground config set workspace-branch-id <hex-id>`)"
        )
    })
}

fn find_branch_by_name(pile: &mut Pile<Blake3>, branch_name: &str) -> Result<Option<Id>> {
    let reader = pile.reader().context("pile reader")?;
    let iter = pile.branches().context("list branches")?;
    let expected = branch_name.to_owned().to_blob().get_handle::<Blake3>();

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
    let catalog = ws.checkout(..).context("checkout workspace branch")?;

    let (root_path, entries) =
        collect_mapped_entries(mappings).context("collect workspace entries")?;
    let parent = latest_snapshot(&catalog).context("find latest snapshot")?;
    let parents = parent.into_iter().collect::<Vec<_>>();
    let snapshot_id = write_snapshot(&mut ws, root_path.as_str(), &entries, label, &parents);
    repo.push(&mut ws)
        .map_err(|err| anyhow!("push snapshot: {err:?}"))?;
    Ok(snapshot_id)
}

fn write_snapshot(
    ws: &mut Workspace<Pile<Blake3>>,
    root_path: &str,
    entries: &[CapturedEntry],
    label: Option<&str>,
    parents: &[Id],
) -> Id {
    let snapshot_id = ufoid();
    let created_at = epoch_interval(now_epoch());
    let materialized_entries = materialize_entries(ws, entries);
    let root_handle = ws.put(root_path.to_owned());
    let state_handle = compute_state_handle(ws, root_handle, &materialized_entries);
    let mut change = TribleSet::new();
    change += entity! { &snapshot_id @
        playground_workspace::kind: playground_workspace::kind_snapshot,
        playground_workspace::created_at: created_at,
    };
    change += entity! { &snapshot_id @
        playground_workspace::root_path: root_handle,
        playground_workspace::state: state_handle,
    };

    if let Some(label) = label {
        let label_handle = ws.put(label.to_owned());
        change += entity! { &snapshot_id @ playground_workspace::label: label_handle };
    }
    for parent in parents {
        change += entity! { &snapshot_id @ playground_workspace::parent_snapshot: *parent };
    }

    for entry in &materialized_entries {
        let entry_set = build_entry_entity(entry);
        let entry_id = entry_set
            .root()
            .expect("entity! must export a single root id");
        change += entity! { &snapshot_id @ playground_workspace::entry: entry_id };
        change += entry_set;
    }

    ws.commit(change, None, Some("playground_workspace snapshot"));
    *snapshot_id
}

fn materialize_entries(
    ws: &mut Workspace<Pile<Blake3>>,
    entries: &[CapturedEntry],
) -> Vec<MaterializedEntry> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let kind = match entry.kind {
            EntryKind::File => playground_workspace::kind_file,
            EntryKind::Dir => playground_workspace::kind_dir,
            EntryKind::Symlink => playground_workspace::kind_symlink,
        };
        let mode = entry.mode.map(|value| (value as u64).to_value());
        let path_handle = ws.put(entry.path.clone());
        let bytes_handle = entry
            .bytes
            .as_ref()
            .map(|bytes| ws.put(Bytes::from_source(bytes.clone())));
        let link_target_handle = entry.link_target.as_ref().map(|target| ws.put(target.clone()));
        out.push(MaterializedEntry {
            path: entry.path.clone(),
            kind,
            mode,
            path_handle,
            bytes_handle,
            link_target_handle,
        });
    }
    out
}

fn compute_state_handle(
    ws: &mut Workspace<Pile<Blake3>>,
    root_handle: Value<Handle<Blake3, LongString>>,
    entries: &[MaterializedEntry],
) -> Value<Handle<Blake3, SimpleArchive>> {
    let mut canonical_entries = entries.to_vec();
    canonical_entries.sort_by(|a, b| a.path.cmp(&b.path));

    let mut state = TribleSet::new();
    let root_entity = entity! { _ @
        playground_workspace::kind: playground_workspace::kind_snapshot,
        playground_workspace::root_path: root_handle,
    };
    let state_root = root_entity
        .root()
        .expect("entity! must export a single root id");
    state += root_entity;

    for entry in canonical_entries.iter() {
        let entry_set = build_entry_entity(entry);
        let entry_id = entry_set
            .root()
            .expect("entity! must export a single root id");
        state += entity! { ExclusiveId::force_ref(&state_root) @ playground_workspace::entry: entry_id };
        state += entry_set;
    }

    let blob: Blob<SimpleArchive> = state.to_blob();
    ws.put(blob)
}

fn build_entry_entity(entry: &MaterializedEntry) -> Fragment {
    entity! { _ @
        playground_workspace::path: entry.path_handle,
        playground_workspace::kind: entry.kind,
        playground_workspace::mode?: entry.mode,
        playground_workspace::bytes?: entry.bytes_handle,
        playground_workspace::link_target?: entry.link_target_handle,
    }
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
        let state = load_handle_attr::<SimpleArchive>(&catalog, snapshot_id, playground_workspace::state);
        let parents = load_id_attrs(&catalog, snapshot_id, playground_workspace::parent_snapshot);
        let entry_count = count_entries(&catalog, snapshot_id);
        let (lower, _): (Epoch, Epoch) = created_at.from_value();
        let created_key = interval_key(created_at);
        snapshots.push(SnapshotInfo {
            id: snapshot_id,
            created_at: lower,
            created_key,
            label,
            root_path,
            state,
            parents,
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

    let root_path =
        load_string_attr(&mut ws, &catalog, snapshot_id, playground_workspace::root_path)?;
    let restore_root = resolve_restore_root(target_root, root_path.as_deref())?;
    let entries = collect_snapshot_entries(&mut ws, &catalog, snapshot_id)?;
    restore_entries(&restore_root, &entries, force)?;

    Ok(Some(snapshot_id))
}

fn resolve_restore_root(target_root: &Path, root_path: Option<&str>) -> Result<PathBuf> {
    let Some(root_path) = root_path.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(target_root.to_path_buf());
    };

    if root_path == "." {
        return Ok(target_root.to_path_buf());
    }

    let rel = Path::new(root_path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| std::matches!(c, Component::ParentDir))
    {
        return Err(anyhow!("invalid snapshot root_path: {}", root_path));
    }

    Ok(target_root.join(rel))
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

fn resolve_snapshot_id(catalog: &TribleSet, raw: &str) -> Result<Id> {
    if raw.eq_ignore_ascii_case("latest") {
        return latest_snapshot(catalog)?
            .ok_or_else(|| anyhow!("no snapshots found"));
    }
    parse_hex_id(raw)
}

fn parse_hex_id(raw: &str) -> Result<Id> {
    Id::from_hex(raw).ok_or_else(|| anyhow!("invalid snapshot id {raw}"))
}

fn load_snapshot_data(
    ws: &mut Workspace<Pile<Blake3>>,
    catalog: &TribleSet,
    snapshot_id: Id,
) -> Result<SnapshotData> {
    let root_path = load_string_attr(ws, catalog, snapshot_id, playground_workspace::root_path)?
        .unwrap_or_else(|| ".".to_string());
    let entries = collect_snapshot_entries(ws, catalog, snapshot_id)?;
    Ok(SnapshotData {
        id: snapshot_id,
        root_path,
        entries,
    })
}

fn build_entry_map(entries: &[CapturedEntry]) -> Result<HashMap<String, CapturedEntry>> {
    let mut map = HashMap::new();
    for entry in entries {
        if map.insert(entry.path.clone(), entry.clone()).is_some() {
            return Err(anyhow!("duplicate path in snapshot: {}", entry.path));
        }
    }
    Ok(map)
}

fn merge_root_path(
    base: &str,
    ours: &str,
    theirs: &str,
    policy: ConflictPolicy,
) -> Result<String> {
    if ours == theirs {
        return Ok(ours.to_string());
    }
    if base == ours {
        return Ok(theirs.to_string());
    }
    if base == theirs {
        return Ok(ours.to_string());
    }
    match policy {
        ConflictPolicy::Ours => Ok(ours.to_string()),
        ConflictPolicy::Theirs => Ok(theirs.to_string()),
        ConflictPolicy::Fail => Err(anyhow!(
            "conflicting root_path values (base={base}, ours={ours}, theirs={theirs})"
        )),
    }
}

fn merge_path(
    base: Option<&CapturedEntry>,
    ours: Option<&CapturedEntry>,
    theirs: Option<&CapturedEntry>,
    policy: ConflictPolicy,
) -> Result<Option<CapturedEntry>> {
    if ours == theirs {
        return Ok(ours.cloned());
    }
    if base == ours {
        return Ok(theirs.cloned());
    }
    if base == theirs {
        return Ok(ours.cloned());
    }
    match policy {
        ConflictPolicy::Ours => Ok(ours.cloned()),
        ConflictPolicy::Theirs => Ok(theirs.cloned()),
        ConflictPolicy::Fail => Err(anyhow!("merge conflict")),
    }
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

fn load_id_attrs(catalog: &TribleSet, entity_id: Id, attr: Attribute<GenId>) -> Vec<Id> {
    let mut ids = find!(
        (value: Value<GenId>),
        pattern!(catalog, [{
            entity_id @
            attr: ?value,
        }])
    )
    .into_iter()
    .filter_map(|(value,)| Id::try_from_value(&value).ok())
    .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids
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
        let state = snapshot
            .state
            .map(archive_handle_short)
            .unwrap_or_else(|| "-".to_string());
        let parents = if snapshot.parents.is_empty() {
            "-".to_string()
        } else {
            snapshot
                .parents
                .iter()
                .map(|id| id_short(*id))
                .collect::<Vec<_>>()
                .join(",")
        };
        println!(
            "{id:x}  {time}  entries={entries}  label={label}  root={root}  state={state}  parents={parents}",
            id = snapshot.id,
            time = snapshot.created_at,
            entries = snapshot.entry_count
        );
    }
}

fn id_short(id: Id) -> String {
    let full = format!("{id:x}");
    full.chars().take(8).collect()
}

fn archive_handle_short(handle: Value<Handle<Blake3, SimpleArchive>>) -> String {
    let mut out = String::with_capacity(8);
    for byte in handle.raw.iter().take(4) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
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

fn parse_optional_hex_id_labeled(raw: Option<&str>, label: &str) -> Result<Option<Id>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let id = Id::from_hex(trimmed).ok_or_else(|| anyhow!("invalid {label} {trimmed}"))?;
    Ok(Some(id))
}

// Snapshot is explicit via flag to avoid ambiguity; target stays positional.
