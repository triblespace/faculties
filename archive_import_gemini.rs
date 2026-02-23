#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4"
//! rand_core = "0.6.4"
//! scraper = "0.23"
//! serde_json = "1"
//! triblespace = "0.16.0"
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser};
use hifitime::{Duration, Epoch};
use scraper::{Html, Selector};
use serde_json::{Map, Value as JsonValue};
use triblespace::core::id::ExclusiveId;
use triblespace::core::import::json_tree::JsonTreeImporter;
use triblespace::prelude::*;


#[path = "archive_common.rs"]
mod common;

#[derive(Parser)]
#[command(
    name = "archive-import-gemini",
    about = "Import Gemini exports into TribleSpace"
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
    conversation_id: String,
    source_message_id: String,
    role: String,
    author: String,
    content: String,
    created_at: Option<Epoch>,
    order: usize,
}

fn import_gemini_path(path: &std::path::Path, repo: &mut common::Repo, branch_id: Id) -> Result<ImportStats> {
    if path.is_dir() {
        let mut files = Vec::new();
        collect_gemini_files(path, &mut files)
            .with_context(|| format!("scan {}", path.display()))?;
        files.sort();
        let mut total = ImportStats::default();
        for file in files {
            let stats = import_gemini_file(&file, repo, branch_id)
                .with_context(|| format!("import {}", file.display()))?;
            total.files += stats.files;
            total.conversations += stats.conversations;
            total.messages += stats.messages;
            total.commits += stats.commits;
        }
        return Ok(total);
    }
    import_gemini_file(path, repo, branch_id)
}

fn import_gemini_file(path: &std::path::Path, repo: &mut common::Repo, branch_id: Id) -> Result<ImportStats> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
    let mut catalog = ws.checkout(..).context("checkout workspace")?;

    let mut stats = ImportStats {
        files: 1,
        ..ImportStats::default()
    };

    let source_path = path.to_string_lossy().to_string();
    let source_path_handle = ws.put(source_path.clone());
    let default_conversation_id = format!(
        "gemini:{:x}",
        common::stable_id(&["playground", "import", "gemini", "conversation", raw.as_str()])
    );

    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let (raw_root, mut records) = if extension == "html" || extension == "htm" {
        let records = parse_gemini_activity_html(&raw, &default_conversation_id);
        (None, records)
    } else if extension == "jsonl" {
        let mut records = Vec::new();
        for (line_idx, line) in raw.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let json: JsonValue = serde_json::from_str(trimmed)
                .with_context(|| format!("parse jsonl line {}", line_idx + 1))?;
            records.push((line_idx, json));
        }
        let mut parsed = Vec::new();
        let mut order = 0usize;
        let mut dedupe = HashSet::new();
        for (_, json) in &records {
            collect_gemini_messages(
                json,
                &default_conversation_id,
                &mut parsed,
                &mut dedupe,
                &mut order,
            );
        }
        (None, parsed)
    } else {
        let root: JsonValue = serde_json::from_str(&raw).context("parse gemini json")?;

        let json_tree_metadata =
            triblespace::core::import::json_tree::build_json_tree_metadata(repo.storage_mut())
                .map_err(|e| anyhow!("build json tree metadata: {e:?}"))?
                .into_facts();
        let raw_root = {
            let mut importer =
                JsonTreeImporter::<_, triblespace::prelude::valueschemas::Blake3>::new(
                    repo.storage_mut(),
                    None,
                );
            let fragment = importer
                .import_str(&raw)
                .context("import gemini raw json tree")?;
            let root_id = fragment
                .root()
                .ok_or_else(|| anyhow!("json tree importer did not return a single root"))?;
            let delta = fragment.facts().difference(&catalog);
            if !delta.is_empty() {
                ws.commit(
                    delta.clone(),
                    Some(json_tree_metadata.clone()),
                    Some("import gemini json tree"),
                );
                common::push_workspace(repo, &mut ws).context("push gemini json tree")?;
                catalog += delta;
                stats.commits += 1;
            }
            root_id
        };

        let mut parsed = Vec::new();
        let mut order = 0usize;
        let mut dedupe = HashSet::new();
        collect_gemini_messages(
            &root,
            &default_conversation_id,
            &mut parsed,
            &mut dedupe,
            &mut order,
        );
        (Some(raw_root), parsed)
    };

    records.sort_by_key(|r| r.order);
    let mut by_conversation: BTreeMap<String, Vec<MessageRecord>> = BTreeMap::new();
    for record in records {
        by_conversation
            .entry(record.conversation_id.clone())
            .or_default()
            .push(record);
    }

    let mut change = TribleSet::new();
    let mut author_cache: HashMap<String, Id> = HashMap::new();
    for (conversation_id, messages) in by_conversation {
        let batch_fragment = entity! { _ @
            common::import_schema::kind: common::import_schema::kind_batch,
            common::import_schema::source_format: "gemini",
            common::import_schema::source_conversation_id: ws.put(conversation_id.clone()),
        };
        let batch_id = batch_fragment
            .root()
            .expect("entity! must export a single root id");
        let batch_entity = ExclusiveId::force_ref(&batch_id);
        change += batch_fragment;
        change += entity! { batch_entity @
            common::import_schema::source_path: source_path_handle,
        };
        if let Some(raw_root) = raw_root {
            change += entity! { batch_entity @ common::import_schema::source_raw_root: raw_root };
        }

        let mut previous: Option<(Id, String)> = None;
        for message in messages {
            let source_message_id_handle = ws.put(message.source_message_id.clone());
            let message_fragment = entity! { _ @
                common::import_schema::batch: batch_id,
                common::import_schema::source_message_id: source_message_id_handle,
            };
            let message_id = message_fragment
                .root()
                .expect("entity! must export a single root id");
            let message_entity = ExclusiveId::force_ref(&message_id);
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
            let content_handle = ws.put(message.content.clone());
            change += entity! { message_entity @
                common::archive::kind: common::archive::kind_message,
                common::archive::author: author_id,
                common::archive::content: content_handle,
                common::archive::created_at: created_at,
            };
            change += entity! { message_entity @
                common::import_schema::batch: batch_id,
                common::import_schema::source_message_id: source_message_id_handle,
                common::import_schema::source_author: ws.put(message.author.clone()),
                common::import_schema::source_role: ws.put(message.role.clone()),
                common::import_schema::source_created_at: created_at,
            };
            if let Some((parent_id, parent_source_id)) = previous.as_ref() {
                change += entity! { message_entity @ common::archive::reply_to: *parent_id };
                change += entity! { message_entity @
                    common::import_schema::source_parent_id: ws.put(parent_source_id.clone()),
                };
            }
            previous = Some((message_id, message.source_message_id.clone()));
            stats.messages += 1;
        }

        stats.conversations += 1;
    }

    let delta = change.difference(&catalog);
    if !delta.is_empty() {
        ws.commit(delta, None, Some("import gemini"));
        common::push_workspace(repo, &mut ws).context("push gemini import")?;
        stats.commits += 1;
    }
    Ok(stats)
}

fn collect_gemini_files(path: &std::path::Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("read dir {}", path.display()))? {
        let entry = entry.context("read dir entry")?;
        let entry_path = entry.path();
        let file_type = entry.file_type().context("entry type")?;
        if file_type.is_dir() {
            collect_gemini_files(&entry_path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        match entry_path.extension().and_then(|s| s.to_str()) {
            Some("json") | Some("jsonl") | Some("html") | Some("htm") => out.push(entry_path),
            _ => {}
        }
    }
    Ok(())
}

fn parse_gemini_activity_html(html: &str, conversation_id: &str) -> Vec<MessageRecord> {
    let document = Html::parse_document(html);
    let outer_selector = Selector::parse(
        "div.outer-cell.mdl-cell.mdl-cell--12-col.mdl-shadow--2dp",
    )
    .expect("valid Gemini outer-cell selector");
    let left_selector = Selector::parse(
        "div.content-cell.mdl-cell.mdl-cell--6-col.mdl-typography--body-1",
    )
    .expect("valid Gemini left-cell selector");

    let mut out = Vec::new();
    let mut index = 0usize;

    for outer in document.select(&outer_selector) {
        let Some(left_cell) = outer.select(&left_selector).find(|cell| {
            let classes = cell.value().attr("class").unwrap_or("");
            !classes
                .split_whitespace()
                .any(|class| class == "mdl-typography--text-right")
        }) else {
            continue;
        };
        let left_html = left_cell.inner_html();
        let left_text = html_to_text(&left_html);
        let lines: Vec<String> = left_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if lines.is_empty() {
            continue;
        }

        let timestamp_idx = lines.iter().position(|line| parse_gemini_activity_timestamp(line).is_some());
        let (pre, timestamp, post) = match timestamp_idx {
            Some(idx) => {
                let ts = parse_gemini_activity_timestamp(&lines[idx]);
                (&lines[..idx], ts, &lines[idx + 1..])
            }
            None => (lines.as_slice(), None, &[][..]),
        };

        if pre.is_empty() {
            continue;
        }

        let prompted = pre[0].starts_with("Prompted");
        let mut user_lines = Vec::new();
        for (idx, line) in pre.iter().enumerate() {
            let mut line = line.as_str();
            if idx == 0 && prompted {
                line = line
                    .trim_start_matches("Prompted")
                    .trim_start_matches('\u{00a0}')
                    .trim();
            }
            if is_gemini_meta_line(line) {
                continue;
            }
            if !line.is_empty() {
                user_lines.push(line.to_string());
            }
        }

        let assistant_lines: Vec<String> = post
            .iter()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || is_gemini_meta_line(line) {
                    None
                } else {
                    Some(line.to_string())
                }
            })
            .collect();

        let created_at = timestamp;
        if !user_lines.is_empty() {
            out.push(MessageRecord {
                conversation_id: conversation_id.to_string(),
                source_message_id: format!("activity-{index:08}:user"),
                role: "user".to_string(),
                author: "user".to_string(),
                content: user_lines.join("\n"),
                created_at,
                order: index * 2,
            });
        }
        if !assistant_lines.is_empty() {
            out.push(MessageRecord {
                conversation_id: conversation_id.to_string(),
                source_message_id: format!("activity-{index:08}:assistant"),
                role: "assistant".to_string(),
                author: "assistant".to_string(),
                content: assistant_lines.join("\n\n"),
                created_at,
                order: index * 2 + 1,
            });
        }
        index += 1;
    }

    out
}

fn is_gemini_meta_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.starts_with("Attached ") {
        return true;
    }
    if trimmed.starts_with("- ") {
        return true;
    }
    if trimmed.ends_with(" generated image.") || trimmed.ends_with(" generated images.") {
        return true;
    }
    false
}

fn parse_gemini_activity_timestamp(input: &str) -> Option<Epoch> {
    let text = input.trim();
    let (datetime, tz) = text.rsplit_once(' ')?;
    let (date_part, time_part) = datetime.split_once(", ")?;
    let mut date_tokens = date_part.split_whitespace();
    let day: u8 = date_tokens.next()?.parse().ok()?;
    let month = match date_tokens.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = date_tokens.next()?.parse().ok()?;
    if date_tokens.next().is_some() {
        return None;
    }

    let mut time_tokens = time_part.split(':');
    let hour: u8 = time_tokens.next()?.parse().ok()?;
    let minute: u8 = time_tokens.next()?.parse().ok()?;
    let second: u8 = time_tokens.next()?.parse().ok()?;
    if time_tokens.next().is_some() {
        return None;
    }

    let local = Epoch::from_gregorian_utc(year, month, day, hour, minute, second, 0);
    let offset_hours = match tz {
        "UTC" | "GMT" => 0.0,
        "CET" => 1.0,
        "CEST" => 2.0,
        "PST" => -8.0,
        "PDT" => -7.0,
        "MST" => -7.0,
        "MDT" => -6.0,
        "CST" => -6.0,
        "CDT" => -5.0,
        "EST" => -5.0,
        "EDT" => -4.0,
        _ => return None,
    };
    Some(local - Duration::from_hours(offset_hours))
}

fn html_to_text(input: &str) -> String {
    let mut text = input
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("</p>", "\n")
        .replace("<p>", "");

    let mut stripped = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.drain(..) {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => stripped.push(ch),
            _ => {}
        }
    }
    decode_html_entities(&stripped)
}

fn decode_html_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        let rest = &input[i..];
        if rest.starts_with('&') {
            if let Some(end_rel) = rest.find(';') {
                let entity = &rest[1..end_rel];
                if let Some(decoded) = decode_entity(entity) {
                    out.push(decoded);
                    i += end_rel + 1;
                    continue;
                }
            }
        }
        if let Some(ch) = rest.chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "nbsp" => Some(' '),
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "#39" => Some('\''),
        _ => {
            if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                let value = u32::from_str_radix(hex, 16).ok()?;
                return char::from_u32(value);
            }
            if let Some(dec) = entity.strip_prefix('#') {
                let value = dec.parse::<u32>().ok()?;
                return char::from_u32(value);
            }
            None
        }
    }
}

fn collect_gemini_messages(
    node: &JsonValue,
    default_conversation_id: &str,
    out: &mut Vec<MessageRecord>,
    dedupe: &mut HashSet<String>,
    next_index: &mut usize,
) {
    if let Some(obj) = node.as_object() {
        if let Some((role, content)) = extract_message_from_object(obj) {
            let conversation_id = extract_conversation_id(obj)
                .unwrap_or(default_conversation_id)
                .to_string();
            let source_message_id = extract_source_message_id(obj).unwrap_or_else(|| {
                let id = format!("node-{:08}", *next_index);
                *next_index += 1;
                id
            });
            let created_at = extract_created_at(obj);
            let dedupe_key = format!(
                "{}|{}|{}|{}|{}",
                conversation_id,
                source_message_id,
                role,
                created_at
                    .map(|e| e.to_tai_seconds().to_string())
                    .unwrap_or_else(|| "-".to_string()),
                content
            );
            if dedupe.insert(dedupe_key) {
                let role = canonical_role(&role).to_string();
                let author = canonical_author_name(&role).to_string();
                let order = *next_index;
                *next_index += 1;
                out.push(MessageRecord {
                    conversation_id,
                    source_message_id,
                    role,
                    author,
                    content,
                    created_at,
                    order,
                });
            }
        }

        for value in obj.values() {
            collect_gemini_messages(
                value,
                default_conversation_id,
                out,
                dedupe,
                next_index,
            );
        }
        return;
    }

    if let Some(items) = node.as_array() {
        for item in items {
            collect_gemini_messages(
                item,
                default_conversation_id,
                out,
                dedupe,
                next_index,
            );
        }
    }
}

fn extract_message_from_object(object: &Map<String, JsonValue>) -> Option<(String, String)> {
    let role = object.get("role").and_then(JsonValue::as_str)?;
    let content = object
        .get("text")
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .or_else(|| object.get("message").and_then(extract_text))
        .or_else(|| object.get("content").and_then(extract_text))
        .or_else(|| object.get("parts").and_then(extract_text))?;
    let content = content.trim().to_string();
    if content.is_empty() {
        return None;
    }
    Some((role.to_string(), content))
}

fn extract_text(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(s) => {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }
        JsonValue::Array(items) => {
            let mut segments = Vec::new();
            for item in items {
                if let Some(text) = extract_text(item) {
                    if !text.trim().is_empty() {
                        segments.push(text);
                    }
                }
            }
            if segments.is_empty() {
                None
            } else {
                Some(segments.join("\n\n"))
            }
        }
        JsonValue::Object(obj) => obj
            .get("text")
            .and_then(JsonValue::as_str)
            .map(str::to_owned)
            .or_else(|| obj.get("content").and_then(extract_text))
            .or_else(|| obj.get("parts").and_then(extract_text))
            .or_else(|| obj.get("message").and_then(extract_text))
            .or_else(|| obj.get("response").and_then(extract_text)),
        _ => None,
    }
}

fn extract_conversation_id(object: &Map<String, JsonValue>) -> Option<&str> {
    object
        .get("conversation_id")
        .and_then(JsonValue::as_str)
        .or_else(|| object.get("conversationId").and_then(JsonValue::as_str))
        .or_else(|| object.get("chat_id").and_then(JsonValue::as_str))
        .or_else(|| object.get("thread_id").and_then(JsonValue::as_str))
}

fn extract_source_message_id(object: &Map<String, JsonValue>) -> Option<String> {
    object
        .get("id")
        .and_then(JsonValue::as_str)
        .or_else(|| object.get("message_id").and_then(JsonValue::as_str))
        .or_else(|| object.get("messageId").and_then(JsonValue::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

fn extract_created_at(object: &Map<String, JsonValue>) -> Option<Epoch> {
    const KEYS: [&str; 7] = [
        "timestamp",
        "time",
        "created_at",
        "create_time",
        "createdAt",
        "update_time",
        "updated_at",
    ];
    for key in KEYS {
        let Some(value) = object.get(key) else {
            continue;
        };
        if let Some(epoch) = parse_epoch(value) {
            return Some(epoch);
        }
    }
    None
}

fn parse_epoch(value: &JsonValue) -> Option<Epoch> {
    match value {
        JsonValue::Number(num) => num.as_f64().and_then(parse_epoch_number),
        JsonValue::String(text) => parse_epoch_string(text),
        _ => None,
    }
}

fn parse_epoch_string(value: &str) -> Option<Epoch> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(epoch) = trimmed.parse::<Epoch>() {
        return Some(epoch);
    }
    trimmed.parse::<f64>().ok().and_then(parse_epoch_number)
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

fn canonical_role(role: &str) -> &str {
    match role {
        "model" | "assistant" | "agent" => "assistant",
        "human" | "user" => "user",
        "system" => "system",
        _ => role,
    }
}

fn canonical_author_name(role: &str) -> &str {
    match role {
        "assistant" => "assistant",
        "user" => "user",
        "system" => "system",
        _ => role,
    }
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
    let branch_id = common::resolve_archive_branch_id(
        &pile_path,
        &cli.branch,
        cli.branch_id.as_deref(),
    )?;
    let (mut repo, branch_id) = common::open_repo_for_write(&pile_path, branch_id, &cli.branch)?;
    let res = import_gemini_path(&path, &mut repo, branch_id);
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
