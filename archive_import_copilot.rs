#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4"
//! rand_core = "0.6.4"
//! serde_json = "1"
//! triblespace = "0.16.0"
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser};
use hifitime::Epoch;
use serde_json::{Map, Value as JsonValue};
use triblespace::core::import::json_tree::JsonTreeImporter;
use triblespace::prelude::*;

#[path = "archive_common.rs"]
mod common;

#[derive(Parser)]
#[command(
    name = "archive-import-copilot",
    about = "Import Copilot chat exports into TribleSpace"
)]
struct Cli {
    /// Path to the pile file to write into.
    #[arg(long, global = true)]
    pile: Option<PathBuf>,
    /// Branch name to write into (created if missing).
    #[arg(long, default_value = "archive", global = true)]
    branch: String,
    /// Branch id to write into (hex). Overrides config/env branch id.
    #[arg(long, global = true)]
    branch_id: Option<String>,
    /// Import path shortcut.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
}

#[derive(Debug, Default, Clone)]
struct ImportStats {
    files: usize,
    conversations: usize,
    messages: usize,
    commits: usize,
}

#[derive(Debug, Clone)]
struct MessageRecord {
    source_message_id: String,
    role: String,
    author: String,
    content: String,
    created_at: Option<Epoch>,
    order: usize,
}

fn import_copilot_path(
    path: &std::path::Path,
    repo: &mut common::Repo,
    branch_id: Id,
) -> Result<ImportStats> {
    if path.is_dir() {
        let mut files = Vec::new();
        collect_copilot_files(path, &mut files)
            .with_context(|| format!("scan {}", path.display()))?;
        files.sort();
        let mut total = ImportStats::default();
        for file in files {
            let stats = import_copilot_file(&file, repo, branch_id)
                .with_context(|| format!("import {}", file.display()))?;
            total.files += stats.files;
            total.conversations += stats.conversations;
            total.messages += stats.messages;
            total.commits += stats.commits;
        }
        return Ok(total);
    }
    import_copilot_file(path, repo, branch_id)
}

fn import_copilot_file(
    path: &std::path::Path,
    repo: &mut common::Repo,
    branch_id: Id,
) -> Result<ImportStats> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let root: JsonValue = serde_json::from_str(&raw).context("parse copilot json")?;
    let object = root
        .as_object()
        .ok_or_else(|| anyhow!("copilot export must be a JSON object"))?;
    let mut records = parse_copilot_records(object)?;

    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
    let mut catalog = ws.checkout(..).context("checkout workspace")?;

    let mut stats = ImportStats {
        files: 1,
        ..ImportStats::default()
    };

    let json_tree_metadata =
        triblespace::core::import::json_tree::build_json_tree_metadata(repo.storage_mut())
            .map_err(|e| anyhow!("build json tree metadata: {e:?}"))?
            .into_facts();

    let raw_root = {
        let mut importer = JsonTreeImporter::<_, triblespace::prelude::valueschemas::Blake3>::new(
            repo.storage_mut(),
            None,
        );
        let fragment = importer
            .import_str(&raw)
            .context("import copilot raw json tree")?;
        let root = fragment
            .root()
            .ok_or_else(|| anyhow!("json tree importer did not return a single root"))?;
        let delta = fragment.facts().difference(&catalog);
        if !delta.is_empty() {
            ws.commit(
                delta.clone(),
                Some(json_tree_metadata.clone()),
                Some("import copilot json tree"),
            );
            common::push_workspace(repo, &mut ws).context("push copilot json tree")?;
            catalog += delta;
            stats.commits += 1;
        }
        root
    };

    let conversation_id = resolve_copilot_conversation_id(object, raw_root);

    records.sort_by_key(|m| m.order);

    let source_path = path.to_string_lossy().to_string();
    let source_path_handle = ws.put(source_path.clone());
    let batch_fragment = entity! { _ @
        common::import_schema::kind: common::import_schema::kind_batch,
        common::import_schema::source_format: "copilot",
        common::import_schema::source_conversation_id: ws.put(conversation_id.clone()),
    };
    let batch_id = batch_fragment
        .root()
        .expect("entity! must export a single root id");
    let batch_entity = batch_id
        .aquire()
        .expect("entity! root ids should be acquired in current thread");
    let mut change = TribleSet::new();

    change += batch_fragment;
    change += entity! { &batch_entity @
        common::import_schema::source_path: source_path_handle,
        common::import_schema::source_raw_root: raw_root,
    };

    let mut author_cache: HashMap<String, Id> = HashMap::new();
    let mut previous: Option<(Id, String)> = None;
    for message in records {
        let source_message_id_handle = ws.put(message.source_message_id.clone());
        let message_fragment = entity! { _ @
            common::import_schema::batch: batch_id,
            common::import_schema::source_message_id: source_message_id_handle,
        };
        let message_id = message_fragment
            .root()
            .expect("entity! must export a single root id");
        let message_entity = message_id
            .aquire()
            .expect("entity! root ids should be acquired in current thread");
        change += message_fragment;

        let author_key = format!("{}::{}", message.author, message.role);
        let author_id = if let Some(id) = author_cache.get(&author_key).copied() {
            id
        } else {
            let (id, author_change) =
                common::ensure_author(&mut ws, &catalog, &message.author, &message.role)?;
            change += author_change;
            author_cache.insert(author_key, id);
            id
        };

        let created_at =
            common::epoch_interval(message.created_at.unwrap_or_else(common::unknown_epoch));
        let reply_to = previous.as_ref().map(|(parent_id, _)| *parent_id);
        let source_parent_id = previous
            .as_ref()
            .map(|(_, parent_source_id)| ws.put(parent_source_id.clone()));
        let content_handle = ws.put(message.content.clone());
        change += entity! { &message_entity @
            common::archive::kind: common::archive::kind_message,
            common::archive::author: author_id,
            common::archive::content: content_handle,
            common::archive::created_at: created_at,
            common::import_schema::source_author: ws.put(message.author.clone()),
            common::import_schema::source_role: ws.put(message.role.clone()),
            common::import_schema::source_created_at: created_at,
            common::archive::reply_to?: reply_to,
            common::import_schema::source_parent_id?: source_parent_id,
        };
        previous = Some((message_id, message.source_message_id.clone()));
        stats.messages += 1;
    }

    let delta = change.difference(&catalog);
    if !delta.is_empty() {
        ws.commit(delta, None, Some("import copilot"));
        common::push_workspace(repo, &mut ws).context("push copilot import")?;
        stats.commits += 1;
    }
    if stats.messages > 0 {
        stats.conversations = 1;
    }

    Ok(stats)
}

fn collect_copilot_files(path: &std::path::Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("read dir {}", path.display()))? {
        let entry = entry.context("read dir entry")?;
        let file_type = entry.file_type().context("entry type")?;
        let entry_path = entry.path();
        if file_type.is_dir() {
            collect_copilot_files(&entry_path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if entry_path.extension().and_then(|s| s.to_str()) == Some("json") {
            out.push(entry_path);
        }
    }
    Ok(())
}

fn parse_copilot_records(object: &Map<String, JsonValue>) -> Result<Vec<MessageRecord>> {
    if let Some(requests) = object.get("requests").and_then(JsonValue::as_array) {
        return Ok(parse_copilot_requests(requests));
    }
    if let Some(messages) = object.get("messages").and_then(JsonValue::as_array) {
        return Ok(parse_copilot_messages(messages));
    }
    Err(anyhow!("copilot export missing requests[] or messages[]"))
}

fn parse_copilot_requests(requests: &[JsonValue]) -> Vec<MessageRecord> {
    let mut records = Vec::new();
    for (idx, request) in requests.iter().enumerate() {
        let Some(req_obj) = request.as_object() else {
            continue;
        };
        let request_id = req_obj
            .get("requestId")
            .and_then(JsonValue::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("request-{idx:08}"));
        let created_at = req_obj.get("timestamp").and_then(parse_epoch_value);

        if let Some(user_text) = extract_copilot_user_text(req_obj) {
            records.push(MessageRecord {
                source_message_id: format!("{request_id}:user"),
                role: "user".to_string(),
                author: "user".to_string(),
                content: user_text,
                created_at,
                order: idx * 2,
            });
        }

        if let Some(assistant_text) = extract_copilot_assistant_text(req_obj) {
            records.push(MessageRecord {
                source_message_id: format!("{request_id}:assistant"),
                role: "assistant".to_string(),
                author: "assistant".to_string(),
                content: assistant_text,
                created_at,
                order: idx * 2 + 1,
            });
        }
    }
    records
}

fn parse_copilot_messages(messages: &[JsonValue]) -> Vec<MessageRecord> {
    let mut records = Vec::new();
    for (idx, message) in messages.iter().enumerate() {
        let Some(msg_obj) = message.as_object() else {
            continue;
        };
        let role = msg_obj
            .get("role")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("assistant")
            .to_string();
        let Some(content) = extract_message_content(msg_obj) else {
            continue;
        };
        let source_message_id = msg_obj
            .get("id")
            .and_then(JsonValue::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("message-{idx:08}"));
        let created_at = msg_obj
            .get("createdAt")
            .or_else(|| msg_obj.get("created_at"))
            .or_else(|| msg_obj.get("timestamp"))
            .and_then(parse_epoch_value);
        records.push(MessageRecord {
            source_message_id,
            author: canonical_author_name(&role).to_string(),
            role,
            content,
            created_at,
            order: idx,
        });
    }
    records
}

fn resolve_copilot_conversation_id(object: &Map<String, JsonValue>, raw_root: Id) -> String {
    for key in ["conversationId", "threadID", "threadUrl", "threadName"] {
        if let Some(value) = object.get(key).and_then(JsonValue::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    format!("copilot:{raw_root:x}")
}

fn extract_message_content(message: &Map<String, JsonValue>) -> Option<String> {
    if let Some(text) = message.get("content").and_then(JsonValue::as_str) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(parts) = message
        .get("content")
        .and_then(JsonValue::as_object)
        .and_then(|obj| obj.get("parts"))
        .and_then(JsonValue::as_array)
    {
        let mut out = Vec::new();
        for part in parts {
            if let Some(text) = part.as_str() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
        }
        if !out.is_empty() {
            return Some(out.join("\n\n"));
        }
    }
    None
}

fn canonical_author_name(role: &str) -> &str {
    match role {
        "user" => "user",
        "assistant" | "agent" | "model" => "assistant",
        "developer" => "developer",
        "system" => "system",
        _ => role,
    }
}

fn parse_epoch_value(value: &JsonValue) -> Option<Epoch> {
    if let Some(seconds) = json_f64(value) {
        return parse_epoch_number(seconds);
    }
    value.as_str().and_then(parse_epoch_str)
}

fn parse_epoch_str(value: &str) -> Option<Epoch> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(epoch) = trimmed.parse::<Epoch>() {
        return Some(epoch);
    }
    if let Ok(seconds) = trimmed.parse::<f64>() {
        return parse_epoch_number(seconds);
    }
    None
}

fn extract_copilot_user_text(req: &Map<String, JsonValue>) -> Option<String> {
    let message = req.get("message")?.as_object()?;
    if let Some(text) = message.get("text").and_then(JsonValue::as_str) {
        if !text.trim().is_empty() {
            return Some(text.to_string());
        }
    }
    if let Some(parts) = message.get("parts").and_then(JsonValue::as_array) {
        let mut segments = Vec::new();
        for part in parts {
            match part {
                JsonValue::String(s) if !s.trim().is_empty() => segments.push(s.clone()),
                JsonValue::Object(obj) => {
                    if let Some(text) = obj.get("text").and_then(JsonValue::as_str) {
                        if !text.trim().is_empty() {
                            segments.push(text.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        if !segments.is_empty() {
            return Some(segments.join("\n\n"));
        }
    }
    None
}

fn extract_copilot_assistant_text(req: &Map<String, JsonValue>) -> Option<String> {
    if let Some(text) = extract_assistant_from_metadata_messages(req) {
        return Some(text);
    }
    if let Some(text) = extract_assistant_from_tool_rounds(req) {
        return Some(text);
    }
    if let Some(text) = extract_assistant_from_response_blocks(req) {
        return Some(text);
    }
    req.get("response")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn extract_assistant_from_metadata_messages(req: &Map<String, JsonValue>) -> Option<String> {
    let metadata = req
        .get("result")
        .and_then(JsonValue::as_object)?
        .get("metadata")
        .and_then(JsonValue::as_object)?;
    let messages = metadata.get("messages")?.as_array()?;
    for item in messages.iter().rev() {
        let Some(message) = item.as_object() else {
            continue;
        };
        if message.get("role").and_then(JsonValue::as_str) != Some("assistant") {
            continue;
        }
        if let Some(content) = message.get("content").and_then(JsonValue::as_str) {
            if !content.trim().is_empty() {
                return Some(content.to_string());
            }
        }
    }
    None
}

fn extract_assistant_from_tool_rounds(req: &Map<String, JsonValue>) -> Option<String> {
    let metadata = req
        .get("result")
        .and_then(JsonValue::as_object)?
        .get("metadata")
        .and_then(JsonValue::as_object)?;
    let rounds = metadata.get("toolCallRounds")?.as_array()?;
    let mut segments = Vec::new();
    for round in rounds {
        let Some(round_obj) = round.as_object() else {
            continue;
        };
        let Some(response) = round_obj.get("response") else {
            continue;
        };
        if let Some(text) = response.as_str() {
            if !text.trim().is_empty() {
                segments.push(text.to_string());
            }
        }
    }
    if segments.is_empty() {
        None
    } else {
        Some(segments.join("\n\n"))
    }
}

fn extract_assistant_from_response_blocks(req: &Map<String, JsonValue>) -> Option<String> {
    let response_items = req.get("response")?.as_array()?;
    let mut segments = Vec::new();
    for item in response_items {
        let Some(item_obj) = item.as_object() else {
            continue;
        };
        let Some(value) = item_obj.get("value").and_then(JsonValue::as_str) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() || value == "````" {
            continue;
        }
        segments.push(value.to_string());
    }
    if segments.is_empty() {
        None
    } else {
        Some(segments.join(""))
    }
}

fn json_f64(value: &JsonValue) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|v| v as f64))
        .or_else(|| value.as_u64().map(|v| v as f64))
}

fn parse_epoch_number(value: f64) -> Option<Epoch> {
    if !value.is_finite() {
        return None;
    }
    let seconds = if value.abs() > 1.0e11 {
        value / 1000.0
    } else {
        value
    };
    common::epoch_from_seconds(seconds)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let pile_path = cli.pile.clone().unwrap_or_else(common::default_pile_path);
    if let Err(err) = common::emit_schema_to_atlas(&pile_path) {
        eprintln!("atlas emit: {err}");
    }
    let Some(path) = cli.path else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };
    let branch_id =
        common::resolve_archive_branch_id(&pile_path, &cli.branch, cli.branch_id.as_deref())?;
    let (mut repo, branch_id) = common::open_repo_for_write(&pile_path, branch_id, &cli.branch)?;
    let res = import_copilot_path(&path, &mut repo, branch_id);
    let close_res = repo
        .close()
        .map_err(|e| anyhow!("close pile {}: {e:?}", pile_path.display()));
    match (res, close_res) {
        (Ok(stats), Ok(())) => {
            println!(
                "Imported {} file(s), {} conversation(s), {} message(s) in {} new commit(s).",
                stats.files, stats.conversations, stats.messages, stats.commits
            );
            Ok(())
        }
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(close_err)) => {
            eprintln!("warning: close pile after error: {close_err:#}");
            Err(err)
        }
    }
}
