#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4"
//! rand_core = "0.6.4"
//! serde_json = "1"
//! triblespace = "0.12.0"
//! ```

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser};
use serde_json::Value as JsonValue;
use triblespace::core::import::json_tree::JsonTreeImporter;
use triblespace::core::id::ExclusiveId;
use triblespace::core::blob::Bytes;
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::*;

#[path = "archive_common.rs"]
mod common;

#[derive(Parser)]
#[command(name = "archive-import-chatgpt", about = "Import ChatGPT exports into TribleSpace")]
struct Cli {
    /// Path to the pile file to write into.
    #[arg(long, global = true)]
    pile: Option<PathBuf>,
    /// Branch name to write into (created if missing).
    #[arg(long, default_value = "archive", global = true)]
    branch: String,
    /// File or directory containing ChatGPT exports.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
}

#[derive(Debug, Default, Clone)]
struct ImportStats {
    conversations: usize,
    messages: usize,
    attachments: usize,
    commits: usize,
}

fn import_chatgpt_path(
    path: &Path,
    repo: &mut common::Repo,
    branch_id: Id,
) -> Result<ImportStats> {
    if path.is_dir() {
        let mut paths = Vec::new();
        collect_conversation_files(path, &mut paths)
            .with_context(|| format!("scan {}", path.display()))?;
        paths.sort();
        let mut total = ImportStats::default();
        for convo_path in paths {
            let stats = import_chatgpt_file(&convo_path, repo, branch_id)
                .with_context(|| format!("import {}", convo_path.display()))?;
            total.conversations += stats.conversations;
            total.messages += stats.messages;
            total.attachments += stats.attachments;
            total.commits += stats.commits;
        }
        return Ok(total);
    }

    import_chatgpt_file(path, repo, branch_id)
}

fn import_chatgpt_file(
    path: &Path,
    repo: &mut common::Repo,
    branch_id: Id,
) -> Result<ImportStats> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let root: JsonValue = serde_json::from_str(&raw).context("parse chatgpt json")?;
    let conversations = root
        .as_array()
        .ok_or_else(|| anyhow!("chatgpt export must be a JSON array"))?;

    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
    let mut catalog = ws.checkout(..).context("checkout workspace")?;

    let json_tree_metadata =
        triblespace::core::import::json_tree::build_json_tree_metadata(repo.storage_mut())
            .map_err(|e| anyhow!("build json tree metadata: {e:?}"))?;

    let export_root = path.parent().unwrap_or_else(|| Path::new("."));
    let export_files =
        index_export_files(export_root).with_context(|| format!("index {}", export_root.display()))?;
.to_string_lossy().to_string());

    let mut stats = ImportStats::default();
    for convo in conversations {
        let convo_id = convo
            .get("id")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");

        let convo_raw = serde_json::to_string(convo).context("serialize conversation json")?;
        let (raw_root, raw_delta) = {
            let mut raw_importer = JsonTreeImporter::<_, triblespace::prelude::valueschemas::Blake3>::new(
                repo.storage_mut(),
                None,
            );
            let raw_root = raw_importer
                .import_str(&convo_raw)
                .with_context(|| format!("import json tree for conversation {convo_id}"))?;
            let raw_delta = raw_importer.data().difference(&catalog);
            (raw_root, raw_delta)
        };
        if !raw_delta.is_empty() {
            ws.commit(
                raw_delta.clone(),
                Some(json_tree_metadata.clone()),
                Some("import chatgpt json tree"),
            );
            common::push_workspace(repo, &mut ws).context("push json tree")?;
            catalog.union(raw_delta);
            stats.commits += 1;
        }

        let title = convo.get("title").and_then(JsonValue::as_str).unwrap_or("");
        let created_epoch = convo
            .get("create_time")
            .and_then(JsonValue::as_f64)
            .and_then(common::epoch_from_seconds);
        let created_at = created_epoch.map(common::epoch_interval);

        let mapping = convo
            .get("mapping")
            .and_then(JsonValue::as_object)
            .ok_or_else(|| anyhow!("conversation {convo_id} missing mapping"))?;

        let batch_id = common::stable_id(&["playground", "import", "chatgpt", "batch", convo_id]);
        let batch_entity = ExclusiveId::force_ref(&batch_id);

        let mut change = TribleSet::new();
        change += entity! { batch_entity @
            common::import_schema::kind: common::import_schema::kind_batch,
            common::import_schema::source_format: "chatgpt",
            common::import_schema::source_path: path_handle,
            common::import_schema::source_raw_root: raw_root,
            common::import_schema::source_conversation_id: ws.put(convo_id.to_string()),
        };
        if !title.is_empty() {
            change += entity! { batch_entity @
                common::import_schema::source_title: ws.put(title.to_string()),
            };
        }
        if let Some(created_at) = created_at {
            change += entity! { batch_entity @ common::import_schema::source_created_at: created_at };
        }

        let mut node_to_message: HashMap<&str, Id> = HashMap::new();
        for (node_id, node) in mapping {
            let message = node.get("message").and_then(JsonValue::as_object);
            let should_import = message.is_some_and(should_import_message);
            if should_import {
                node_to_message.insert(
                    node_id.as_str(),
                    common::stable_id(&[
                        "playground",
                        "import",
                        "chatgpt",
                        "message",
                        convo_id,
                        node_id.as_str(),
                    ]),
                );
            }
        }

        let mut attachment_data_loaded: HashSet<Id> = HashSet::new();
        for (node_id, node) in mapping {
            let message = match node.get("message").and_then(JsonValue::as_object) {
                Some(msg) => msg,
                None => continue,
            };
            if !should_import_message(message) {
                continue;
            }
            let content = extract_message_text(message).unwrap_or_default();
            let Some(message_id) = node_to_message.get(node_id.as_str()).copied() else {
                continue;
            };
            let message_entity = ExclusiveId::force_ref(&message_id);

            let role = message
                .get("author")
                .and_then(JsonValue::as_object)
                .and_then(|author| author.get("role"))
                .and_then(JsonValue::as_str)
                .unwrap_or("unknown");
            let name = message
                .get("author")
                .and_then(JsonValue::as_object)
                .and_then(|author| author.get("name"))
                .and_then(JsonValue::as_str)
                .unwrap_or(role);

            let (author_id, author_change) = common::ensure_author(&mut ws, &catalog, name, role)?;
            change += author_change;

            let created_at_epoch = message
                .get("create_time")
                .and_then(JsonValue::as_f64)
                .and_then(common::epoch_from_seconds)
                .or_else(|| created_epoch.clone())
                .unwrap_or_else(common::now_epoch);
            let created_at = common::epoch_interval(created_at_epoch);

            let (ws.put(content)){ message_entity @
                common::archive::kind: common::archive::kind_message,
                common::archive::author: author_id,
                common::archive::content: content_handle,
                common::archive::created_at: created_at,
            };

            if let Some(content_type) = message_content_type(message) {
                if !content_type.is_empty() {
                    change += entity! { message_entity @ common::archive::content_type: content_type };
                }
            }

            // Import attachments referenced by this message (images, files, etc).
            for attachment in collect_attachments(message) {
                let AttachmentFields {
                    source_id,
                    source_pointer,
                    name,
                    mime,
                    size_bytes,
                    width_px,
                    height_px,
                } = attachment;
                let attachment_id =
                    common::stable_id(&[
                        "playground",
                        "import",
                        "chatgpt",
                        "attachment",
                        source_id.as_str(),
                    ]);
                let attachment_entity = ExclusiveId::force_ref(&attachment_id);

                // Always link the message -> attachment, even if we don't have bytes.
                change += entity! { message_entity @ common::archive::attachment: attachment_id };

                let (ws.put(source_id.clone())) = ws.put(source_id.clone());
          common::archive::kind: common::archive::kind_attachment,
                    common::archive::attachment_source_id: source_id_handle,
                };

                if let Some(pointer) = source_pointer {
                    change += entity! { attachment_entity @
                        common::archive::attachment_source_pointer: ws.put(pointer),
                    };
                }
                if let Some(name) = name {
                    change += entity! { attachment_entity @
                        common::archive::attachment_name: ws.put(name),
                    };
                }
                if let Some(mime) = mime {
                    change += entity! { attachment_entity @
                        common::archive::attachment_mime: mime.as_str(),
                    };
                }
                if let Some(size) = size_bytes {
                    change += entity! { attachment_entity @ common::archive::attachment_size_bytes: size };
                }
                if let Some(width) = width_px {
                    change += entity! { attachment_entity @ common::archive::attachment_width_px: width };
                }
                if let Some(height) = height_px {
                    change += entity! { attachment_entity @ common::archive::attachment_height_px: height };
                }

                let needs_data = attachment_data_handle(&catalog, attachment_id).is_none()
                    && attachment_data_handle(&change, attachment_id).is_none();
                if needs_data {
                    if let Some(path) = export_files.get(&source_id) {
                        if attachment_data_loaded.insert(attachment_id) {
                            let bytes = fs::read(path)
                                .with_context(|| format!("read attachment {}", path.display()))?;
                            let (ws.put(
                                Bytes::from_source(bytes),
                            )) = ws.attachment_entity @
                                common::archive::attachment_data: data_handle,
                            };
                            stats.attachments += 1;
                        }
                    }
                }
            }

            if let Some(parent) = node.get("parent").and_then(JsonValue::as_str) {
                if let Some(parent_id) = node_to_message.get(parent).copied() {
                    change += entity! { message_entity @ common::archive::reply_to: parent_id };
                    change += entity! { message_entity @
                        common::import_schema::source_parent_id: ws.put(parent.to_string()),
                    };
                }
            }

            change += entity! { message_entity @
                common::import_schema::batch: batch_id,
                common::import_schema::source_message_id: ws.put(node_id.to_string()),
                common::import_schema::source_author: ws.put(name.to_string()),
                common::import_schema::source_role: ws.put(role.to_string()),
                common::import_schema::source_created_at: created_at,
            };

            stats.messages += 1;
        }

        let delta = change.difference(&catalog);
        if !delta.is_empty() {
            ws.commit(delta.clone(), None, Some("import chatgpt"));
            common::push_workspace(repo, &mut ws).context("push import")?;
            catalog.union(delta);
            stats.commits += 1;
        }
        stats.conversations += 1;
    }

    Ok(stats)
}

#[derive(Debug, Default, Clone)]
struct AttachmentFields {
    source_id: String,
    source_pointer: Option<String>,
    name: Option<String>,
    mime: Option<String>,
    size_bytes: Option<u64>,
    width_px: Option<u64>,
    height_px: Option<u64>,
}

fn collect_conversation_files(path: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("read dir {}", path.display()))? {
        let entry = entry.context("read dir entry")?;
        let entry_path = entry.path();
        let file_type = entry.file_type().context("entry type")?;
        if file_type.is_dir() {
            collect_conversation_files(&entry_path, out)?;
        } else if file_type.is_file() {
            if entry_path.file_name().and_then(|name| name.to_str()) == Some("conversations.json")
            {
                out.push(entry_path);
            }
        }
    }
    Ok(())
}

fn index_export_files(root: &Path) -> Result<HashMap<String, PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("read dir {}", dir.display()))? {
            let entry = entry.context("read dir entry")?;
            let entry_path = entry.path();
            let file_type = entry.file_type().context("entry type")?;
            if file_type.is_dir() {
                stack.push(entry_path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(file_id) = chatgpt_file_id_from_filename(name) else {
                continue;
            };
            files.push((file_id, entry_path));
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = HashMap::new();
    for (file_id, path) in files {
        out.entry(file_id).or_insert(path);
    }
    Ok(out)
}

fn chatgpt_file_id_from_filename(filename: &str) -> Option<String> {
    // ChatGPT exports commonly store attachments as:
    // - file-<id>-image.png
    // - file-<id>-Screenshot ... .png
    // - file-<id>-<uuid>.jpeg
    // We key by the "file-<id>" prefix.
    if !filename.starts_with("file-") {
        return None;
    }
    let mut it = filename.splitn(3, '-');
    let first = it.next()?;
    let second = it.next()?;
    if first != "file" || second.is_empty() {
        return None;
    }
    Some(format!("{first}-{second}"))
}

fn message_content_type(message: &serde_json::Map<String, JsonValue>) -> Option<&str> {
    message
        .get("content")
        .and_then(JsonValue::as_object)
        .and_then(|c| c.get("content_type"))
        .and_then(JsonValue::as_str)
}

fn should_import_message(message: &serde_json::Map<String, JsonValue>) -> bool {
    let text = extract_message_text(message).unwrap_or_default();
    if !text.is_empty() {
        return true;
    }

    // Import messages that carry file pointers even if they have no caption text.
    if has_message_attachments(message) {
        return true;
    }

    false
}

fn has_message_attachments(message: &serde_json::Map<String, JsonValue>) -> bool {
    let meta_attachments = message
        .get("metadata")
        .and_then(JsonValue::as_object)
        .and_then(|m| m.get("attachments"))
        .and_then(JsonValue::as_array)
        .is_some_and(|arr| !arr.is_empty());
    if meta_attachments {
        return true;
    }

    let Some(parts) = message
        .get("content")
        .and_then(JsonValue::as_object)
        .and_then(|c| c.get("parts"))
        .and_then(JsonValue::as_array)
    else {
        return false;
    };

    for part in parts {
        let Some(obj) = part.as_object() else {
            continue;
        };
        if let Some(pointer) = obj.get("asset_pointer").and_then(JsonValue::as_str) {
            if file_id_from_asset_pointer(pointer).is_some() {
                return true;
            }
        }
    }
    false
}

fn file_id_from_asset_pointer(pointer: &str) -> Option<&str> {
    // Typical pointer: "file-service://file-<id>"
    let prefix = "file-service://";
    if pointer.starts_with(prefix) {
        let rest = &pointer[prefix.len()..];
        if rest.starts_with("file-") {
            return Some(rest);
        }
    }
    None
}

fn collect_attachments(message: &serde_json::Map<String, JsonValue>) -> Vec<AttachmentFields> {
    let mut by_id: HashMap<String, AttachmentFields> = HashMap::new();

    // 1) metadata.attachments
    if let Some(attachments) = message
        .get("metadata")
        .and_then(JsonValue::as_object)
        .and_then(|m| m.get("attachments"))
        .and_then(JsonValue::as_array)
    {
        for att in attachments {
            let Some(obj) = att.as_object() else {
                continue;
            };
            let Some(id) = obj.get("id").and_then(JsonValue::as_str) else {
                continue;
            };
            let entry = by_id.entry(id.to_string()).or_insert_with(|| AttachmentFields {
                source_id: id.to_string(),
                ..Default::default()
            });

            if let Some(name) = obj.get("name").and_then(JsonValue::as_str) {
                entry.name.get_or_insert_with(|| name.to_string());
            }
            if let Some(mime) = obj.get("mime_type").and_then(JsonValue::as_str) {
                entry.mime.get_or_insert_with(|| mime.to_string());
            }

            // The export uses `size` in metadata.attachments.
            if let Some(size) = json_u64(obj.get("size")) {
                entry.size_bytes.get_or_insert(size);
            }
            if let Some(width) = json_u64(obj.get("width")) {
                entry.width_px.get_or_insert(width);
            }
            if let Some(height) = json_u64(obj.get("height")) {
                entry.height_px.get_or_insert(height);
            }
        }
    }

    // 2) content.parts objects (contains asset_pointer + size_bytes, etc)
    if let Some(parts) = message
        .get("content")
        .and_then(JsonValue::as_object)
        .and_then(|c| c.get("parts"))
        .and_then(JsonValue::as_array)
    {
        for part in parts {
            let Some(obj) = part.as_object() else {
                continue;
            };
            let Some(pointer) = obj.get("asset_pointer").and_then(JsonValue::as_str) else {
                continue;
            };
            let Some(file_id) = file_id_from_asset_pointer(pointer) else {
                continue;
            };

            let entry = by_id.entry(file_id.to_string()).or_insert_with(|| AttachmentFields {
                source_id: file_id.to_string(),
                ..Default::default()
            });

            entry
                .source_pointer
                .get_or_insert_with(|| pointer.to_string());

            // The parts use `size_bytes`.
            if let Some(size) = json_u64(obj.get("size_bytes")) {
                entry.size_bytes.get_or_insert(size);
            }
            if let Some(width) = json_u64(obj.get("width")) {
                entry.width_px.get_or_insert(width);
            }
            if let Some(height) = json_u64(obj.get("height")) {
                entry.height_px.get_or_insert(height);
            }
        }
    }

    let mut out: Vec<_> = by_id.into_values().collect();
    out.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    out
}

fn json_u64(v: Option<&JsonValue>) -> Option<u64> {
    let v = v?;
    if let Some(u) = v.as_u64() {
        return Some(u);
    }
    if let Some(i) = v.as_i64() {
        if i >= 0 {
            return Some(i as u64);
        }
    }
    if let Some(f) = v.as_f64() {
        if f.is_finite() && f >= 0.0 && f.fract() == 0.0 {
            return Some(f as u64);
        }
    }
    None
}

fn extract_message_text(message: &serde_json::Map<String, JsonValue>) -> Option<String> {
    let content = message.get("content")?.as_object()?;
    let parts = content.get("parts")?.as_array()?;
    let mut out = String::new();
    for part in parts {
        if let Some(text) = part.as_str() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn attachment_data_handle(
    catalog: &TribleSet,
    attachment_id: Id,
) -> Option<Value<triblespace::prelude::valueschemas::Handle<triblespace::prelude::valueschemas::Blake3, common::archive_schema::FileBytes>>> {
    find!(
        (handle: Value<triblespace::prelude::valueschemas::Handle<triblespace::prelude::valueschemas::Blake3, common::archive_schema::FileBytes>>),
        pattern!(catalog, [{ attachment_id @ common::archive::attachment_data: ?handle }])
    )
    .into_iter()
    .next()
    .map(|(h,)| h)
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
    let (mut repo, branch_id) = common::open_repo_for_write(&pile_path, &cli.branch)?;
    let stats = import_chatgpt_path(&path, &mut repo, branch_id)?;
    println!(
        "Imported {} conversation(s), {} message(s), {} attachment(s) in {} new commit(s).",
        stats.conversations, stats.messages, stats.attachments, stats.commits
    );
    repo.close()
        .map_err(|e| anyhow!("close pile {}: {e:?}", pile_path.display()))
}
