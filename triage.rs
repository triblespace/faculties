#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4.2.3"
//! rand_core = "0.6.4"
//! triblespace = "0.16.0"
//! ```

use anyhow::{Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::core::metadata;
use triblespace::core::repo::branch as branch_proto;
use triblespace::core::repo::pile::{Pile, ReadError};
use triblespace::core::repo::{BlobStoreMeta, PullError, PushResult, Repository, Workspace};
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::*;

const DEFAULT_MAIN_BRANCH: &str = "main";
const DEFAULT_CONFIG_BRANCH: &str = "config";
const DEFAULT_LOCAL_BRANCH: &str = "local-messages";
const DEFAULT_RELATIONS_BRANCH: &str = "relations";
const DEFAULT_ATLAS_BRANCH: &str = "atlas";
const DEFAULT_COMPASS_BRANCH: &str = "compass";
const DEFAULT_EXEC_BRANCH: &str = "cognition";
const DEFAULT_TEAMS_BRANCH: &str = "teams";
const DEFAULT_WORKSPACE_BRANCH: &str = "workspace";
const DEFAULT_ARCHIVE_BRANCH: &str = "archive";
const DEFAULT_WEB_BRANCH: &str = "web";
const DEFAULT_MEDIA_BRANCH: &str = "media";
const FIXED_CONFIG_BRANCH_ID: Id = id_hex!("4790808CF044F979FC7C2E47FCCB4A64");

const KIND_PERSON_ID: Id = id_hex!("D8ADDE47121F4E7868017463EC860726");
const KIND_LOCAL_MESSAGE_ID: Id = id_hex!("A3556A66B00276797FCE8A2742AB850F");
const KIND_LOCAL_READ_ID: Id = id_hex!("B663C15BB6F2BF591EA870386DD48537");
const KIND_CONFIG_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");
const KIND_EXEC_REQUEST_ID: Id = id_hex!("3D2512DAE86B14B9049930F3146A3188");
const KIND_EXEC_IN_PROGRESS_ID: Id = id_hex!("2D81A8D840822CF082DE5DE569B53730");
const KIND_EXEC_RESULT_ID: Id = id_hex!("DF7165210F066E84D93E9A430BB0D4BD");
const KIND_LLM_REQUEST_ID: Id = id_hex!("1524B4C030D4F10365D9DCEE801A09C8");
const KIND_LLM_IN_PROGRESS_ID: Id = id_hex!("16C69FC4928D54BF93E6F3222B4685A7");
const KIND_LLM_RESULT_ID: Id = id_hex!("DE498E4697F9F01219C75E7BC183DB91");
const KIND_REASON_EVENT_ID: Id = id_hex!("9D43BB36D8B4A6275CAF38A1D5DACF36");
const REPO_HEAD_ATTR: Id = id_hex!("272FBC56108F336C4D2E17289468C35F");
const REPO_PARENT_ATTR: Id = id_hex!("317044B612C690000D798CA660ECFD2A");
const REPO_CONTENT_ATTR: Id = id_hex!("4DD4DDD05CC31734B03ABB4E43188B1F");

type TextHandle = Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>>;

mod config {
    use super::*;
    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: valueschemas::GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: valueschemas::NsTAIInterval;
        "35E36AE7B60AD946661BD63B3CD64672" as branch: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "4E2F9CA7A8456DED8C43A3BE741ADA58" as branch_id: valueschemas::GenId;
        "EDEFFF6AFF6318E44CCF6A602B012604" as compass_branch_id: valueschemas::GenId;
        "C188E12ABBDD83D283A23DBAD4B784AF" as exec_branch_id: valueschemas::GenId;
        "2ED6FF7EAB93CB5608555AE4B9664CF8" as local_messages_branch_id: valueschemas::GenId;
        "D35F4F02E29825FBC790E324EFCD1B34" as relations_branch_id: valueschemas::GenId;
        "22A0E76B8044311563369298306906E3" as teams_branch_id: valueschemas::GenId;
        "20D37D92C2AEF5C98899C4C35AA1E35E" as workspace_branch_id: valueschemas::GenId;
        "047112FC535518D289E64FBE0B60F06E" as archive_branch_id: valueschemas::GenId;
        "A4DFF7BE658B1EA16F866E3039FFF8D6" as web_branch_id: valueschemas::GenId;
        "229941B84503AAE4976A49E020D1282B" as media_branch_id: valueschemas::GenId;
        "D1DC11B303725409AB8A30C6B59DB2D7" as persona_id: valueschemas::GenId;
    }
}

mod local {
    use super::*;
    attributes! {
        "95D58D3E68A43979F8AA51415541414C" as to: valueschemas::GenId;
        "53ECCC7489AF8D30EF385ED12073F4A3" as created_at: valueschemas::NsTAIInterval;
        "2213B191326E9B99605FA094E516E50E" as about_message: valueschemas::GenId;
        "99E92F483731FA6D59115A8D6D187A37" as reader: valueschemas::GenId;
    }
}

mod relations {
    use super::*;
    attributes! {
        "8F162B593D390E1424394DBF6883A72C" as alias: valueschemas::ShortString;
    }
}

mod exec {
    use super::*;
    attributes! {
        "AA2F34973589295FA70B538D92CD30F8" as kind: valueschemas::GenId;
        "79DD6A1A02E598033EDCE5C667E8E3E6" as command_text: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "AAD2627FB70DC16F6ADF8869AE1B203F" as requested_at: valueschemas::NsTAIInterval;
        "C4C3870642CAB5F55E7E575B1A62E640" as about_request: valueschemas::GenId;
        "B878792F16C0C27C776992FA053A2218" as started_at: valueschemas::NsTAIInterval;
        "B4B81B90EFB4D1F5EE62DDE9CB48025D" as finished_at: valueschemas::NsTAIInterval;
        "B68F9025545C7E616EB90C6440220348" as exit_code: valueschemas::U256BE;
        "BE4D1876B22EAF93AAD1175DB76D1C72" as stderr_text: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "E9C77284C7DDCF522A8AC4622FE3FB11" as error: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
    }
}

mod llm {
    use super::*;
    attributes! {
        "5F10520477A04E5FB322C85CC78C6762" as kind: valueschemas::GenId;
        "5A14A02113CE43A59881D0717726F465" as about_request: valueschemas::GenId;
        "0DA5DD275AA34F86B0297CC35F1B7395" as requested_at: valueschemas::NsTAIInterval;
        "1DE7C6BCE0223199368070A82EA23A7E" as started_at: valueschemas::NsTAIInterval;
        "238CF718317A94DB46B8D75E7CB6D609" as finished_at: valueschemas::NsTAIInterval;
        "9E9B829C473E416E9150D4B94A6A2DC4" as error: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
    }
}

mod reason {
    use super::*;
    attributes! {
        "B10329D5D1087D15A3DAFF7A7CC50696" as text: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "FBA9BC32A457C7BFFDB7E0181D3E82A4" as created_at: valueschemas::NsTAIInterval;
        "E6B1C728F1AE9F46CAB4DBB60D1A9528" as about_turn: valueschemas::GenId;
        "514F4FE9F560FB155450462C8CF50749" as command_text: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
    }
}

#[derive(Parser)]
#[command(
    name = "triage",
    about = "Doctor-style cross-instance diagnostics for playground agent piles"
)]
struct Cli {
    /// Path to the pile file to inspect
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Main branch name (used when config has no branch id)
    #[arg(long, default_value = DEFAULT_MAIN_BRANCH, global = true)]
    branch: String,
    /// Optional explicit main branch id (hex)
    #[arg(long, global = true)]
    branch_id: Option<String>,
    /// Config branch name
    #[arg(long, default_value = DEFAULT_CONFIG_BRANCH, global = true)]
    config_branch: String,
    /// Local messages branch name
    #[arg(long, default_value = DEFAULT_LOCAL_BRANCH, global = true)]
    local_branch: String,
    /// Relations branch name
    #[arg(long, default_value = DEFAULT_RELATIONS_BRANCH, global = true)]
    relations_branch: String,
    /// Atlas branch name for schema registration
    #[arg(long, default_value = DEFAULT_ATLAS_BRANCH, global = true)]
    atlas_branch: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Full health scan with queue + loop heuristics
    Scan {
        /// Max recent exec attempts used for loop diagnostics
        #[arg(long, default_value_t = 40)]
        recent: usize,
        /// Minimum repeated attempts to report as a probable loop
        #[arg(long, default_value_t = 3)]
        loop_min: usize,
        /// Mark in-progress requests older than this as stale
        #[arg(long, default_value_t = 15)]
        stale_min: i64,
    },
    /// Show recent exec attempts and repeated failure patterns
    Loops {
        #[arg(long, default_value_t = 40)]
        recent: usize,
        #[arg(long, default_value_t = 3)]
        min_repeat: usize,
    },
    /// Show an interleaved recent activity timeline (exec/llm/reason)
    Timeline {
        /// Max events to print (newest first)
        #[arg(long, default_value_t = 80)]
        recent: usize,
    },
    /// Inspect commit-chain integrity for the target branch
    Chain,
    /// Repair branch-level consistency issues
    Repair {
        #[command(subcommand)]
        command: RepairCommand,
    },
}

#[derive(Subcommand)]
enum RepairCommand {
    /// Merge duplicate named branches into canonical config-registered IDs
    BranchDuplicates {
        /// Preview actions without writing changes
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

#[derive(Debug, Clone, Default)]
struct ConfigSnapshot {
    updated_at: Option<i128>,
    branch_name: Option<String>,
    branch_id: Option<Id>,
    compass_branch_id: Option<Id>,
    exec_branch_id: Option<Id>,
    local_messages_branch_id: Option<Id>,
    relations_branch_id: Option<Id>,
    teams_branch_id: Option<Id>,
    workspace_branch_id: Option<Id>,
    archive_branch_id: Option<Id>,
    web_branch_id: Option<Id>,
    media_branch_id: Option<Id>,
    persona_id: Option<Id>,
}

#[derive(Debug, Clone)]
struct ExecRequestRow {
    id: Id,
    command: Option<String>,
    requested_at: Option<i128>,
}

#[derive(Debug, Clone)]
struct ExecInProgressRow {
    about_request: Id,
    started_at: Option<i128>,
}

#[derive(Debug, Clone)]
struct ExecResultRow {
    id: Id,
    about_request: Id,
    finished_at: Option<i128>,
    exit_code: Option<i64>,
    stderr_text: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ExecState {
    requests: HashMap<Id, ExecRequestRow>,
    in_progress: Vec<ExecInProgressRow>,
    results: Vec<ExecResultRow>,
}

#[derive(Debug, Clone)]
struct LlmRequestRow {
    requested_at: Option<i128>,
}

#[derive(Debug, Clone)]
struct LlmInProgressRow {
    about_request: Id,
    started_at: Option<i128>,
}

#[derive(Debug, Clone)]
struct LlmResultRow {
    about_request: Id,
    finished_at: Option<i128>,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct LlmState {
    requests: HashMap<Id, LlmRequestRow>,
    in_progress: Vec<LlmInProgressRow>,
    results: Vec<LlmResultRow>,
}

#[derive(Debug, Clone)]
struct ReasonEventRow {
    created_at: Option<i128>,
    text: Option<String>,
    about_turn: Option<Id>,
    command_text: Option<String>,
}

#[derive(Debug, Clone)]
struct TimelineRow {
    at: i128,
    source: &'static str,
    detail: String,
}

#[derive(Debug, Clone)]
struct ExecAttempt {
    request_id: Id,
    result_id: Id,
    finished_at: i128,
    command: String,
    exit_code: Option<i64>,
    fingerprint: String,
}

#[derive(Debug, Clone)]
struct PatternSummary {
    command: String,
    exit_code: Option<i64>,
    fingerprint: String,
    count: usize,
    latest: i128,
}

#[derive(Debug, Clone)]
struct LoopReport {
    recent: Vec<ExecAttempt>,
    top_patterns: Vec<PatternSummary>,
    contiguous_head: Option<PatternSummary>,
}

#[derive(Debug, Clone)]
struct ChainIssue {
    commit_hash: String,
    depth_from_head: usize,
    reason: String,
}

#[derive(Debug, Clone)]
struct ChainReport {
    commit_head: String,
    checked_commits: usize,
    issue: Option<ChainIssue>,
}

fn now_epoch() -> Epoch {
    Epoch::now().unwrap_or_else(|_| Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0))
}

fn interval_key(interval: Value<valueschemas::NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.from_value();
    lower.to_tai_duration().total_nanoseconds()
}

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
}

fn format_age(now_key: i128, past_key: i128) -> String {
    let delta_ns = now_key.saturating_sub(past_key);
    let delta_s = (delta_ns / 1_000_000_000).max(0) as i64;
    if delta_s < 60 {
        format!("{delta_s}s")
    } else if delta_s < 3600 {
        format!("{}m", delta_s / 60)
    } else if delta_s < 86_400 {
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

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
        .trim()
        .to_string()
}

fn parse_hex_id(raw: &str, label: &str) -> Result<Id> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{label} is empty");
    }
    Id::from_hex(trimmed).ok_or_else(|| anyhow!("invalid {label} {trimmed}"))
}

fn u256be_to_u64(value: Value<valueschemas::U256BE>) -> Option<u64> {
    let raw = value.raw;
    if raw[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    let bytes: [u8; 8] = raw[24..32].try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn read_text(ws: &mut Workspace<Pile<valueschemas::Blake3>>, handle: TextHandle) -> Result<String> {
    let view: View<str> = ws
        .get::<View<str>, blobschemas::LongString>(handle)
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
    Ok(Repository::new(pile, SigningKey::generate(&mut OsRng)))
}

fn pull_workspace(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_id: Id,
    context: &str,
) -> Result<Workspace<Pile<valueschemas::Blake3>>> {
    match repo.pull(branch_id) {
        Ok(ws) => Ok(ws),
        Err(err) => {
            let Some(valid_length) = pull_corrupt_valid_length(&err) else {
                return Err(anyhow!("{context}: {err:?}"));
            };
            eprintln!(
                "warning: {context}: corrupt pile tail (valid_length={valid_length}), restoring and retrying"
            );
            repo.storage_mut()
                .restore()
                .map_err(|restore_err| anyhow!("{context}: restore pile: {restore_err:?}"))?;
            repo.pull(branch_id)
                .map_err(|retry_err| anyhow!("{context} after restore: {retry_err:?}"))
        }
    }
}

fn pull_corrupt_valid_length<B: std::error::Error>(
    err: &PullError<ReadError, ReadError, B>,
) -> Option<usize> {
    match err {
        PullError::BlobReader(ReadError::CorruptPile { valid_length })
        | PullError::BranchStorage(ReadError::CorruptPile { valid_length }) => Some(*valid_length),
        _ => None,
    }
}

fn find_branch_by_name(
    pile: &mut Pile<valueschemas::Blake3>,
    branch_name: &str,
) -> Result<Option<Id>> {
    Ok(find_branch_ids_by_name(pile, branch_name)?
        .into_iter()
        .next())
}

fn find_branch_ids_by_name(
    pile: &mut Pile<valueschemas::Blake3>,
    branch_name: &str,
) -> Result<Vec<Id>> {
    let name_handle = branch_name
        .to_owned()
        .to_blob()
        .get_handle::<valueschemas::Blake3>();
    let reader = pile.reader().map_err(|e| anyhow!("pile reader: {e:?}"))?;
    let iter = pile
        .branches()
        .map_err(|e| anyhow!("list branches: {e:?}"))?;
    let mut matches = Vec::new();
    for branch_entry in iter {
        let branch_id = branch_entry.map_err(|e| anyhow!("branch id: {e:?}"))?;
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
        if name != name_handle {
            continue;
        }
        let head_ts = reader
            .metadata(head)
            .ok()
            .flatten()
            .map(|meta| meta.timestamp)
            .unwrap_or(0);
        matches.push((branch_id, head_ts));
    }
    matches.sort_by_key(|(id, ts)| (Reverse(*ts), format!("{id:x}")));
    Ok(matches.into_iter().map(|(id, _)| id).collect())
}

fn ensure_branch_with_id(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_id: Id,
    branch_name: &str,
) -> Result<()> {
    if repo
        .storage_mut()
        .head(branch_id)
        .map_err(|e| anyhow!("read branch {branch_id:x} head: {e:?}"))?
        .is_some()
    {
        return Ok(());
    }

    let name_blob = branch_name.to_owned().to_blob();
    let name_handle = name_blob.get_handle::<valueschemas::Blake3>();
    repo.storage_mut()
        .put(name_blob)
        .map_err(|e| anyhow!("store branch name blob: {e:?}"))?;
    let metadata = branch_proto::branch_unsigned(branch_id, name_handle, None);
    let metadata_handle = repo
        .storage_mut()
        .put(metadata.to_blob())
        .map_err(|e| anyhow!("store branch metadata: {e:?}"))?;
    let result = repo
        .storage_mut()
        .update(branch_id, None, Some(metadata_handle))
        .map_err(|e| anyhow!("create branch {branch_name} ({branch_id:x}): {e:?}"))?;
    match result {
        PushResult::Success() | PushResult::Conflict(_) => Ok(()),
    }
}

fn ensure_branch(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_name: &str,
) -> Result<Id> {
    if let Some(id) = find_branch_by_name(repo.storage_mut(), branch_name)? {
        return Ok(id);
    }

    let branch_id = *genid();
    ensure_branch_with_id(repo, branch_id, branch_name)?;
    Ok(branch_id)
}

fn push_workspace(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
) -> Result<()> {
    while let Some(mut conflict) = repo
        .try_push(ws)
        .map_err(|e| anyhow!("push workspace: {e:?}"))?
    {
        conflict
            .merge(ws)
            .map_err(|e| anyhow!("merge workspace: {e:?}"))?;
        *ws = conflict;
    }
    Ok(())
}

fn load_latest_config(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_name: &str,
) -> Result<Option<ConfigSnapshot>> {
    let Some(branch_id) = find_branch_by_name(repo.storage_mut(), branch_name)? else {
        return Ok(None);
    };
    load_latest_config_from_branch_id(repo, branch_id)
}

fn load_latest_config_from_branch_id(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_id: Id,
) -> Result<Option<ConfigSnapshot>> {
    let mut ws = pull_workspace(repo, branch_id, "pull config workspace")?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout config workspace: {e:?}"))?;

    let mut latest: Option<(Id, i128)> = None;
    for (config_id, updated_at) in find!(
        (config_id: Id, updated_at: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{
            ?config_id @
            config::kind: &KIND_CONFIG_ID,
            config::updated_at: ?updated_at,
        }])
    ) {
        let key = interval_key(updated_at);
        match latest {
            Some((_, best)) if best >= key => {}
            _ => latest = Some((config_id, key)),
        }
    }

    let Some((config_id, updated_at)) = latest else {
        return Ok(None);
    };

    let mut snapshot = ConfigSnapshot {
        updated_at: Some(updated_at),
        ..Default::default()
    };

    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::compass_branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.compass_branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::exec_branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.exec_branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::local_messages_branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.local_messages_branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::relations_branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.relations_branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::teams_branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.teams_branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::workspace_branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.workspace_branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::archive_branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.archive_branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::web_branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.web_branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::media_branch_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.media_branch_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config::persona_id: ?value }])
    ) {
        if entity == config_id {
            snapshot.persona_id = Some(value.from_value());
            break;
        }
    }
    for (entity, value) in find!(
        (entity: Id, value: TextHandle),
        pattern!(&space, [{ ?entity @ config::branch: ?value }])
    ) {
        if entity == config_id {
            snapshot.branch_name = Some(read_text(&mut ws, value)?);
            break;
        }
    }

    Ok(Some(snapshot))
}

fn resolve_target_branch(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    cli: &Cli,
    config: &Option<ConfigSnapshot>,
) -> Result<Id> {
    if let Some(raw_id) = cli.branch_id.as_ref() {
        return parse_hex_id(raw_id, "branch-id");
    }
    if let Some(id) = config.as_ref().and_then(|cfg| cfg.branch_id) {
        return Ok(id);
    }
    if let Some(id) = find_branch_by_name(repo.storage_mut(), cli.branch.as_str())? {
        return Ok(id);
    }
    bail!("missing target branch '{}'", cli.branch);
}

fn collect_exec_state(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
    space: &TribleSet,
) -> Result<ExecState> {
    let mut state = ExecState::default();

    for (request_id,) in find!(
        (request_id: Id),
        pattern!(&space, [{ ?request_id @ exec::kind: &KIND_EXEC_REQUEST_ID }])
    ) {
        state.requests.insert(
            request_id,
            ExecRequestRow {
                id: request_id,
                command: None,
                requested_at: None,
            },
        );
    }

    for (request_id, handle) in find!(
        (request_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?request_id @ exec::command_text: ?handle }])
    ) {
        if let Some(entry) = state.requests.get_mut(&request_id) {
            entry.command = Some(read_text(ws, handle)?);
        }
    }

    for (request_id, requested_at) in find!(
        (request_id: Id, requested_at: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{ ?request_id @ exec::requested_at: ?requested_at }])
    ) {
        if let Some(entry) = state.requests.get_mut(&request_id) {
            entry.requested_at = Some(interval_key(requested_at));
        }
    }

    for (event_id, about_request) in find!(
        (event_id: Id, about_request: Id),
        pattern!(&space, [{
            ?event_id @
            exec::kind: &KIND_EXEC_IN_PROGRESS_ID,
            exec::about_request: ?about_request,
        }])
    ) {
        let mut started_at = None;
        for (id, value) in find!(
            (id: Id, value: Value<valueschemas::NsTAIInterval>),
            pattern!(&space, [{ ?id @ exec::started_at: ?value }])
        ) {
            if id == event_id {
                started_at = Some(interval_key(value));
                break;
            }
        }
        state.in_progress.push(ExecInProgressRow {
            about_request,
            started_at,
        });
    }

    let mut result_map: HashMap<Id, ExecResultRow> = HashMap::new();
    for (result_id, about_request) in find!(
        (result_id: Id, about_request: Id),
        pattern!(&space, [{
            ?result_id @
            exec::kind: &KIND_EXEC_RESULT_ID,
            exec::about_request: ?about_request,
        }])
    ) {
        result_map.insert(
            result_id,
            ExecResultRow {
                id: result_id,
                about_request,
                finished_at: None,
                exit_code: None,
                stderr_text: None,
                error: None,
            },
        );
    }

    for (result_id, value) in find!(
        (result_id: Id, value: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{ ?result_id @ exec::finished_at: ?value }])
    ) {
        if let Some(entry) = result_map.get_mut(&result_id) {
            entry.finished_at = Some(interval_key(value));
        }
    }

    for (result_id, value) in find!(
        (result_id: Id, value: Value<valueschemas::U256BE>),
        pattern!(&space, [{ ?result_id @ exec::exit_code: ?value }])
    ) {
        if let Some(entry) = result_map.get_mut(&result_id) {
            entry.exit_code = u256be_to_u64(value).map(|n| n as i64);
        }
    }

    for (result_id, handle) in find!(
        (result_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?result_id @ exec::stderr_text: ?handle }])
    ) {
        if let Some(entry) = result_map.get_mut(&result_id) {
            entry.stderr_text = Some(read_text(ws, handle)?);
        }
    }

    for (result_id, handle) in find!(
        (result_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?result_id @ exec::error: ?handle }])
    ) {
        if let Some(entry) = result_map.get_mut(&result_id) {
            entry.error = Some(read_text(ws, handle)?);
        }
    }

    state.results = result_map.into_values().collect();
    Ok(state)
}

fn collect_llm_state(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
    space: &TribleSet,
) -> Result<LlmState> {
    let mut state = LlmState::default();

    for (request_id,) in find!(
        (request_id: Id),
        pattern!(&space, [{ ?request_id @ llm::kind: &KIND_LLM_REQUEST_ID }])
    ) {
        state
            .requests
            .insert(request_id, LlmRequestRow { requested_at: None });
    }

    for (request_id, requested_at) in find!(
        (request_id: Id, requested_at: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{ ?request_id @ llm::requested_at: ?requested_at }])
    ) {
        if let Some(entry) = state.requests.get_mut(&request_id) {
            entry.requested_at = Some(interval_key(requested_at));
        }
    }

    for (event_id, about_request) in find!(
        (event_id: Id, about_request: Id),
        pattern!(&space, [{
            ?event_id @
            llm::kind: &KIND_LLM_IN_PROGRESS_ID,
            llm::about_request: ?about_request,
        }])
    ) {
        let mut started_at = None;
        for (id, value) in find!(
            (id: Id, value: Value<valueschemas::NsTAIInterval>),
            pattern!(&space, [{ ?id @ llm::started_at: ?value }])
        ) {
            if id == event_id {
                started_at = Some(interval_key(value));
                break;
            }
        }
        state.in_progress.push(LlmInProgressRow {
            about_request,
            started_at,
        });
    }

    let mut result_map: HashMap<Id, LlmResultRow> = HashMap::new();
    for (result_id, about_request) in find!(
        (result_id: Id, about_request: Id),
        pattern!(&space, [{
            ?result_id @
            llm::kind: &KIND_LLM_RESULT_ID,
            llm::about_request: ?about_request,
        }])
    ) {
        result_map.insert(
            result_id,
            LlmResultRow {
                about_request,
                finished_at: None,
                error: None,
            },
        );
    }

    for (result_id, value) in find!(
        (result_id: Id, value: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{ ?result_id @ llm::finished_at: ?value }])
    ) {
        if let Some(entry) = result_map.get_mut(&result_id) {
            entry.finished_at = Some(interval_key(value));
        }
    }

    for (result_id, handle) in find!(
        (result_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?result_id @ llm::error: ?handle }])
    ) {
        if let Some(entry) = result_map.get_mut(&result_id) {
            entry.error = Some(read_text(ws, handle)?);
        }
    }

    state.results = result_map.into_values().collect();
    Ok(state)
}

fn collect_reason_state(
    ws: &mut Workspace<Pile<valueschemas::Blake3>>,
    space: &TribleSet,
) -> Result<Vec<ReasonEventRow>> {
    let mut rows: HashMap<Id, ReasonEventRow> = HashMap::new();

    for (reason_id,) in find!(
        (reason_id: Id),
        pattern!(&space, [{ ?reason_id @ metadata::tag: &KIND_REASON_EVENT_ID }])
    ) {
        rows.insert(
            reason_id,
            ReasonEventRow {
                created_at: None,
                text: None,
                about_turn: None,
                command_text: None,
            },
        );
    }

    for (reason_id, created_at) in find!(
        (reason_id: Id, created_at: Value<valueschemas::NsTAIInterval>),
        pattern!(&space, [{ ?reason_id @ reason::created_at: ?created_at }])
    ) {
        if let Some(row) = rows.get_mut(&reason_id) {
            row.created_at = Some(interval_key(created_at));
        }
    }

    for (reason_id, handle) in find!(
        (reason_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?reason_id @ reason::text: ?handle }])
    ) {
        if let Some(row) = rows.get_mut(&reason_id) {
            row.text = Some(read_text(ws, handle)?);
        }
    }

    for (reason_id, about_turn) in find!(
        (reason_id: Id, about_turn: Id),
        pattern!(&space, [{ ?reason_id @ reason::about_turn: ?about_turn }])
    ) {
        if let Some(row) = rows.get_mut(&reason_id) {
            row.about_turn = Some(about_turn);
        }
    }

    for (reason_id, handle) in find!(
        (reason_id: Id, handle: TextHandle),
        pattern!(&space, [{ ?reason_id @ reason::command_text: ?handle }])
    ) {
        if let Some(row) = rows.get_mut(&reason_id) {
            row.command_text = Some(read_text(ws, handle)?);
        }
    }

    let mut list: Vec<ReasonEventRow> = rows.into_values().collect();
    list.sort_by_key(|row| row.created_at.unwrap_or(i128::MIN));
    list.reverse();
    Ok(list)
}

fn pending_exec_count(state: &ExecState) -> usize {
    let done: HashSet<Id> = state.results.iter().map(|row| row.about_request).collect();
    let running: HashSet<Id> = state
        .in_progress
        .iter()
        .map(|row| row.about_request)
        .filter(|id| !done.contains(id))
        .collect();
    state
        .requests
        .keys()
        .filter(|id| !done.contains(*id) && !running.contains(*id))
        .count()
}

fn pending_llm_count(state: &LlmState) -> usize {
    let done: HashSet<Id> = state.results.iter().map(|row| row.about_request).collect();
    let running: HashSet<Id> = state
        .in_progress
        .iter()
        .map(|row| row.about_request)
        .filter(|id| !done.contains(id))
        .collect();
    state
        .requests
        .keys()
        .filter(|id| !done.contains(*id) && !running.contains(*id))
        .count()
}

fn stale_exec_in_progress_count(state: &ExecState, now_key: i128, stale_ns: i128) -> usize {
    let done: HashSet<Id> = state.results.iter().map(|row| row.about_request).collect();
    state
        .in_progress
        .iter()
        .filter(|row| !done.contains(&row.about_request))
        .filter_map(|row| row.started_at)
        .filter(|started| now_key.saturating_sub(*started) >= stale_ns)
        .count()
}

fn stale_llm_in_progress_count(state: &LlmState, now_key: i128, stale_ns: i128) -> usize {
    let done: HashSet<Id> = state.results.iter().map(|row| row.about_request).collect();
    state
        .in_progress
        .iter()
        .filter(|row| !done.contains(&row.about_request))
        .filter_map(|row| row.started_at)
        .filter(|started| now_key.saturating_sub(*started) >= stale_ns)
        .count()
}

fn active_exec_running_count(state: &ExecState) -> usize {
    let done: HashSet<Id> = state.results.iter().map(|row| row.about_request).collect();
    state
        .in_progress
        .iter()
        .map(|row| row.about_request)
        .filter(|id| !done.contains(id))
        .collect::<HashSet<_>>()
        .len()
}

fn active_llm_running_count(state: &LlmState) -> usize {
    let done: HashSet<Id> = state.results.iter().map(|row| row.about_request).collect();
    state
        .in_progress
        .iter()
        .map(|row| row.about_request)
        .filter(|id| !done.contains(id))
        .collect::<HashSet<_>>()
        .len()
}

fn collect_exec_attempts(state: &ExecState, recent: usize) -> Vec<ExecAttempt> {
    let mut rows: Vec<ExecAttempt> = state
        .results
        .iter()
        .filter_map(|result| {
            let finished_at = result.finished_at?;
            let request = state.requests.get(&result.about_request)?;
            let command = request.command.clone()?;
            let fingerprint = result
                .error
                .as_ref()
                .map(|s| first_line(s))
                .or_else(|| result.stderr_text.as_ref().map(|s| first_line(s)))
                .unwrap_or_else(|| {
                    if result.exit_code == Some(0) {
                        "<ok>".to_string()
                    } else {
                        "<no stderr text>".to_string()
                    }
                });
            Some(ExecAttempt {
                request_id: request.id,
                result_id: result.id,
                finished_at,
                command,
                exit_code: result.exit_code,
                fingerprint,
            })
        })
        .collect();
    rows.sort_by_key(|row| row.finished_at);
    rows.reverse();
    rows.truncate(recent);
    rows
}

fn build_loop_report(state: &ExecState, recent: usize, min_repeat: usize) -> LoopReport {
    let recent_rows = collect_exec_attempts(state, recent);

    let mut by_pattern: HashMap<(String, Option<i64>, String), PatternSummary> = HashMap::new();
    for row in &recent_rows {
        let key = (row.command.clone(), row.exit_code, row.fingerprint.clone());
        let entry = by_pattern.entry(key).or_insert_with(|| PatternSummary {
            command: row.command.clone(),
            exit_code: row.exit_code,
            fingerprint: row.fingerprint.clone(),
            count: 0,
            latest: row.finished_at,
        });
        entry.count += 1;
        entry.latest = entry.latest.max(row.finished_at);
    }
    let mut top_patterns: Vec<PatternSummary> = by_pattern.into_values().collect();
    top_patterns.sort_by_key(|pat| (pat.count, pat.latest));
    top_patterns.reverse();

    let contiguous_head = recent_rows.first().and_then(|head| {
        let mut count = 0usize;
        for row in &recent_rows {
            if row.command == head.command
                && row.exit_code == head.exit_code
                && row.fingerprint == head.fingerprint
            {
                count += 1;
            } else {
                break;
            }
        }
        (count >= min_repeat).then_some(PatternSummary {
            command: head.command.clone(),
            exit_code: head.exit_code,
            fingerprint: head.fingerprint.clone(),
            count,
            latest: head.finished_at,
        })
    });

    LoopReport {
        recent: recent_rows,
        top_patterns,
        contiguous_head,
    }
}

fn pattern_is_failure(pattern: &PatternSummary) -> bool {
    if pattern.exit_code.unwrap_or(1) != 0 {
        return true;
    }
    let normalized = pattern.fingerprint.trim();
    !normalized.is_empty() && normalized != "<ok>"
}

fn load_relation_terms(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_name: &str,
) -> Result<Vec<String>> {
    let Some(branch_id) = find_branch_by_name(repo.storage_mut(), branch_name)? else {
        return Ok(Vec::new());
    };
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull relations workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout relations workspace: {e:?}"))?;

    let mut terms: HashSet<String> = HashSet::new();
    for (_person_id, handle) in find!(
        (_person_id: Id, handle: TextHandle),
        pattern!(&space, [{
            ?_person_id @
            metadata::tag: &KIND_PERSON_ID,
            metadata::name: ?handle,
        }])
    ) {
        terms.insert(read_text(&mut ws, handle)?);
    }
    for (alias,) in find!(
        (alias: Value<valueschemas::ShortString>),
        pattern!(&space, [{ relations::alias: ?alias }])
    ) {
        let alias: String = alias.from_value();
        terms.insert(alias);
    }

    let mut out: Vec<String> = terms.into_iter().collect();
    out.sort();
    Ok(out)
}

fn find_case_variant(terms: &[String], label: &str) -> Option<String> {
    terms
        .iter()
        .find(|term| term.eq_ignore_ascii_case(label) && *term != label)
        .cloned()
}

fn extract_unknown_person_label(text: &str) -> Option<String> {
    let marker = "unknown person label '";
    let start = text.find(marker)?;
    let rest = &text[start + marker.len()..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

fn count_unread_local_messages(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    local_branch: &str,
    reader_id: Id,
) -> Result<Option<usize>> {
    let Some(branch_id) = find_branch_by_name(repo.storage_mut(), local_branch)? else {
        return Ok(None);
    };
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull local-messages workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout local-messages workspace: {e:?}"))?;

    let mut incoming: HashSet<Id> = HashSet::new();
    for (message_id, to_id) in find!(
        (message_id: Id, to_id: Id),
        pattern!(&space, [{
            ?message_id @
            metadata::tag: &KIND_LOCAL_MESSAGE_ID,
            local::to: ?to_id,
        }])
    ) {
        if to_id == reader_id {
            incoming.insert(message_id);
        }
    }

    let mut read: HashSet<Id> = HashSet::new();
    for (about_message, ack_reader) in find!(
        (about_message: Id, ack_reader: Id),
        pattern!(&space, [{
            _?read_id @
            metadata::tag: &KIND_LOCAL_READ_ID,
            local::about_message: ?about_message,
            local::reader: ?ack_reader,
        }])
    ) {
        if ack_reader == reader_id {
            read.insert(about_message);
        }
    }

    Ok(Some(incoming.difference(&read).count()))
}

fn cmd_scan(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    cli: &Cli,
    recent: usize,
    loop_min: usize,
    stale_min: i64,
) -> Result<()> {
    let config = load_latest_config(repo, cli.config_branch.as_str())?;
    let branch_id = resolve_target_branch(repo, cli, &config)?;
    let mut ws = pull_workspace(repo, branch_id, "pull target workspace")?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout target workspace: {e:?}"))?;
    let exec_state = collect_exec_state(&mut ws, &space)?;
    let llm_state = collect_llm_state(&mut ws, &space)?;

    let now_key = now_epoch().to_tai_duration().total_nanoseconds();
    let stale_ns = (stale_min.max(0) as i128) * 60 * 1_000_000_000;

    let exec_pending = pending_exec_count(&exec_state);
    let llm_pending = pending_llm_count(&llm_state);
    let stale_exec = stale_exec_in_progress_count(&exec_state, now_key, stale_ns);
    let stale_llm = stale_llm_in_progress_count(&llm_state, now_key, stale_ns);
    let exec_running = active_exec_running_count(&exec_state);
    let llm_running = active_llm_running_count(&llm_state);
    let loop_report = build_loop_report(&exec_state, recent, loop_min);

    let unread_local = if let Some(persona_id) = config.as_ref().and_then(|cfg| cfg.persona_id) {
        count_unread_local_messages(repo, cli.local_branch.as_str(), persona_id)?
    } else {
        None
    };

    println!("Triage scan");
    println!("- pile: {}", cli.pile.display());
    println!("- target branch: {branch_id:x}");
    if let Some(cfg) = config.as_ref() {
        if let Some(updated_at) = cfg.updated_at {
            println!("- config age: {}", format_age(now_key, updated_at));
        }
        if let Some(branch_name) = cfg.branch_name.as_ref() {
            println!("- config branch label: {branch_name}");
        }
        if let Some(config_branch_id) = cfg.branch_id {
            println!("- config branch id: {config_branch_id:x}");
        }
        if let Some(persona_id) = cfg.persona_id {
            println!("- persona id: {persona_id:x}");
        }
    } else {
        println!("- config: missing");
    }

    println!();
    println!("Queues");
    println!(
        "- exec: requests={} pending={} running={} results={}",
        exec_state.requests.len(),
        exec_pending,
        exec_running,
        exec_state.results.len()
    );
    println!(
        "- llm:  requests={} pending={} running={} results={}",
        llm_state.requests.len(),
        llm_pending,
        llm_running,
        llm_state.results.len()
    );
    println!("- stale in-progress (>{stale_min}m): exec={stale_exec}, llm={stale_llm}");
    match unread_local {
        Some(count) => println!("- local unread (persona inbox): {count}"),
        None => println!("- local unread (persona inbox): unavailable"),
    }

    println!();
    println!("Loop heuristics");
    let probable_pattern = loop_report.contiguous_head.as_ref().or_else(|| {
        loop_report
            .top_patterns
            .iter()
            .find(|pat| pat.count >= loop_min && pattern_is_failure(pat))
    });
    if let Some(head) = probable_pattern {
        println!(
            "- probable loop: {} repeated {}x (exit={}): {}",
            truncate_single_line(&head.command, 80),
            head.count,
            head.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".to_string()),
            truncate_single_line(&head.fingerprint, 120)
        );
    } else {
        println!("- no contiguous failure loop >= {loop_min} in recent exec results");
    }

    let recent_llm_failures = collect_recent_llm_failures(&llm_state, recent);
    println!();
    println!("Recent LLM failures");
    if recent_llm_failures.is_empty() {
        println!("- none in recent window");
    } else {
        for row in recent_llm_failures {
            let age = row
                .finished_at
                .map(|at| format_age(now_key, at))
                .unwrap_or_else(|| "-".to_string());
            let detail = row
                .error
                .as_deref()
                .map(first_line)
                .unwrap_or_else(|| "<missing error text>".to_string());
            println!("- {age} | {}", truncate_single_line(detail.as_str(), 140));
        }
    }

    println!();
    println!("Suggested next checks");
    if llm_pending > 0 && llm_state.in_progress.is_empty() {
        println!("- LLM worker might be down: pending requests exist without in-progress events.");
    }
    if exec_pending > 0 && exec_state.in_progress.is_empty() {
        println!(
            "- Exec worker might be down: pending command requests exist without in-progress events."
        );
    }
    if stale_exec > 0 || stale_llm > 0 {
        println!("- One or more workers appear stale; inspect service logs and process health.");
    }
    if let Some(head) = probable_pattern {
        if let Some(label) = extract_unknown_person_label(&head.fingerprint) {
            let terms = load_relation_terms(repo, cli.relations_branch.as_str())?;
            if let Some(case_variant) = find_case_variant(&terms, label.as_str()) {
                println!(
                    "- local_messages label mismatch: '{}' failed; try '{}' or add '{}' as alias in relations.",
                    label, case_variant, label
                );
            } else {
                println!(
                    "- local_messages unknown label '{}': add it to relations or use a known label/id.",
                    label
                );
            }
        }
        if head.fingerprint.contains("rust-script")
            && head.fingerprint.contains("No such file or directory")
        {
            println!(
                "- rust-script is missing in the VM: install it (or invoke faculties via `rust-script <file>` fallback)."
            );
        }
        if head.fingerprint.contains("commentary: not found") {
            println!(
                "- command emission includes markdown fence/preamble; harden command extraction to strip wrappers."
            );
        }
    }
    if llm_pending == 0
        && exec_pending == 0
        && stale_exec == 0
        && stale_llm == 0
        && unread_local.unwrap_or(0) == 0
    {
        println!("- system looks healthy; no obvious blockers detected.");
    }

    Ok(())
}

fn collect_recent_llm_failures(state: &LlmState, recent: usize) -> Vec<LlmResultRow> {
    let mut failures: Vec<LlmResultRow> = state
        .results
        .iter()
        .filter(|row| row.error.is_some())
        .cloned()
        .collect();
    failures.sort_by_key(|row| row.finished_at.unwrap_or(i128::MIN));
    failures.reverse();
    failures.into_iter().take(recent.min(5)).collect()
}

fn verify_commit_chain(
    reader: &triblespace::core::repo::pile::PileReader<valueschemas::Blake3>,
    start: Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>>,
) -> ChainReport {
    let head_hash: Value<valueschemas::Hash<valueschemas::Blake3>> =
        valueschemas::Handle::to_hash(start);
    let commit_head: String = head_hash.from_value();

    let mut queue: VecDeque<(
        Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>>,
        usize,
    )> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    queue.push_back((start, 0));

    let mut checked_commits = 0usize;
    while let Some((commit, depth)) = queue.pop_front() {
        let commit_hash_value: Value<valueschemas::Hash<valueschemas::Blake3>> =
            valueschemas::Handle::to_hash(commit);
        let commit_hash: String = commit_hash_value.from_value();
        if !visited.insert(commit_hash.clone()) {
            continue;
        }

        match reader.metadata(commit) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return ChainReport {
                    commit_head,
                    checked_commits,
                    issue: Some(ChainIssue {
                        commit_hash,
                        depth_from_head: depth,
                        reason: "commit blob missing".to_string(),
                    }),
                };
            }
            Err(err) => {
                return ChainReport {
                    commit_head,
                    checked_commits,
                    issue: Some(ChainIssue {
                        commit_hash,
                        depth_from_head: depth,
                        reason: format!("commit metadata error: {err:?}"),
                    }),
                };
            }
        }

        let commit_meta: TribleSet = match reader.get(commit) {
            Ok(meta) => meta,
            Err(err) => {
                return ChainReport {
                    commit_head,
                    checked_commits,
                    issue: Some(ChainIssue {
                        commit_hash,
                        depth_from_head: depth,
                        reason: format!("commit decode failed: {err:?}"),
                    }),
                };
            }
        };

        let mut saw_content_attr = false;
        let mut missing_content_ref = false;
        let mut parents = Vec::new();
        for trible in commit_meta.iter() {
            if trible.a() == &REPO_CONTENT_ATTR {
                saw_content_attr = true;
                let content = *trible
                    .v::<valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>>();
                match reader.metadata(content) {
                    Ok(Some(_)) => {}
                    Ok(None) => missing_content_ref = true,
                    Err(err) => {
                        return ChainReport {
                            commit_head,
                            checked_commits,
                            issue: Some(ChainIssue {
                                commit_hash,
                                depth_from_head: depth,
                                reason: format!("content metadata error: {err:?}"),
                            }),
                        };
                    }
                }
            } else if trible.a() == &REPO_PARENT_ATTR {
                parents.push(
                    *trible.v::<
                        valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>,
                    >(),
                );
            }
        }

        // Commits may intentionally omit repo::content (for example merge-only
        // commits). Only treat missing content as corruption when the commit
        // explicitly references repo::content blobs and one of those blobs is
        // unavailable.
        if saw_content_attr && missing_content_ref {
            return ChainReport {
                commit_head,
                checked_commits,
                issue: Some(ChainIssue {
                    commit_hash,
                    depth_from_head: depth,
                    reason: "referenced content blob missing".to_string(),
                }),
            };
        }

        checked_commits += 1;
        for parent in parents {
            queue.push_back((parent, depth + 1));
        }
    }

    ChainReport {
        commit_head,
        checked_commits,
        issue: None,
    }
}

fn cmd_chain(repo: &mut Repository<Pile<valueschemas::Blake3>>, cli: &Cli) -> Result<()> {
    let config = load_latest_config(repo, cli.config_branch.as_str())?;
    let branch_id = resolve_target_branch(repo, cli, &config)?;
    let Some(branch_meta_handle) = repo
        .storage_mut()
        .head(branch_id)
        .map_err(|err| anyhow!("read branch metadata head: {err:?}"))?
    else {
        bail!("target branch {branch_id:x} has no branch metadata head");
    };

    let reader = repo
        .storage_mut()
        .reader()
        .map_err(|err| anyhow!("open pile reader: {err:?}"))?;
    let branch_meta: TribleSet = reader
        .get(branch_meta_handle)
        .map_err(|err| anyhow!("decode branch metadata: {err:?}"))?;

    let mut branch_name = None;
    let mut commit_head = None;
    for trible in branch_meta.iter() {
        if trible.a() == &metadata::name.id() {
            let handle =
                *trible.v::<valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>>();
            if let Ok(view) = reader.get::<View<str>, _>(handle) {
                branch_name = Some(view.as_ref().to_string());
            }
        } else if trible.a() == &REPO_HEAD_ATTR {
            commit_head = Some(
                *trible
                    .v::<valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>>(),
            );
        }
    }

    let Some(commit_head) = commit_head else {
        bail!("branch metadata for {branch_id:x} has no repo head");
    };

    let report = verify_commit_chain(&reader, commit_head);
    println!("Commit-chain integrity");
    println!("- pile: {}", cli.pile.display());
    println!(
        "- branch: {} ({branch_id:x})",
        branch_name.unwrap_or_else(|| "<unnamed>".to_string())
    );
    println!("- current commit head: {}", report.commit_head);
    match report.issue {
        Some(issue) => {
            println!("- status: broken");
            println!("- issue: {}", issue.reason);
            println!("- issue commit: {}", issue.commit_hash);
            println!("- depth from head: {}", issue.depth_from_head);
            println!(
                "- commits verified before failure: {}",
                report.checked_commits
            );
        }
        None => {
            println!("- status: healthy");
            println!("- commits verified: {}", report.checked_commits);
        }
    }

    Ok(())
}

fn cmd_loops(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    cli: &Cli,
    recent: usize,
    min_repeat: usize,
) -> Result<()> {
    let config = load_latest_config(repo, cli.config_branch.as_str())?;
    let branch_id = resolve_target_branch(repo, cli, &config)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull target workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout target workspace: {e:?}"))?;
    let exec_state = collect_exec_state(&mut ws, &space)?;
    let report = build_loop_report(&exec_state, recent, min_repeat);
    let now_key = now_epoch().to_tai_duration().total_nanoseconds();

    println!("Triage loops");
    println!("- branch: {branch_id:x}");
    println!("- recent attempts: {}", report.recent.len());

    if let Some(head) = report.contiguous_head.as_ref() {
        println!(
            "- contiguous head loop: {}x, exit={}, command={}",
            head.count,
            head.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".to_string()),
            truncate_single_line(&head.command, 90)
        );
    } else {
        println!("- contiguous head loop: none (threshold {min_repeat})");
    }

    println!();
    println!("Top patterns");
    for pattern in report.top_patterns.iter().take(5) {
        println!(
            "- {}x | exit={} | {} | {}",
            pattern.count,
            pattern
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".to_string()),
            truncate_single_line(&pattern.command, 70),
            truncate_single_line(&pattern.fingerprint, 80)
        );
    }

    println!();
    println!("Recent attempts");
    for row in &report.recent {
        println!(
            "- [{}:{}] {} | exit={} | {} | {}",
            id_prefix(row.request_id),
            id_prefix(row.result_id),
            format_age(now_key, row.finished_at),
            row.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".to_string()),
            truncate_single_line(&row.command, 70),
            truncate_single_line(&row.fingerprint, 90)
        );
    }

    Ok(())
}

fn build_timeline_rows(
    exec_state: &ExecState,
    llm_state: &LlmState,
    reason_rows: &[ReasonEventRow],
    recent: usize,
) -> Vec<TimelineRow> {
    let mut rows = Vec::<TimelineRow>::new();

    for request in exec_state.requests.values() {
        if let (Some(at), Some(command)) = (request.requested_at, request.command.as_ref()) {
            rows.push(TimelineRow {
                at,
                source: "exec",
                detail: format!(
                    "[{}] {}",
                    id_prefix(request.id),
                    truncate_single_line(command, 120)
                ),
            });
        }
    }

    let request_commands: HashMap<Id, String> = exec_state
        .requests
        .values()
        .filter_map(|request| request.command.clone().map(|command| (request.id, command)))
        .collect();

    for result in &exec_state.results {
        let at = result.finished_at.unwrap_or(i128::MIN);
        let command = request_commands
            .get(&result.about_request)
            .cloned()
            .unwrap_or_else(|| "<missing command>".to_string());
        let status = if let Some(error) = result.error.as_ref() {
            format!("error {}", truncate_single_line(error, 72))
        } else if let Some(stderr) = result.stderr_text.as_ref() {
            let line = first_line(stderr);
            if line == "<ok>" {
                format!("exit {}", result.exit_code.unwrap_or(0))
            } else {
                format!(
                    "exit {} stderr {}",
                    result.exit_code.unwrap_or(-1),
                    truncate_single_line(line.as_str(), 72)
                )
            }
        } else {
            format!("exit {}", result.exit_code.unwrap_or(-1))
        };
        rows.push(TimelineRow {
            at,
            source: "exec-result",
            detail: format!(
                "[{}:{}] {} | {}",
                id_prefix(result.about_request),
                id_prefix(result.id),
                truncate_single_line(command.as_str(), 100),
                status
            ),
        });
    }

    for request in llm_state.requests.keys() {
        if let Some(entry) = llm_state.requests.get(request) {
            if let Some(at) = entry.requested_at {
                rows.push(TimelineRow {
                    at,
                    source: "llm",
                    detail: format!("[{}] request", id_prefix(*request)),
                });
            }
        }
    }
    for result in &llm_state.results {
        if let Some(error) = result.error.as_ref() {
            rows.push(TimelineRow {
                at: result.finished_at.unwrap_or(i128::MIN),
                source: "llm-error",
                detail: truncate_single_line(error, 130),
            });
        }
    }

    for row in reason_rows {
        let text = row.text.as_deref().unwrap_or("<missing>");
        let mut detail = String::new();
        if let Some(turn_id) = row.about_turn {
            detail.push_str(format!("[turn {}] ", id_prefix(turn_id)).as_str());
        }
        detail.push_str(truncate_single_line(text, 120).as_str());
        if let Some(command) = row.command_text.as_ref() {
            detail.push_str(" | ");
            detail.push_str(truncate_single_line(command, 96).as_str());
        }
        rows.push(TimelineRow {
            at: row.created_at.unwrap_or(i128::MIN),
            source: "reason",
            detail,
        });
    }

    rows.sort_by_key(|row| row.at);
    rows.reverse();
    if rows.len() > recent {
        rows.truncate(recent);
    }
    rows
}

fn cmd_timeline(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    cli: &Cli,
    recent: usize,
) -> Result<()> {
    let config = load_latest_config(repo, cli.config_branch.as_str())?;
    let branch_id = resolve_target_branch(repo, cli, &config)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull target workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout target workspace: {e:?}"))?;
    let exec_state = collect_exec_state(&mut ws, &space)?;
    let llm_state = collect_llm_state(&mut ws, &space)?;
    let reason_state = collect_reason_state(&mut ws, &space)?;
    let rows = build_timeline_rows(&exec_state, &llm_state, &reason_state, recent);
    let now_key = now_epoch().to_tai_duration().total_nanoseconds();

    println!("Triage timeline");
    println!("- pile: {}", cli.pile.display());
    println!("- branch: {branch_id:x}");
    println!("- rows: {}", rows.len());
    println!();
    for row in rows {
        println!(
            "- {:>5} {:>11} | {}",
            format_age(now_key, row.at),
            row.source,
            row.detail
        );
    }
    Ok(())
}

#[derive(Debug)]
struct BranchRepairOutcome {
    name: String,
    canonical_id: Id,
    duplicate_ids: Vec<Id>,
    merged_facts: usize,
    merged_branches: usize,
    deleted_branches: usize,
    skipped_branches: usize,
}

fn insert_canonical_branch(
    map: &mut HashMap<String, Id>,
    name: &str,
    id: Option<Id>,
) -> Result<()> {
    let Some(id) = id else {
        return Ok(());
    };
    match map.get(name).copied() {
        Some(existing) if existing != id => {
            bail!("conflicting canonical ids for branch name '{name}': {existing:x} vs {id:x}");
        }
        _ => {
            map.insert(name.to_owned(), id);
            Ok(())
        }
    }
}

fn delete_branch(repo: &mut Repository<Pile<valueschemas::Blake3>>, branch_id: Id) -> Result<()> {
    let mut expected = repo
        .storage_mut()
        .head(branch_id)
        .map_err(|err| anyhow!("read branch {branch_id:x} head: {err:?}"))?;
    if expected.is_none() {
        return Ok(());
    }

    for _ in 0..3 {
        let result = repo
            .storage_mut()
            .update(branch_id, expected, None)
            .map_err(|err| anyhow!("delete branch {branch_id:x}: {err:?}"))?;
        match result {
            PushResult::Success() => return Ok(()),
            PushResult::Conflict(current) => {
                if current.is_none() {
                    return Ok(());
                }
                expected = current;
            }
        }
    }
    bail!("delete branch {branch_id:x}: conflict after retries")
}

fn repair_named_duplicates(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_name: &str,
    canonical_id: Id,
    dry_run: bool,
) -> Result<BranchRepairOutcome> {
    if !dry_run {
        ensure_branch_with_id(repo, canonical_id, branch_name)?;
    }

    let duplicate_ids = find_branch_ids_by_name(repo.storage_mut(), branch_name)?
        .into_iter()
        .filter(|id| *id != canonical_id)
        .collect::<Vec<_>>();

    let mut outcome = BranchRepairOutcome {
        name: branch_name.to_owned(),
        canonical_id,
        duplicate_ids: duplicate_ids.clone(),
        merged_facts: 0,
        merged_branches: 0,
        deleted_branches: 0,
        skipped_branches: 0,
    };
    if duplicate_ids.is_empty() {
        return Ok(outcome);
    }

    if dry_run {
        let canonical_exists = repo
            .storage_mut()
            .head(canonical_id)
            .map_err(|e| anyhow!("read canonical branch {canonical_id:x} head: {e:?}"))?
            .is_some();
        if !canonical_exists {
            outcome.skipped_branches = outcome.duplicate_ids.len();
            return Ok(outcome);
        }
    }

    let mut canonical_ws = repo
        .pull(canonical_id)
        .map_err(|err| anyhow!("pull canonical branch {canonical_id:x}: {err:?}"))?;
    let mut canonical_data = canonical_ws
        .checkout(..)
        .map_err(|err| anyhow!("checkout canonical branch {canonical_id:x}: {err:?}"))?;

    for duplicate_id in duplicate_ids {
        let mut duplicate_ws = match repo.pull(duplicate_id) {
            Ok(ws) => ws,
            Err(err) => {
                outcome.skipped_branches += 1;
                eprintln!(
                    "warning: skip duplicate branch {duplicate_id:x} ({branch_name}): pull failed ({err:?})"
                );
                continue;
            }
        };
        let duplicate_data = match duplicate_ws.checkout(..) {
            Ok(set) => set,
            Err(err) => {
                outcome.skipped_branches += 1;
                eprintln!(
                    "warning: skip duplicate branch {duplicate_id:x} ({branch_name}): checkout failed ({err:?})"
                );
                continue;
            }
        };

        let delta = duplicate_data.difference(&canonical_data);
        if !delta.is_empty() {
            outcome.merged_facts += delta.len();
            outcome.merged_branches += 1;
            if !dry_run {
                canonical_ws.commit(delta.clone(), None, Some("triage repair branch duplicates"));
                push_workspace(repo, &mut canonical_ws)?;
                canonical_data += delta;
            }
        }

        if !dry_run {
            match delete_branch(repo, duplicate_id) {
                Ok(()) => outcome.deleted_branches += 1,
                Err(err) => {
                    outcome.skipped_branches += 1;
                    eprintln!(
                        "warning: failed to delete duplicate branch {duplicate_id:x} ({branch_name}): {err:#}"
                    );
                }
            }
        }
    }

    Ok(outcome)
}

fn cmd_repair_branch_duplicates(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    cli: &Cli,
    dry_run: bool,
) -> Result<()> {
    let mut canonical = HashMap::<String, Id>::new();
    insert_canonical_branch(
        &mut canonical,
        cli.config_branch.as_str(),
        Some(FIXED_CONFIG_BRANCH_ID),
    )?;

    let config_branch_head = repo
        .storage_mut()
        .head(FIXED_CONFIG_BRANCH_ID)
        .map_err(|e| anyhow!("read fixed config branch head: {e:?}"))?;
    let config_snapshot = if config_branch_head.is_some() {
        load_latest_config_from_branch_id(repo, FIXED_CONFIG_BRANCH_ID)?
    } else {
        load_latest_config(repo, cli.config_branch.as_str())?
    };

    if let Some(cfg) = config_snapshot.as_ref() {
        let core_name = cfg
            .branch_name
            .as_deref()
            .unwrap_or(cli.branch.as_str())
            .to_owned();
        let core_id = match cfg.branch_id {
            Some(id) => Some(id),
            None => find_branch_by_name(repo.storage_mut(), core_name.as_str())?,
        };
        insert_canonical_branch(&mut canonical, core_name.as_str(), core_id)?;
        insert_canonical_branch(&mut canonical, DEFAULT_EXEC_BRANCH, cfg.exec_branch_id)?;
        insert_canonical_branch(
            &mut canonical,
            DEFAULT_COMPASS_BRANCH,
            cfg.compass_branch_id,
        )?;
        insert_canonical_branch(
            &mut canonical,
            DEFAULT_LOCAL_BRANCH,
            cfg.local_messages_branch_id,
        )?;
        insert_canonical_branch(
            &mut canonical,
            DEFAULT_RELATIONS_BRANCH,
            cfg.relations_branch_id,
        )?;
        insert_canonical_branch(&mut canonical, DEFAULT_TEAMS_BRANCH, cfg.teams_branch_id)?;
        insert_canonical_branch(
            &mut canonical,
            DEFAULT_WORKSPACE_BRANCH,
            cfg.workspace_branch_id,
        )?;
        insert_canonical_branch(
            &mut canonical,
            DEFAULT_ARCHIVE_BRANCH,
            cfg.archive_branch_id,
        )?;
        insert_canonical_branch(&mut canonical, DEFAULT_WEB_BRANCH, cfg.web_branch_id)?;
        insert_canonical_branch(&mut canonical, DEFAULT_MEDIA_BRANCH, cfg.media_branch_id)?;
    }

    let mut names: Vec<String> = canonical.keys().cloned().collect();
    names.sort();

    println!(
        "Repair duplicate named branches{}",
        if dry_run { " (dry-run)" } else { "" }
    );
    println!("- pile: {}", cli.pile.display());
    println!("- canonical branch entries: {}", names.len());

    let mut outcomes = Vec::new();
    for name in names {
        let canonical_id = canonical[&name];
        let outcome = repair_named_duplicates(repo, name.as_str(), canonical_id, dry_run)?;
        outcomes.push(outcome);
    }

    let touched = outcomes
        .iter()
        .filter(|o| !o.duplicate_ids.is_empty())
        .count();
    let merged_branches: usize = outcomes.iter().map(|o| o.merged_branches).sum();
    let merged_facts: usize = outcomes.iter().map(|o| o.merged_facts).sum();
    let deleted: usize = outcomes.iter().map(|o| o.deleted_branches).sum();
    let skipped: usize = outcomes.iter().map(|o| o.skipped_branches).sum();

    println!();
    println!("Summary");
    println!("- names with duplicates: {touched}");
    println!("- merged duplicate branches: {merged_branches}");
    println!("- merged facts: {merged_facts}");
    if dry_run {
        println!(
            "- branches to delete: {}",
            outcomes
                .iter()
                .map(|o| o.duplicate_ids.len())
                .sum::<usize>()
        );
    } else {
        println!("- deleted duplicate branches: {deleted}");
    }
    if skipped > 0 {
        println!("- skipped branches: {skipped}");
    }

    println!();
    println!("Details");
    for outcome in outcomes {
        if outcome.duplicate_ids.is_empty() {
            continue;
        }
        let duplicate_list = outcome
            .duplicate_ids
            .iter()
            .map(|id| format!("{id:x}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "- {} -> {:x} | duplicates: [{}] | merged_facts={} | {}",
            outcome.name,
            outcome.canonical_id,
            duplicate_list,
            outcome.merged_facts,
            if dry_run {
                "would_delete=true".to_string()
            } else {
                format!("deleted={}", outcome.deleted_branches)
            }
        );
    }

    Ok(())
}

fn emit_schema_to_atlas(pile_path: &Path, atlas_branch: &str) -> Result<()> {
    let mut repo = open_repo(pile_path)?;
    let res = (|| -> Result<(), anyhow::Error> {
        let branch_id = ensure_branch(&mut repo, atlas_branch)?;

        let mut metadata_set = TribleSet::new();
        metadata_set +=
            <valueschemas::GenId as metadata::ConstDescribe>::describe(repo.storage_mut())?;
        metadata_set +=
            <valueschemas::NsTAIInterval as metadata::ConstDescribe>::describe(repo.storage_mut())?;
        metadata_set +=
            <valueschemas::U256BE as metadata::ConstDescribe>::describe(repo.storage_mut())?;
        metadata_set +=
            <valueschemas::ShortString as metadata::ConstDescribe>::describe(repo.storage_mut())?;
        metadata_set += <valueschemas::Handle<
            valueschemas::Blake3,
            blobschemas::LongString,
        > as metadata::ConstDescribe>::describe(repo.storage_mut())?;
        metadata_set +=
            <blobschemas::LongString as metadata::ConstDescribe>::describe(repo.storage_mut())?;

        metadata_set += metadata::Describe::describe(&config::kind, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&config::updated_at, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&config::branch, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&config::branch_id, repo.storage_mut())?;
        metadata_set +=
            metadata::Describe::describe(&config::compass_branch_id, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&config::exec_branch_id, repo.storage_mut())?;
        metadata_set +=
            metadata::Describe::describe(&config::local_messages_branch_id, repo.storage_mut())?;
        metadata_set +=
            metadata::Describe::describe(&config::relations_branch_id, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&config::teams_branch_id, repo.storage_mut())?;
        metadata_set +=
            metadata::Describe::describe(&config::workspace_branch_id, repo.storage_mut())?;
        metadata_set +=
            metadata::Describe::describe(&config::archive_branch_id, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&config::web_branch_id, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&config::media_branch_id, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&config::persona_id, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&exec::kind, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&exec::command_text, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&exec::requested_at, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&exec::about_request, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&exec::started_at, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&exec::finished_at, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&exec::exit_code, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&exec::stderr_text, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&exec::error, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&llm::kind, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&llm::about_request, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&llm::requested_at, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&llm::started_at, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&llm::finished_at, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&llm::error, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&local::to, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&local::created_at, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&local::about_message, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&local::reader, repo.storage_mut())?;
        metadata_set += metadata::Describe::describe(&relations::alias, repo.storage_mut())?;

        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull atlas workspace: {e:?}"))?;
        let current = ws
            .checkout(..)
            .map_err(|e| anyhow!("checkout atlas workspace: {e:?}"))?;
        let delta = metadata_set.difference(&current);
        if !delta.is_empty() {
            ws.commit(delta, None, Some("atlas schema metadata"));
            repo.push(&mut ws)
                .map_err(|e| anyhow!("push atlas metadata: {e:?}"))?;
        }

        Ok(())
    })();

    let close_res = repo.close().map_err(|e| anyhow!("close pile: {e:?}"));
    res.and(close_res)?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Err(err) = emit_schema_to_atlas(&cli.pile, cli.atlas_branch.as_str()) {
        eprintln!("atlas emit: {err}");
    }

    let Some(command) = cli.command.as_ref() else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };

    let mut repo = open_repo(&cli.pile)?;
    let command_result = match command {
        Command::Scan {
            recent,
            loop_min,
            stale_min,
        } => cmd_scan(&mut repo, &cli, *recent, *loop_min, *stale_min),
        Command::Loops { recent, min_repeat } => cmd_loops(&mut repo, &cli, *recent, *min_repeat),
        Command::Timeline { recent } => cmd_timeline(&mut repo, &cli, *recent),
        Command::Chain => cmd_chain(&mut repo, &cli),
        Command::Repair { command } => match command {
            RepairCommand::BranchDuplicates { dry_run } => {
                cmd_repair_branch_duplicates(&mut repo, &cli, *dry_run)
            }
        },
    };
    let close_result = repo.close().map_err(|e| anyhow!("close pile: {e:?}"));
    command_result?;
    close_result?;
    Ok(())
}
