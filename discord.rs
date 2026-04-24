#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive", "env"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4.2.3"
//! rand_core = "0.6.4"
//! reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls", "json"] }
//! serde_json = "1"
//! triblespace = "0.36"
//! # Path dep until the `discord` schema module ships in a published
//! # `faculties` crate release. Once bumped + published, switch to
//! # `faculties = "0.12"` (or whatever carries the discord module).
//! faculties = { version = "0.11", path = "/Users/jp/Desktop/chatbot/liora/faculties" }
//! ```
//!
//! # Discord faculty
//!
//! Ingests Discord channel messages into a TribleSpace pile and
//! posts new messages on request. Bot-token only — no OAuth2 dance.
//! Paste the token once (`discord login @token.txt`); it's cached
//! in the pile under a `kind_token` entity and reused on every
//! subsequent call.
//!
//! Messages land on the `discord` branch using the generic
//! `archive::*` schema (author / content / reply_to / kind_message)
//! so downstream consumers don't care which protocol they came
//! from. Discord-specific context (guild, channel, external
//! snowflakes, raw JSON) lives under the `discord::*` attributes.
//!
//! Entity ids are derived intrinsically from the external
//! snowflake via the identity-only-fragment idiom — re-ingesting
//! the same message collapses to the same entity, so edits and
//! re-runs are idempotent.
//!
//! ## MVP scope
//!
//! - `discord login <token>` — cache bot token in the pile.
//! - `discord send <channel_id> <text>` — POST a message.
//! - `discord read <channel_id>` — GET recent messages, ingest
//!   into the pile (per-channel cursor stored as
//!   `discord::cursor_last_message_id` so successive calls are
//!   incremental).
//!
//! Not yet implemented: attachments, gateway websocket for
//! real-time events, guild/channel listing. All straightforward
//! follow-ups; kept out of the MVP so the first working cut is
//! small.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use reqwest::blocking::Client;
use serde_json::{Value as JsonValue, json};

use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::valueschemas::{Blake3, Handle, NsTAIInterval};
use triblespace::prelude::*;

use faculties::schemas::archive::archive;
use faculties::schemas::discord::{DEFAULT_BRANCH, DEFAULT_LOG_BRANCH, discord};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

#[derive(Parser)]
#[command(
    name = "discord",
    about = "Post to and ingest Discord channels into TribleSpace"
)]
struct Cli {
    /// Path to the pile file to write into.
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Branch name to write into (created if missing).
    #[arg(long, default_value = DEFAULT_BRANCH)]
    branch: String,
    /// Branch id (hex). Overrides `--branch`.
    #[arg(long)]
    branch_id: Option<String>,
    #[command(subcommand)]
    command: Option<CommandMode>,
}

#[derive(Subcommand)]
enum CommandMode {
    /// Cache a Discord bot token in the pile. Subsequent calls
    /// read it from the pile — no need to re-pass it.
    Login {
        /// Bot token (from the Discord developer portal). Use
        /// `@path` to read from a file or `@-` for stdin.
        token: String,
    },
    /// Post a message to a Discord channel.
    Send {
        /// Channel id (external Discord snowflake).
        channel_id: String,
        /// Message body. Use `@path` / `@-` for file / stdin.
        text: String,
    },
    /// Pull recent messages from a channel into the pile.
    /// Successive runs are incremental — a per-channel cursor
    /// records the newest message already ingested.
    Read {
        /// Channel id (external Discord snowflake).
        channel_id: String,
        /// Max messages to fetch per call (1–100, Discord's cap).
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

#[derive(Clone, Debug)]
struct DiscordConfig {
    pile_path: PathBuf,
    #[allow(dead_code)]
    branch: String,
    branch_id: Id,
    log_branch_id: Id,
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    let Some(mode) = cli.command.take() else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };
    let config = build_config(&cli)?;

    match mode {
        CommandMode::Login { token } => login(config, token),
        CommandMode::Send { channel_id, text } => send(config, channel_id, text),
        CommandMode::Read { channel_id, limit } => read(config, channel_id, limit),
    }
}

// ── config / pile plumbing ───────────────────────────────────────

fn build_config(cli: &Cli) -> Result<DiscordConfig> {
    let pile_path = cli.pile.clone();
    let branch = cli.branch.clone();
    let log_branch = DEFAULT_LOG_BRANCH.to_string();
    let branch_id = with_repo(&pile_path, |repo| {
        if let Some(hex) = cli.branch_id.as_deref() {
            return Id::from_hex(hex.trim())
                .ok_or_else(|| anyhow!("invalid branch id '{hex}'"));
        }
        repo.ensure_branch(&branch, None)
            .map_err(|e| anyhow!("ensure discord branch: {e:?}"))
    })?;
    let log_branch_id = with_repo(&pile_path, |repo| {
        repo.ensure_branch(&log_branch, None)
            .map_err(|e| anyhow!("ensure logs branch: {e:?}"))
    })?;
    Ok(DiscordConfig {
        pile_path,
        branch,
        branch_id,
        log_branch_id,
    })
}

fn open_pile(path: &PathBuf) -> Result<Pile<Blake3>> {
    Pile::open(path).with_context(|| format!("open pile {}", path.display()))
}

fn with_repo<T>(
    pile_path: &PathBuf,
    f: impl FnOnce(&mut Repository<Pile<Blake3>>) -> Result<T>,
) -> Result<T> {
    let pile = open_pile(pile_path)?;
    let repo = Repository::new(pile, SigningKey::generate(&mut OsRng), TribleSet::new())
        .map_err(|e| anyhow!("create repository: {e:?}"))?;
    with_repo_close(repo, f)
}

fn with_repo_close<T, F>(repo: Repository<Pile<Blake3>>, f: F) -> Result<T>
where
    F: FnOnce(&mut Repository<Pile<Blake3>>) -> Result<T>,
{
    let mut repo = repo;
    let result = f(&mut repo);
    let pile = repo.into_storage();
    let close_res = pile.close().map_err(|e| anyhow!("close pile: {e:?}"));
    if let Err(err) = close_res {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }
    result
}

fn log_event(config: &DiscordConfig, level: &str, message: &str) -> Result<()> {
    with_repo(&config.pile_path, |repo| {
        let mut ws = repo
            .pull(config.log_branch_id)
            .map_err(|e| anyhow!("pull logs: {e:?}"))?;
        let level_handle = ws.put(level.to_string());
        let message_handle = ws.put(message.to_string());
        let change = entity! { _ @
            metadata::tag: discord::kind_log,
            metadata::created_at: epoch_interval(now_epoch()),
            archive::author_role: level_handle,
            archive::content: message_handle,
        };
        ws.commit(change, &format!("discord {level}"));
        repo.push(&mut ws).map_err(|e| anyhow!("push logs: {e:?}"))?;
        Ok(())
    })
}

// ── token cache ──────────────────────────────────────────────────

fn login(config: DiscordConfig, raw_token: String) -> Result<()> {
    let token = load_value_or_file_trimmed(&raw_token, "token")?;
    if token.is_empty() {
        bail!("empty token");
    }

    with_repo(&config.pile_path, |repo| {
        let mut ws = repo
            .pull(config.branch_id)
            .map_err(|e| anyhow!("pull discord: {e:?}"))?;
        let catalog = ws
            .checkout(..)
            .map_err(|e| anyhow!("checkout discord: {e:?}"))?
            .into_facts();

        // Identity fragment for the bot-token entity — keyed on
        // `kind_token` alone, so there's exactly one token entity
        // per pile. Re-running `login` updates the token on the
        // same entity rather than minting a new one.
        let id_frag = entity! { _ @ metadata::tag: discord::kind_token };
        let token_id = id_frag.root().ok_or_else(|| anyhow!("token id rooted"))?;

        let token_handle = ws.put(token);
        let mut change = id_frag;
        change += entity! { ExclusiveId::force_ref(&token_id) @
            discord::bot_token: token_handle,
        };

        let delta = change.difference(&catalog);
        if !delta.is_empty() {
            ws.commit(delta, "discord: store bot token");
            repo.push(&mut ws)
                .map_err(|e| anyhow!("push discord: {e:?}"))?;
        }
        Ok(())
    })?;

    log_event(&config, "info", "bot token cached")?;
    println!("Token cached in pile.");
    Ok(())
}

fn load_bot_token(config: &DiscordConfig) -> Result<String> {
    let token: Result<Option<String>> = with_repo(&config.pile_path, |repo| {
        let mut ws = repo
            .pull(config.branch_id)
            .map_err(|e| anyhow!("pull discord: {e:?}"))?;
        let catalog = ws
            .checkout(..)
            .map_err(|e| anyhow!("checkout discord: {e:?}"))?
            .into_facts();
        // Find any (entity, bot_token-handle) pair. There's at
        // most one by construction (kind_token is intrinsic-id'd
        // from just the tag).
        for (_tok, handle) in find!(
            (tok: Id, handle: Value<Handle<Blake3, LongString>>),
            pattern!(&catalog, [{
                ?tok @
                metadata::tag: discord::kind_token,
                discord::bot_token: ?handle,
            }])
        ) {
            let view: View<str> = ws
                .get(handle)
                .map_err(|e| anyhow!("get token bytes: {e:?}"))?;
            return Ok(Some(view.to_string()));
        }
        Ok(None)
    });
    token?.ok_or_else(|| anyhow!("no bot token cached; run `discord login <token>` first"))
}

// ── send ─────────────────────────────────────────────────────────

fn send(config: DiscordConfig, channel_id: String, raw_text: String) -> Result<()> {
    let token = load_bot_token(&config)?;
    let text = load_value_or_file(&raw_text, "message text")?;
    if text.trim().is_empty() {
        bail!("empty message body");
    }

    let client = build_client()?;
    let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bot {token}"))
        .header("Content-Type", "application/json")
        .body(json!({ "content": text }).to_string())
        .send()
        .with_context(|| format!("POST {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("discord send failed ({status}): {body}");
    }

    let json: JsonValue = resp.json().context("parse send response")?;
    let message_id = json
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    log_event(
        &config,
        "info",
        &format!("sent message {message_id} to channel {channel_id}"),
    )?;
    println!("Sent message {message_id} to channel {channel_id}");
    Ok(())
}

// ── read ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct IncomingMessage {
    external_id: String,
    raw_json: String,
    channel_external_id: String,
    author_external_id: String,
    author_display_name: String,
    content: String,
    created_at: Value<NsTAIInterval>,
    reply_to_external_id: Option<String>,
}

fn read(config: DiscordConfig, channel_id: String, limit: u32) -> Result<()> {
    let token = load_bot_token(&config)?;
    let limit = limit.clamp(1, 100);

    // Fetch cursor (last ingested snowflake) for this channel.
    let cursor = load_channel_cursor(&config, &channel_id)?;

    // Call Discord REST. `after=<id>` returns messages newer than
    // `id` in *oldest-first* order (despite what the docs initially
    // suggest; verify with the response). If no cursor yet, omit —
    // you'll get the most-recent `limit` messages in reverse order,
    // which we'll normalise below.
    let client = build_client()?;
    let mut url = format!(
        "{DISCORD_API_BASE}/channels/{channel_id}/messages?limit={limit}"
    );
    if let Some(c) = cursor.as_deref() {
        url.push_str(&format!("&after={c}"));
    }
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bot {token}"))
        .send()
        .with_context(|| format!("GET {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("discord read failed ({status}): {body}");
    }
    let messages: Vec<JsonValue> = resp.json().context("parse read response")?;

    if messages.is_empty() {
        println!("No new messages in channel {channel_id}.");
        return Ok(());
    }

    let incoming = parse_messages(messages, &channel_id)?;

    // ── commit ──
    let ingested = incoming.len();
    let newest_snowflake = incoming
        .iter()
        .map(|m| m.external_id.clone())
        .max_by(|a, b| compare_snowflakes(a, b));

    with_repo(&config.pile_path, |repo| {
        let mut ws = repo
            .pull(config.branch_id)
            .map_err(|e| anyhow!("pull discord: {e:?}"))?;
        let catalog = ws
            .checkout(..)
            .map_err(|e| anyhow!("checkout discord: {e:?}"))?
            .into_facts();

        let change = build_ingest_change(&mut ws, &catalog, incoming)?;
        let delta = change.difference(&catalog);
        if !delta.is_empty() {
            ws.commit(
                delta,
                &format!("discord: ingest {ingested} messages from {channel_id}"),
            );
            repo.push(&mut ws)
                .map_err(|e| anyhow!("push discord: {e:?}"))?;
        }
        Ok(())
    })?;

    if let Some(snowflake) = newest_snowflake {
        store_channel_cursor(&config, &channel_id, &snowflake)?;
    }

    log_event(
        &config,
        "info",
        &format!("ingested {ingested} messages from channel {channel_id}"),
    )?;
    println!("Ingested {ingested} messages from channel {channel_id}.");
    Ok(())
}

fn parse_messages(messages: Vec<JsonValue>, channel_external_id: &str) -> Result<Vec<IncomingMessage>> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        let raw_json = serde_json::to_string(&message).context("serialize message json")?;
        let external_id = message
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("message missing id"))?
            .to_string();
        let content = message
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let author = message
            .get("author")
            .cloned()
            .unwrap_or(JsonValue::Null);
        let author_external_id = author
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let author_display_name = author
            .get("global_name")
            .and_then(|v| v.as_str())
            .or_else(|| author.get("username").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let timestamp_str = message
            .get("timestamp")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("message {external_id} missing timestamp"))?;
        let created_at = parse_iso8601(timestamp_str)
            .with_context(|| format!("parse timestamp {timestamp_str}"))?;
        let reply_to_external_id = message
            .get("referenced_message")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        out.push(IncomingMessage {
            external_id,
            raw_json,
            channel_external_id: channel_external_id.to_string(),
            author_external_id,
            author_display_name,
            content,
            created_at,
            reply_to_external_id,
        });
    }
    // Normalise to oldest-first for stable `reply_to` resolution.
    out.sort_by(|a, b| compare_snowflakes(&a.external_id, &b.external_id));
    Ok(out)
}

fn build_ingest_change(
    ws: &mut Workspace<Pile<Blake3>>,
    _catalog: &TribleSet,
    messages: Vec<IncomingMessage>,
) -> Result<TribleSet> {
    let mut change = TribleSet::new();

    // Resolve each external id (channel, author, message) to an
    // intrinsic Id via the identity-only-fragment idiom. Cached
    // across this batch so repeated references hit the same id.
    let mut channel_ids: HashMap<String, Id> = HashMap::new();
    let mut author_ids: HashMap<String, Id> = HashMap::new();
    let mut message_ids: HashMap<String, Id> = HashMap::new();

    for message in &messages {
        // ── channel ──
        if !channel_ids.contains_key(&message.channel_external_id) {
            let external_handle = ws.put(message.channel_external_id.clone());
            let id_frag = entity! { _ @
                discord::channel_id: external_handle,
            };
            let channel_id = id_frag
                .root()
                .ok_or_else(|| anyhow!("channel id rooted"))?;
            change += id_frag;
            change += entity! { ExclusiveId::force_ref(&channel_id) @
                metadata::tag: discord::kind_channel,
            };
            channel_ids.insert(message.channel_external_id.clone(), channel_id);
        }
        // ── author ──
        if !author_ids.contains_key(&message.author_external_id) {
            let external_handle = ws.put(message.author_external_id.clone());
            let id_frag = entity! { _ @
                discord::user_id: external_handle,
            };
            let author_id = id_frag
                .root()
                .ok_or_else(|| anyhow!("author id rooted"))?;
            change += id_frag;
            let mut author_facts = entity! { ExclusiveId::force_ref(&author_id) @
                metadata::tag: archive::kind_author,
            };
            if !message.author_display_name.is_empty() {
                let name_handle = ws.put(message.author_display_name.clone());
                author_facts += entity! { ExclusiveId::force_ref(&author_id) @
                    archive::author_name: name_handle,
                };
            }
            change += author_facts;
            author_ids.insert(message.author_external_id.clone(), author_id);
        }
    }

    // ── messages (second pass, so reply_to can resolve predecessors from this batch) ──
    for message in &messages {
        let external_handle = ws.put(message.external_id.clone());
        let id_frag = entity! { _ @
            discord::message_id: external_handle,
        };
        let message_id = id_frag
            .root()
            .ok_or_else(|| anyhow!("message id rooted"))?;
        message_ids.insert(message.external_id.clone(), message_id);

        let content_handle = ws.put(message.content.clone());
        let raw_handle = ws.put(message.raw_json.clone());
        let channel_id = channel_ids[&message.channel_external_id];
        let author_id = author_ids[&message.author_external_id];
        let reply_to = message
            .reply_to_external_id
            .as_ref()
            .and_then(|ext| message_ids.get(ext).copied());

        change += id_frag;
        change += entity! { ExclusiveId::force_ref(&message_id) @
            metadata::tag: archive::kind_message,
            archive::author: author_id,
            archive::content: content_handle,
            metadata::created_at: message.created_at,
            discord::channel: channel_id,
            discord::message_raw: raw_handle,
            archive::reply_to?: reply_to,
        };
    }

    Ok(change)
}

// ── per-channel cursor ───────────────────────────────────────────

fn load_channel_cursor(config: &DiscordConfig, channel_external_id: &str) -> Result<Option<String>> {
    with_repo(&config.pile_path, |repo| {
        let mut ws = repo
            .pull(config.branch_id)
            .map_err(|e| anyhow!("pull discord: {e:?}"))?;
        let catalog = ws
            .checkout(..)
            .map_err(|e| anyhow!("checkout discord: {e:?}"))?
            .into_facts();

        // Cursor id is the root of `{kind_cursor, channel_id=X}`.
        let external_handle = ws.put(channel_external_id.to_string());
        let id_frag = entity! { _ @
            metadata::tag: discord::kind_cursor,
            discord::channel_id: external_handle,
        };
        let cursor_id = id_frag
            .root()
            .ok_or_else(|| anyhow!("cursor id rooted"))?;

        for (_cur, handle) in find!(
            (cur: Id, handle: Value<Handle<Blake3, LongString>>),
            pattern!(&catalog, [{
                ?cur @
                metadata::tag: discord::kind_cursor,
                discord::cursor_last_message_id: ?handle,
            }])
        ) {
            // find! doesn't let us filter by cursor_id in-macro
            // without rebinding, so check match manually:
            if _cur == cursor_id {
                let view: View<str> = ws
                    .get(handle)
                    .map_err(|e| anyhow!("get cursor: {e:?}"))?;
                return Ok(Some(view.to_string()));
            }
        }
        Ok(None)
    })
}

fn store_channel_cursor(
    config: &DiscordConfig,
    channel_external_id: &str,
    snowflake: &str,
) -> Result<()> {
    with_repo(&config.pile_path, |repo| {
        let mut ws = repo
            .pull(config.branch_id)
            .map_err(|e| anyhow!("pull discord: {e:?}"))?;
        let catalog = ws
            .checkout(..)
            .map_err(|e| anyhow!("checkout discord: {e:?}"))?
            .into_facts();

        let external_handle = ws.put(channel_external_id.to_string());
        let id_frag = entity! { _ @
            metadata::tag: discord::kind_cursor,
            discord::channel_id: external_handle,
        };
        let cursor_id = id_frag
            .root()
            .ok_or_else(|| anyhow!("cursor id rooted"))?;

        let snowflake_handle = ws.put(snowflake.to_string());
        let mut change = id_frag;
        change += entity! { ExclusiveId::force_ref(&cursor_id) @
            discord::cursor_last_message_id: snowflake_handle,
        };

        let delta = change.difference(&catalog);
        if !delta.is_empty() {
            ws.commit(
                delta,
                &format!("discord: update cursor for {channel_external_id}"),
            );
            repo.push(&mut ws)
                .map_err(|e| anyhow!("push discord: {e:?}"))?;
        }
        Ok(())
    })
}

// ── helpers ──────────────────────────────────────────────────────

fn build_client() -> Result<Client> {
    Client::builder()
        .user_agent("triblespace-discord/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .context("build reqwest client")
}

/// Compare two Discord snowflakes numerically without parsing —
/// they're fixed-width u64 strings, so lexicographic works as long
/// as the strings are equal-length. Discord snowflakes are all
/// 17–19 digits; compare by (length, string) to get numeric order.
fn compare_snowflakes(a: &str, b: &str) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn parse_iso8601(value: &str) -> Result<Value<NsTAIInterval>> {
    // Discord timestamps look like `2026-04-22T09:12:34.567000+00:00`.
    // hifitime's Epoch::from_gregorian_str handles RFC3339.
    let epoch = Epoch::from_gregorian_str(value)
        .map_err(|e| anyhow!("parse iso8601 '{value}': {e}"))?;
    Ok(epoch_interval(epoch))
}

fn now_epoch() -> Epoch {
    Epoch::now().unwrap_or(Epoch::from_gregorian_tai_at_midnight(2026, 1, 1))
}

fn epoch_interval(epoch: Epoch) -> Value<NsTAIInterval> {
    (epoch, epoch).try_to_value().unwrap()
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
        return fs::read_to_string(path)
            .with_context(|| format!("read {label} from {path}"));
    }
    Ok(raw.to_string())
}

fn load_value_or_file_trimmed(raw: &str, label: &str) -> Result<String> {
    Ok(load_value_or_file(raw, label)?.trim().to_string())
}
