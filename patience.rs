#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4.2.3"
//! humantime = "2.3.0"
//! rand_core = "0.6.4"
//! triblespace = "0.16.0"
//! ```

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use humantime::parse_duration;
use rand_core::OsRng;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use triblespace::core::metadata;
use triblespace::core::repo::branch as branch_proto;
use triblespace::core::repo::{PushResult, Repository};
use triblespace::macros::{attributes, id_hex};
use triblespace::prelude::*;

const DEFAULT_BRANCH: &str = "cognition";
const FIXED_CONFIG_BRANCH_ID: Id = id_hex!("4790808CF044F979FC7C2E47FCCB4A64");
const CONFIG_KIND_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");
const KIND_TIMEOUT_EXTENSION_ID: Id = id_hex!("75BC66A1C39131B9A0975613AC9B59FD");

mod exec_schema {
    use super::*;

    attributes! {
        "AA2F34973589295FA70B538D92CD30F8" as kind: valueschemas::GenId;
        "C4C3870642CAB5F55E7E575B1A62E640" as about_request: valueschemas::GenId;
        "442A275ABC6834231FC65A4B89773ECD" as worker: valueschemas::GenId;
        "7FFF32386EBB2AE92094B7D88DE2743D" as timeout_ms: valueschemas::U256BE;
        "AAD2627FB70DC16F6ADF8869AE1B203F" as requested_at: valueschemas::NsTAIInterval;
    }
}

mod config_schema {
    use super::*;

    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: valueschemas::GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: valueschemas::NsTAIInterval;
        "4E2F9CA7A8456DED8C43A3BE741ADA58" as branch_id: valueschemas::GenId;
        "C188E12ABBDD83D283A23DBAD4B784AF" as exec_branch_id: valueschemas::GenId;
    }
}

#[derive(Parser)]
#[command(
    name = "patience",
    about = "Extend the active turn timeout and optionally run a command"
)]
struct Cli {
    /// Path to the pile file.
    #[arg(long, global = true)]
    pile: Option<PathBuf>,
    /// Config branch id (hex). Defaults to $CONFIG_BRANCH_ID or fixed config branch id.
    #[arg(long, global = true)]
    config_branch_id: Option<String>,
    /// Target branch name for timeout extension events.
    #[arg(long, default_value = DEFAULT_BRANCH, global = true)]
    branch: String,
    /// Target branch id for timeout extension events (hex). Overrides config.
    #[arg(long, global = true)]
    branch_id: Option<String>,
    /// Turn id to annotate (hex). Defaults to $TURN_ID.
    #[arg(long, global = true)]
    turn_id: Option<String>,
    /// Worker id to annotate (hex). Defaults to $WORKER_ID.
    #[arg(long, global = true)]
    worker_id: Option<String>,
    /// Timeout extension duration (e.g. 5m, 90s, 1h).
    #[arg(value_name = "DURATION")]
    duration: Option<String>,
    /// Optional command to run after extending timeout (pass after `--`).
    #[arg(
        value_name = "COMMAND",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        last = true
    )]
    command: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct ConfigSnapshot {
    branch_id: Option<Id>,
    exec_branch_id: Option<Id>,
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

fn id_prefix(id: Id) -> String {
    let hex = format!("{id:x}");
    hex[..8].to_string()
}

fn resolve_pile_path(cli: &Cli) -> PathBuf {
    cli.pile
        .clone()
        .or_else(|| std::env::var("PILE").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("self.pile"))
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

fn parse_timeout_ms(raw: &str) -> Result<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("duration is empty");
    }
    if let Ok(ms) = trimmed.parse::<u64>() {
        return Ok(ms);
    }
    let duration = parse_duration(trimmed).with_context(|| format!("invalid duration '{trimmed}'"))?;
    let millis = duration.as_millis();
    if millis == 0 {
        bail!("duration must be greater than zero");
    }
    if millis > u128::from(u64::MAX) {
        bail!("duration exceeds maximum supported timeout");
    }
    Ok(millis as u64)
}

fn open_repo(path: &Path) -> Result<Repository<Pile<valueschemas::Blake3>>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile directory {}: {e}", parent.display()))?;
    }

    let mut pile = Pile::<valueschemas::Blake3>::open(path)
        .map_err(|e| anyhow!("open pile {}: {e:?}", path.display()))?;
    if let Err(err) = pile.restore() {
        let _ = pile.close();
        return Err(anyhow!("restore pile {}: {err:?}", path.display()));
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
    let close_res = repo.close().map_err(|e| anyhow!("close pile: {e:?}"));
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
        .map_err(|e| anyhow!("branch head {branch_name}: {e:?}"))?
        .is_some()
    {
        return Ok(());
    }

    let name_blob = branch_name.to_owned().to_blob();
    let name_handle = name_blob.get_handle::<valueschemas::Blake3>();
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

fn find_branch_by_name(
    pile: &mut Pile<valueschemas::Blake3>,
    branch_name: &str,
) -> Result<Option<Id>> {
    let name_handle = branch_name
        .to_owned()
        .to_blob()
        .get_handle::<valueschemas::Blake3>();
    let reader = pile.reader().map_err(|e| anyhow!("pile reader: {e:?}"))?;
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
            (
                handle: Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>>
            ),
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

fn load_config_snapshot(repo: &mut Repository<Pile<valueschemas::Blake3>>, branch_id: Id) -> Result<ConfigSnapshot> {
    let Some(_head) = repo
        .storage_mut()
        .head(branch_id)
        .map_err(|e| anyhow!("config branch head: {e:?}"))?
    else {
        return Ok(ConfigSnapshot::default());
    };

    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull config workspace: {e:?}"))?;
    let space = ws.checkout(..).context("checkout config workspace")?;

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
        if latest.is_none_or(|(_, current)| key > current) {
            latest = Some((config_id, key));
        }
    }
    let Some((config_id, _)) = latest else {
        return Ok(ConfigSnapshot::default());
    };

    let branch_id = find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config_schema::branch_id: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == config_id).then_some(value.from_value()));
    let exec_branch_id = find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config_schema::exec_branch_id: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == config_id).then_some(value.from_value()));

    Ok(ConfigSnapshot {
        branch_id,
        exec_branch_id,
    })
}

fn resolve_target_branch_id(
    repo: &mut Repository<Pile<valueschemas::Blake3>>,
    branch_name: &str,
    config_branch_id: Id,
    explicit_branch_id: Option<Id>,
) -> Result<Id> {
    if let Some(id) = explicit_branch_id {
        return Ok(id);
    }

    let snapshot = load_config_snapshot(repo, config_branch_id)?;
    if let Some(id) = snapshot.exec_branch_id.or(snapshot.branch_id) {
        return Ok(id);
    }

    if let Some(id) = find_branch_by_name(repo.storage_mut(), branch_name)? {
        return Ok(id);
    }

    Ok(*ufoid())
}

fn append_timeout_extension(
    pile: &Path,
    branch_name: &str,
    config_branch_id: Id,
    explicit_branch_id: Option<Id>,
    request_id: Id,
    worker_id: Id,
    timeout_ms: u64,
) -> Result<Id> {
    with_repo(pile, |repo| {
        let branch_id =
            resolve_target_branch_id(repo, branch_name, config_branch_id, explicit_branch_id)?;
        ensure_branch_with_id(repo, branch_id, branch_name)?;

        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull workspace: {e:?}"))?;

        let event_id = ufoid();
        let now = epoch_interval(now_epoch());
        let change = entity! { &event_id @
            exec_schema::kind: KIND_TIMEOUT_EXTENSION_ID,
            exec_schema::about_request: request_id,
            exec_schema::worker: worker_id,
            exec_schema::timeout_ms: timeout_ms,
            exec_schema::requested_at: now,
        };
        ws.commit(change, None, Some("playground_exec timeout_extension"));
        repo.push(&mut ws)
            .map_err(|e| anyhow!("push timeout extension: {e:?}"))?;
        Ok(*event_id)
    })
}

fn shell_quote(word: &str) -> String {
    if word
        .chars()
        .all(|ch| {
            ch.is_ascii_alphanumeric() || std::matches!(ch, '_' | '-' | '.' | '/' | ':' | '=')
        })
    {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

fn render_command(command: &[String]) -> String {
    command
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_command(command: &[String]) -> Result<i32> {
    let Some(bin) = command.first() else {
        bail!("missing command");
    };
    let status = ProcessCommand::new(bin)
        .args(command.iter().skip(1))
        .status()
        .with_context(|| format!("run command `{}`", render_command(command)))?;
    Ok(status.code().unwrap_or(1))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let pile_path = resolve_pile_path(&cli);
    let Some(duration_raw) = cli.duration.as_ref() else {
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    };

    let timeout_ms = parse_timeout_ms(duration_raw)?;
    let env_config_branch_id = std::env::var("CONFIG_BRANCH_ID").ok();
    let env_turn_id = std::env::var("TURN_ID").ok();
    let env_worker_id = std::env::var("WORKER_ID").ok();
    let env_branch_id = std::env::var("TRIBLESPACE_BRANCH_ID").ok();

    let config_branch_id = parse_optional_hex_id(
        cli.config_branch_id
            .as_deref()
            .or(env_config_branch_id.as_deref()),
        "config branch id",
    )?
    .unwrap_or(FIXED_CONFIG_BRANCH_ID);
    let explicit_branch_id =
        parse_optional_hex_id(cli.branch_id.as_deref().or(env_branch_id.as_deref()), "branch id")?;
    let request_id = parse_optional_hex_id(
        cli.turn_id.as_deref().or(env_turn_id.as_deref()),
        "turn id",
    )?
    .ok_or_else(|| anyhow!("missing turn id (pass --turn-id or set TURN_ID)"))?;
    let worker_id = parse_optional_hex_id(
        cli.worker_id.as_deref().or(env_worker_id.as_deref()),
        "worker id",
    )?
    .ok_or_else(|| anyhow!("missing worker id (pass --worker-id or set WORKER_ID)"))?;

    let event_id = append_timeout_extension(
        &pile_path,
        &cli.branch,
        config_branch_id,
        explicit_branch_id,
        request_id,
        worker_id,
        timeout_ms,
    )?;

    eprintln!(
        "[{}] timeout extended by {} ms",
        id_prefix(event_id),
        timeout_ms
    );

    if cli.command.is_empty() {
        return Ok(());
    }

    let exit_code = run_command(&cli.command)?;
    std::process::exit(exit_code);
}
