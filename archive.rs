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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use hifitime::Epoch;
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::valueschemas::{Blake3, Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

#[path = "archive_common.rs"]
mod common;


#[derive(Parser)]
#[command(name = "archive", about = "Query imported archives in TribleSpace")]
struct Cli {
    /// Path to the pile file to query.
    #[arg(long, global = true)]
    pile: Option<PathBuf>,
    /// Branch name to query.
    #[arg(long, default_value = "archive", global = true)]
    branch: String,
    /// Branch id to query (hex). Overrides config/env branch id.
    #[arg(long, global = true)]
    branch_id: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Import external archives into the archive branch.
    Import {
        #[arg(value_enum)]
        source: ImportSource,
        /// Optional path override for this source (or backup root for `all`).
        path: Option<PathBuf>,
    },
    /// List the most recent messages.
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show one message by id prefix.
    Show {
        id: String,
    },
    /// Show a reply_to chain ending at the given message id prefix.
    Thread {
        id: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Search message content (substring match).
    Search {
        text: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Use case-sensitive matching.
        #[arg(long)]
        case_sensitive: bool,
    },
    /// List imported conversations.
    Imports {
        format: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ImportSource {
    Chatgpt,
    Codex,
    Copilot,
    Gemini,
    All,
}

impl ImportSource {
    fn label(self) -> &'static str {
        match self {
            ImportSource::Chatgpt => "chatgpt",
            ImportSource::Codex => "codex",
            ImportSource::Copilot => "copilot",
            ImportSource::Gemini => "gemini",
            ImportSource::All => "all",
        }
    }
}

#[derive(Debug, Clone)]
struct ImportJob {
    source: ImportSource,
    path: PathBuf,
}

fn faculty_dir() -> Result<PathBuf> {
    let arg0 = std::env::args()
        .next()
        .ok_or_else(|| anyhow!("missing argv[0]"))?;
    let path = PathBuf::from(arg0);
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .context("resolve current directory")?
            .join(path)
    };
    let parent = abs
        .parent()
        .ok_or_else(|| anyhow!("cannot resolve faculty directory"))?;
    Ok(parent.to_path_buf())
}

fn importer_script_name(source: ImportSource) -> Option<&'static str> {
    match source {
        ImportSource::Chatgpt => Some("archive_import_chatgpt.rs"),
        ImportSource::Codex => Some("archive_import_codex.rs"),
        ImportSource::Copilot => Some("archive_import_copilot.rs"),
        ImportSource::Gemini => Some("archive_import_gemini.rs"),
        ImportSource::All => None,
    }
}

fn default_source_path(source: ImportSource, base: &Path) -> PathBuf {
    match source {
        ImportSource::Chatgpt => base.to_path_buf(),
        ImportSource::Codex => base.join("codex"),
        ImportSource::Copilot => base.join("copilot"),
        ImportSource::Gemini => base.join("gemini/Takeout/My Activity/Gemini Apps/My Activity.html"),
        ImportSource::All => base.to_path_buf(),
    }
}

fn resolve_import_jobs(source: ImportSource, path: Option<&Path>) -> Result<Vec<ImportJob>> {
    match source {
        ImportSource::All => {
            let root = path
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("chatgptbackup"));
            Ok(vec![
                ImportJob {
                    source: ImportSource::Chatgpt,
                    path: default_source_path(ImportSource::Chatgpt, &root),
                },
                ImportJob {
                    source: ImportSource::Codex,
                    path: default_source_path(ImportSource::Codex, &root),
                },
                ImportJob {
                    source: ImportSource::Copilot,
                    path: default_source_path(ImportSource::Copilot, &root),
                },
                ImportJob {
                    source: ImportSource::Gemini,
                    path: default_source_path(ImportSource::Gemini, &root),
                },
            ])
        }
        one => Ok(vec![ImportJob {
            source: one,
            path: path
                .map(Path::to_path_buf)
                .unwrap_or_else(|| default_source_path(one, Path::new("chatgptbackup"))),
        }]),
    }
}

fn run_import_jobs(
    source: ImportSource,
    path: Option<&Path>,
    pile_path: &Path,
    branch_name: &str,
    branch_id: Id,
) -> Result<()> {
    let jobs = resolve_import_jobs(source, path)?;
    let importers_dir = faculty_dir()?.join("../importers");
    let branch_id_hex = format!("{branch_id:x}");
    println!(
        "archive import: {} job(s) -> {} ({}) on pile {}",
        jobs.len(),
        branch_name,
        branch_id_hex,
        pile_path.display()
    );

    for job in jobs {
        let Some(script_name) = importer_script_name(job.source) else {
            continue;
        };
        if source == ImportSource::All && !job.path.exists() {
            eprintln!(
                "skip {} import (path missing): {}",
                job.source.label(),
                job.path.display()
            );
            continue;
        }
        if !job.path.exists() {
            bail!(
                "{} import path not found: {}",
                job.source.label(),
                job.path.display()
            );
        }
        let script_path = importers_dir.join(script_name);
        if !script_path.exists() {
            bail!("missing importer script {}", script_path.display());
        }
        println!("import {} from {}", job.source.label(), job.path.display());
        let status = ProcessCommand::new(&script_path)
            .arg("--pile")
            .arg(pile_path)
            .arg("--branch")
            .arg(branch_name)
            .arg("--branch-id")
            .arg(&branch_id_hex)
            .arg(&job.path)
            .status()
            .with_context(|| {
                format!(
                    "run {} importer at {}",
                    job.source.label(),
                    script_path.display()
                )
            })?;
        if !status.success() {
            bail!(
                "{} importer failed with exit status {status}",
                job.source.label()
            );
        }
    }

    Ok(())
}

fn interval_key(interval: Value<NsTAIInterval>) -> i128 {
    let (lower, _upper): (Epoch, Epoch) = interval.from_value();
    lower.to_tai_duration().total_nanoseconds()
}

fn load_longstring(ws: &mut common::Ws, handle: Value<Handle<Blake3, LongString>>) -> Result<String> {
    let view: View<str> = ws.get(handle).context("read longstring")?;
    Ok(view.to_string())
}

fn u256be_to_u64(value: Value<U256BE>) -> Option<u64> {
    let raw = value.raw;
    if raw[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    let bytes: [u8; 8] = raw[24..32].try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn author_name(ws: &mut common::Ws, catalog: &TribleSet, author_id: Id) -> Result<String> {
    let Some(handle) = find!(
        (handle: Value<Handle<Blake3, LongString>>),
        pattern!(catalog, [{ author_id @ common::archive::author_name: ?handle }])
    )
    .into_iter()
    .next()
    .map(|(h,)| h) else {
        return Ok("<unknown>".to_string());
    };
    load_longstring(ws, handle)
}

fn author_role(ws: &mut common::Ws, catalog: &TribleSet, author_id: Id) -> Result<Option<String>> {
    let Some(handle) = find!(
        (handle: Value<Handle<Blake3, LongString>>),
        pattern!(catalog, [{ author_id @ common::archive::author_role: ?handle }])
    )
    .into_iter()
    .next()
    .map(|(h,)| h) else {
        return Ok(None);
    };
    Ok(Some(load_longstring(ws, handle)?))
}

fn message_content_type(catalog: &TribleSet, message_id: Id) -> Option<String> {
    find!(
        (content_type: String),
        pattern!(catalog, [{ message_id @ common::archive::content_type: ?content_type }])
    )
    .into_iter()
    .next()
    .map(|(ct,)| ct)
}

#[derive(Debug, Clone)]
struct AttachmentRecord {
    id: Id,
    source_id: Option<String>,
    name: Option<String>,
    mime: Option<String>,
    size_bytes: Option<u64>,
    width_px: Option<u64>,
    height_px: Option<u64>,
    has_data: bool,
}

fn message_attachments(
    ws: &mut common::Ws,
    catalog: &TribleSet,
    message_id: Id,
) -> Result<Vec<AttachmentRecord>> {
    let mut attachments: Vec<Id> = find!(
        (attachment: Id),
        pattern!(catalog, [{ message_id @ common::archive::attachment: ?attachment }])
    )
    .into_iter()
    .map(|(a,)| a)
    .collect();
    attachments.sort();
    attachments.dedup();

    let mut out = Vec::new();
    for attachment_id in attachments {
        let source_id = find!(
            (handle: Value<Handle<Blake3, LongString>>),
            pattern!(catalog, [{ attachment_id @ common::archive::attachment_source_id: ?handle }])
        )
        .into_iter()
        .next()
        .map(|(h,)| h);
        let source_id = match source_id {
            Some(h) => Some(load_longstring(ws, h)?),
            None => None,
        };

        let name = find!(
            (handle: Value<Handle<Blake3, LongString>>),
            pattern!(catalog, [{ attachment_id @ common::archive::attachment_name: ?handle }])
        )
        .into_iter()
        .next()
        .map(|(h,)| h);
        let name = match name {
            Some(h) => Some(load_longstring(ws, h)?),
            None => None,
        };

        let mime = find!(
            (mime: String),
            pattern!(catalog, [{ attachment_id @ common::archive::attachment_mime: ?mime }])
        )
        .into_iter()
        .next()
        .map(|(m,)| m);

        let size_bytes = find!(
            (size: Value<U256BE>),
            pattern!(catalog, [{ attachment_id @ common::archive::attachment_size_bytes: ?size }])
        )
        .into_iter()
        .next()
        .and_then(|(s,)| u256be_to_u64(s));

        let width_px = find!(
            (width: Value<U256BE>),
            pattern!(catalog, [{ attachment_id @ common::archive::attachment_width_px: ?width }])
        )
        .into_iter()
        .next()
        .and_then(|(w,)| u256be_to_u64(w));

        let height_px = find!(
            (height: Value<U256BE>),
            pattern!(catalog, [{ attachment_id @ common::archive::attachment_height_px: ?height }])
        )
        .into_iter()
        .next()
        .and_then(|(h,)| u256be_to_u64(h));

        let has_data = find!(
            (handle: Value<_>),
            pattern!(catalog, [{ attachment_id @ common::archive::attachment_data: ?handle }])
        )
        .into_iter()
        .next()
        .is_some();

        out.push(AttachmentRecord {
            id: attachment_id,
            source_id,
            name,
            mime,
            size_bytes,
            width_px,
            height_px,
            has_data,
        });
    }
    Ok(out)
}

fn resolve_message_id(catalog: &TribleSet, prefix: &str) -> Result<Id> {
    let trimmed = prefix.trim();
    if trimmed.len() == 32 {
        if let Some(id) = Id::from_hex(trimmed) {
            return Ok(id);
        }
    }

    let mut matches = Vec::new();
    for (message_id,) in find!(
        (message: Id),
        pattern!(catalog, [{
            ?message @ common::archive::kind: common::archive::kind_message,
        }])
    ) {
        if format!("{message_id:x}").starts_with(trimmed) {
            matches.push(message_id);
            if matches.len() > 10 {
                break;
            }
        }
    }

    match matches.len() {
        0 => bail!("no message matches id prefix {trimmed}"),
        1 => Ok(matches[0]),
        _ => bail!(
            "id prefix {trimmed} is ambiguous; matches: {}",
            matches
                .into_iter()
                .map(|id| format!("{id:x}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn message_record(
    ws: &mut common::Ws,
    catalog: &TribleSet,
    message_id: Id,
) -> Result<(Id, String, Option<String>, Value<NsTAIInterval>, Value<Handle<Blake3, LongString>>, Option<Id>)> {
    let Some((author_id, content_handle, created_at)) = find!(
        (
            author: Id,
            content: Value<Handle<Blake3, LongString>>,
            created_at: Value<NsTAIInterval>
        ),
        pattern!(catalog, [{
            message_id @
                common::archive::author: ?author,
                common::archive::content: ?content,
                common::archive::created_at: ?created_at,
        }])
    )
    .into_iter()
    .next()
    .map(|(a, c, t)| (a, c, t))
    else {
        return Err(anyhow!("message {message_id:x} missing required fields"));
    };

    let reply_to = find!(
        (parent: Id),
        pattern!(catalog, [{ message_id @ common::archive::reply_to: ?parent }])
    )
    .into_iter()
    .next()
    .map(|(p,)| p);

    let name = author_name(ws, catalog, author_id)?;
    let role = author_role(ws, catalog, author_id)?;
    Ok((message_id, name, role, created_at, content_handle, reply_to))
}

fn snippet(text: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if out.chars().count() >= max {
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let pile_path = cli.pile.clone().unwrap_or_else(common::default_pile_path);
    if let Err(err) = common::emit_schema_to_atlas(&pile_path) {
        eprintln!("atlas emit: {err}");
    }
    let Some(cmd) = cli.command else {
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
    if let Command::Import { source, path } = cmd {
        return run_import_jobs(
            source,
            path.as_deref(),
            &pile_path,
            &cli.branch,
            branch_id,
        );
    }

    let (mut repo, branch_id) = common::open_repo_for_read(&pile_path, branch_id, &cli.branch)?;

    let res = (|| -> Result<()> {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull workspace: {e:?}"))?;
        let catalog = ws.checkout(..).context("checkout workspace")?;

        match cmd {
            Command::Import { .. } => unreachable!("import is handled before opening the branch"),
            Command::List { limit } => {
                let mut records = Vec::new();
                for (message_id, author_id, content_handle, created_at) in find!(
                    (
                        message: Id,
                        author: Id,
                        content: Value<Handle<Blake3, LongString>>,
                        created_at: Value<NsTAIInterval>
                    ),
                    pattern!(&catalog, [{
                        ?message @
                            common::archive::kind: common::archive::kind_message,
                            common::archive::author: ?author,
                            common::archive::content: ?content,
                            common::archive::created_at: ?created_at,
                    }])
                ) {
                    records.push((interval_key(created_at), message_id, author_id, content_handle, created_at));
                }
                records.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                let take = limit.min(records.len());
                for (_key, message_id, author_id, content_handle, created_at) in records.into_iter().rev().take(take) {
                    let name = author_name(&mut ws, &catalog, author_id)?;
                    let role = author_role(&mut ws, &catalog, author_id)?;
                    let content = load_longstring(&mut ws, content_handle)?;
                    let (lower, _upper): (Epoch, Epoch) = created_at.from_value();
                    let role = role.as_deref().unwrap_or("");
                    println!(
                        "{} {} {} {}",
                        &format!("{message_id:x}")[..8],
                        lower,
                        if role.is_empty() { name } else { format!("{name} ({role})") },
                        snippet(&content, 120)
                    );
                }
            }
            Command::Show { id } => {
                let message_id = resolve_message_id(&catalog, &id)?;
                let (message_id, name, role, created_at, content_handle, reply_to) =
                    message_record(&mut ws, &catalog, message_id)?;
                let content = load_longstring(&mut ws, content_handle)?;
                let (lower, _upper): (Epoch, Epoch) = created_at.from_value();
                let content_type = message_content_type(&catalog, message_id);
                let attachments = message_attachments(&mut ws, &catalog, message_id)?;

                println!("id: {message_id:x}");
                println!("created_at: {lower}");
                match role {
                    Some(role) => println!("author: {name} ({role})"),
                    None => println!("author: {name}"),
                }
                if let Some(parent) = reply_to {
                    println!("reply_to: {parent:x}");
                }
                if let Some(content_type) = content_type {
                    println!("content_type: {content_type}");
                }
                if !attachments.is_empty() {
                    println!("attachments: {}", attachments.len());
                    for att in attachments {
                        let mut extras = Vec::new();
                        if let Some(mime) = att.mime.as_deref() {
                            extras.push(mime.to_string());
                        }
                        if let Some(size) = att.size_bytes {
                            extras.push(format!("{size}b"));
                        }
                        if let (Some(w), Some(h)) = (att.width_px, att.height_px) {
                            extras.push(format!("{w}x{h}px"));
                        }
                        if att.has_data {
                            extras.push("data".to_string());
                        }
                        let label = att
                            .name
                            .as_deref()
                            .or(att.source_id.as_deref())
                            .unwrap_or("<unknown>");
                        if extras.is_empty() {
                            println!("  - {} {}", &format!("{:x}", att.id)[..8], label);
                        } else {
                            println!(
                                "  - {} {} ({})",
                                &format!("{:x}", att.id)[..8],
                                label,
                                extras.join(", ")
                            );
                        }
                    }
                }
                println!();
                print!("{content}");
                if !content.ends_with('\n') {
                    println!();
                }
            }
            Command::Thread { id, limit } => {
                let leaf = resolve_message_id(&catalog, &id)?;
                let mut chain = Vec::new();
                let mut seen = HashSet::new();
                let mut current = leaf;

                for _ in 0..limit {
                    if !seen.insert(current) {
                        break;
                    }
                    chain.push(current);
                    let parent = find!(
                        (parent: Id),
                        pattern!(&catalog, [{ current @ common::archive::reply_to: ?parent }])
                    )
                    .into_iter()
                    .next()
                    .map(|(p,)| p);
                    let Some(parent) = parent else { break };
                    current = parent;
                }

                chain.reverse();
                for message_id in chain {
                    let (message_id, name, role, created_at, content_handle, _reply_to) =
                        message_record(&mut ws, &catalog, message_id)?;
                    let content = load_longstring(&mut ws, content_handle)?;
                    let (lower, _upper): (Epoch, Epoch) = created_at.from_value();
                    let role = role.as_deref().unwrap_or("");
                    println!(
                        "{} {} {} {}",
                        &format!("{message_id:x}")[..8],
                        lower,
                        if role.is_empty() { name } else { format!("{name} ({role})") },
                        snippet(&content, 120)
                    );
                }
            }
            Command::Search {
                text,
                limit,
                case_sensitive,
            } => {
                let needle = if case_sensitive {
                    text.clone()
                } else {
                    text.to_lowercase()
                };

                let mut matches = Vec::new();
                for (message_id, author_id, content_handle, created_at) in find!(
                    (
                        message: Id,
                        author: Id,
                        content: Value<Handle<Blake3, LongString>>,
                        created_at: Value<NsTAIInterval>
                    ),
                    pattern!(&catalog, [{
                        ?message @
                            common::archive::kind: common::archive::kind_message,
                            common::archive::author: ?author,
                            common::archive::content: ?content,
                            common::archive::created_at: ?created_at,
                    }])
                ) {
                    let content = load_longstring(&mut ws, content_handle)?;
                    let haystack = if case_sensitive {
                        content.clone()
                    } else {
                        content.to_lowercase()
                    };
                    if haystack.contains(&needle) {
                        matches.push((interval_key(created_at), message_id, author_id, created_at, content));
                    }
                }
                matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                for (_key, message_id, author_id, created_at, content) in matches.into_iter().rev().take(limit) {
                    let name = author_name(&mut ws, &catalog, author_id)?;
                    let role = author_role(&mut ws, &catalog, author_id)?;
                    let (lower, _upper): (Epoch, Epoch) = created_at.from_value();
                    let role = role.as_deref().unwrap_or("");
                    println!(
                        "{} {} {} {}",
                        &format!("{message_id:x}")[..8],
                        lower,
                        if role.is_empty() { name } else { format!("{name} ({role})") },
                        snippet(&content, 120)
                    );
                }
            }
            Command::Imports { format, limit } => {
                let format_filter = format.map(|s| s.to_lowercase());

                let mut conversations = Vec::new();
                for (conversation_id, source_format, source_conversation_id_handle) in find!(
                    (
                        conversation: Id,
                        format: String,
                        source_conversation_id: Value<Handle<Blake3, LongString>>
                    ),
                    pattern!(&catalog, [{
                        ?conversation @
                            common::import_schema::kind: common::import_schema::kind_conversation,
                            common::import_schema::source_format: ?format,
                            common::import_schema::source_conversation_id: ?source_conversation_id,
                    }])
                ) {
                    if let Some(filter) = format_filter.as_deref() {
                        if source_format.to_lowercase() != filter {
                            continue;
                        }
                    }
                    let source_conversation_id =
                        load_longstring(&mut ws, source_conversation_id_handle)?;
                    conversations.push((conversation_id, source_format, source_conversation_id));
                }

                conversations.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
                for (conversation_id, source_format, source_conversation_id) in
                    conversations.into_iter().take(limit)
                {
                    println!(
                        "{} {} convo={}",
                        &format!("{conversation_id:x}")[..8],
                        source_format,
                        source_conversation_id
                    );
                }
            }
        }

        Ok(())
    })();

    let close_result = repo
        .close()
        .map_err(|e| anyhow!("close pile {}: {e:?}", pile_path.display()));

    match (res, close_result) {
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(err),
        (Ok(()), Ok(())) => Ok(()),
    }
}
