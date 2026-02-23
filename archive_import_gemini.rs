#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4"
//! rand_core = "0.6.4"
//! scraper = "0.23"
//! triblespace = "0.16.0"
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser};
use hifitime::{Duration, Epoch};
use scraper::{Html, Selector};
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
    let catalog = ws.checkout(..).context("checkout workspace")?;

    let mut stats = ImportStats {
        files: 1,
        ..ImportStats::default()
    };

    let source_path = path.to_string_lossy().to_string();
    let source_path_handle = ws.put(source_path.clone());
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    if extension != "html" && extension != "htm" {
        return Err(anyhow!(
            "Gemini importer currently supports only HTML exports; got {}",
            path.display()
        ));
    }
    let mut records = parse_gemini_activity_html(&raw);

    records.sort_by_key(|r| r.order);
    if records.is_empty() {
        return Ok(stats);
    }
    stats.conversations = 1;

    let mut change = TribleSet::new();
    let mut author_cache: HashMap<String, Id> = HashMap::new();
    let batch_fragment = entity! { _ @
        common::import_schema::kind: common::import_schema::kind_batch,
        common::import_schema::source_format: "gemini",
    };
    let batch_id = batch_fragment
        .root()
        .expect("entity! must export a single root id");
    let batch_entity = batch_id
        .aquire()
        .expect("entity! root ids should be acquired in current thread");
    change += batch_fragment;
    change += entity! { &batch_entity @
        common::import_schema::source_path: source_path_handle,
    };

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
        ws.commit(delta, None, Some("import gemini"));
        common::push_workspace(repo, &mut ws).context("push gemini import")?;
        stats.commits += 1;
    }
    Ok(stats)
}

fn hash_prefix(input: &str) -> String {
    use triblespace::core::value::schemas::hash::Blake3 as Blake3Hasher;

    let mut hasher = Blake3Hasher::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in digest.as_bytes().iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
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
            Some("html") | Some("htm") => out.push(entry_path),
            _ => {}
        }
    }
    Ok(())
}

fn parse_gemini_activity_html(html: &str) -> Vec<MessageRecord> {
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
            let user_content = user_lines.join("\n");
            out.push(MessageRecord {
                source_message_id: build_activity_source_message_id(
                    "user",
                    created_at,
                    user_content.as_str(),
                    index,
                ),
                role: "user".to_string(),
                author: "user".to_string(),
                content: user_content,
                created_at,
                order: index * 2,
            });
        }
        if !assistant_lines.is_empty() {
            let assistant_content = assistant_lines.join("\n\n");
            out.push(MessageRecord {
                source_message_id: build_activity_source_message_id(
                    "assistant",
                    created_at,
                    assistant_content.as_str(),
                    index,
                ),
                role: "assistant".to_string(),
                author: "assistant".to_string(),
                content: assistant_content,
                created_at,
                order: index * 2 + 1,
            });
        }
        index += 1;
    }

    out
}

fn build_activity_source_message_id(
    role: &str,
    created_at: Option<Epoch>,
    content: &str,
    index: usize,
) -> String {
    let ts = created_at
        .map(|epoch| epoch.to_tai_duration().total_nanoseconds().to_string())
        .unwrap_or_else(|| "-".to_string());
    let seed = format!("activity|{role}|{ts}|{}", content.trim());
    let hash = hash_prefix(seed.as_str());
    format!("activity:{hash}:{index:08}:{role}")
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
