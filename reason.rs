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

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use triblespace::core::metadata;
use triblespace::core::repo::branch as branch_proto;
use triblespace::core::repo::{PushResult, Repository};
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::*;

const DEFAULT_BRANCH: &str = "cognition";
const FIXED_CONFIG_BRANCH_ID: Id = id_hex!("4790808CF044F979FC7C2E47FCCB4A64");
const CONFIG_KIND_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");

const KIND_REASON_ID: Id = id_hex!("9D43BB36D8B4A6275CAF38A1D5DACF36");

type TextHandle = Value<valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>>;

mod reason_schema {
    use super::*;

    attributes! {
        "B10329D5D1087D15A3DAFF7A7CC50696" as text: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "FBA9BC32A457C7BFFDB7E0181D3E82A4" as created_at: valueschemas::NsTAIInterval;
        "E6B1C728F1AE9F46CAB4DBB60D1A9528" as about_turn: valueschemas::GenId;
        "721DED6DA776F2CF4FB91C54D9F82358" as worker: valueschemas::GenId;
        "514F4FE9F560FB155450462C8CF50749" as command_text: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
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
    name = "reason",
    about = "Record explicit reasoning notes linked to the current execution turn"
)]
struct Cli {
    /// Path to the pile file.
    #[arg(long, global = true)]
    pile: Option<PathBuf>,
    /// Config branch id (hex). Defaults to $CONFIG_BRANCH_ID or fixed config branch id.
    #[arg(long, global = true)]
    config_branch_id: Option<String>,
    /// Target branch name for reason events.
    #[arg(long, default_value = DEFAULT_BRANCH, global = true)]
    branch: String,
    /// Target branch id for reason events (hex). Overrides config.
    #[arg(long, global = true)]
    branch_id: Option<String>,
    /// Turn id to annotate (hex). Defaults to $TURN_ID.
    #[arg(long, global = true)]
    turn_id: Option<String>,
    /// Worker id to annotate (hex). Defaults to $WORKER_ID.
    #[arg(long, global = true)]
    worker_id: Option<String>,
    /// Free-form reasoning text.
    #[arg(value_name = "TEXT", help = "Free-form reasoning text. Use @path for file input or @- for stdin.")]
    text: Option<String>,
    /// Optional command to run after logging the reason (pass after `--`).
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

fn append_reason(
    pile: &Path,
    branch_name: &str,
    config_branch_id: Id,
    explicit_branch_id: Option<Id>,
    turn_id: Option<Id>,
    worker_id: Option<Id>,
    text: &str,
    command_text: Option<&str>,
) -> Result<Id> {
    with_repo(pile, |repo| {
        let branch_id =
            resolve_target_branch_id(repo, branch_name, config_branch_id, explicit_branch_id)?;
        ensure_branch_with_id(repo, branch_id, branch_name)?;

        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull workspace: {e:?}"))?;

        let reason_id = ufoid();
        let now = epoch_interval(now_epoch());
        let text_handle = ws.put(text.to_string());
        let command_handle = command_text.map(|cmd| ws.put(cmd.to_string()));
        let change = entity! { &reason_id @
            metadata::tag: &KIND_REASON_ID,
            reason_schema::text: text_handle,
            reason_schema::created_at: now,
            reason_schema::about_turn?: turn_id,
            reason_schema::worker?: worker_id,
            reason_schema::command_text?: command_handle,
        };
        ws.commit(change, None, Some("reason"));
        repo.push(&mut ws)
            .map_err(|e| anyhow!("push reason: {e:?}"))?;
        Ok(*reason_id)
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

    let Some(text_raw) = cli.text.as_ref() else {
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    };
    let text = load_value_or_file(text_raw, "reason text")?;

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
    let turn_id = parse_optional_hex_id(cli.turn_id.as_deref().or(env_turn_id.as_deref()), "turn id")?;
    let worker_id =
        parse_optional_hex_id(cli.worker_id.as_deref().or(env_worker_id.as_deref()), "worker id")?;

    if text.trim().is_empty() {
        bail!("reason text is empty");
    }

    if cli.command.is_empty() {
        let reason_id = append_reason(
            &pile_path,
            &cli.branch,
            config_branch_id,
            explicit_branch_id,
            turn_id,
            worker_id,
            &text,
            None,
        )?;
        println!("reason_id: {reason_id:x}");
        return Ok(());
    }

    let command_text = render_command(&cli.command);
    let reason_id = append_reason(
        &pile_path,
        &cli.branch,
        config_branch_id,
        explicit_branch_id,
        turn_id,
        worker_id,
        &text,
        None,
    )?;
    let action_event_id = append_reason(
        &pile_path,
        &cli.branch,
        config_branch_id,
        explicit_branch_id,
        turn_id,
        worker_id,
        command_text.as_str(),
        Some(command_text.as_str()),
    )?;
    eprintln!("reason_id: {reason_id:x}");
    eprintln!("reason_action_id: {action_event_id:x}");
    let exit_code = run_command(&cli.command)?;
    std::process::exit(exit_code);
}
