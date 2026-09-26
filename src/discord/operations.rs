//! Discord observation faculty.
//!
//! The faculty stores complete, immutable message observations in one
//! SimpleArchive-union collection. Discord snowflakes identify stable anchors;
//! mutable payloads never accumulate conflicting values on those anchors.
//! Replaying an identical REST payload converges on the same intrinsic
//! observation, while an edit creates a new observation linked to the same
//! message anchor.
//!
//! Forward progress is represented by immutable numeric intervals. The first
//! bounded import establishes an explicit baseline immediately before its
//! oldest returned message, or at a floor its caller names. Later reads page
//! forward from the connected frontier, a bounded number of pages per pull,
//! before publishing one interval in the same signed COMMIT as every message
//! and attachment it covers. A bounded recent-window fetch also reconciles
//! edits. The REST API cannot prove deletions or edits outside that window;
//! future Gateway tombstones can be modeled as another immutable observation
//! kind.
//!
//! An attachment is stored with its bytes, and one Discord declares larger
//! than [`MAX_ATTACHMENT_BYTES`] by its name and size alone, so a single
//! large file can never hold a channel's ingestion back. A system notice (a
//! pin, a member joining) is stored like any message and marked as one.
//!
//! Bot credentials are deliberately external input. This faculty neither
//! claims the historical shared logs branch nor stores mutable secrets in the
//! logical Discord dataset.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use hifitime::{Epoch, TimeScale};
use reqwest::blocking::Client;
use serde_json::{json, Value as JsonValue};

use crate::collection_names::{open_configured, open_exact_in};
use crate::discord as discord_model;
use crate::files as file_capability;
use crate::schemas::archive::archive;
use crate::schemas::discord::{discord, DEFAULT_SCOPE_ID};
use crate::storage::FactArchive;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{
    records::CollectionHandle, Collection, CollectionCommit, CollectionSnapshotExt,
    CollectionStoreExt,
};
use triblespace::core::metadata;
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
/// Attachments Discord declares larger than this are recorded by name and
/// size, without their bytes.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// How long one attachment download may take.
const ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(120);
/// Forward pages one pull reads at most; a longer gap is closed by the pulls
/// after it, each going on from the frontier the one before proved.
const FORWARD_PAGES: usize = 10;
/// Message types somebody writes: default, reply, and the two kinds of
/// application command. Every other type is a system notice.
const WRITTEN_MESSAGE_TYPES: [u64; 4] = [0, 19, 20, 23];

#[derive(Clone, Debug)]
pub struct SendReceipt {
    pub message_id: String,
    pub channel_id: String,
    pub commit: CollectionCommit,
}
#[derive(Clone, Debug)]
pub struct PullOptions {
    pub channel_id: Option<String>,
    pub fetch_limit: u32,
    pub reconcile_limit: u32,
}
impl Default for PullOptions {
    fn default() -> Self {
        Self {
            channel_id: None,
            fetch_limit: 100,
            reconcile_limit: 50,
        }
    }
}
impl PullOptions {
    pub fn validate(&self) -> Result<()> {
        if let Some(channel) = &self.channel_id {
            discord_model::validate_snowflake(channel)?;
        }
        if !(1..=100).contains(&self.fetch_limit) || !(1..=100).contains(&self.reconcile_limit) {
            bail!("fetch_limit and reconcile_limit must each be between 1 and 100");
        }
        Ok(())
    }
}
impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            channel_id: None,
            since: None,
            limit: 20,
            descending: false,
        }
    }
}
impl ReadOptions {
    pub fn validate(&self) -> Result<()> {
        if let Some(channel) = &self.channel_id {
            discord_model::validate_snowflake(channel)?;
        }
        if let Some(since) = &self.since {
            parse_iso8601(since.trim())?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct ChannelReceipt {
    pub channel_id: String,
    pub observations: usize,
    pub coverage: Option<discord_model::CoverageInterval>,
    pub commit: Option<CollectionCommit>,
    /// The forward read stopped at its page budget with more to read: pull
    /// again to go on.
    pub more: bool,
}
#[derive(Clone, Debug)]
pub struct ChannelPull {
    pub channel_id: String,
    pub name: String,
    pub result: std::result::Result<ChannelReceipt, String>,
}
#[derive(Clone, Debug)]
pub struct PullReport {
    pub all_visible: bool,
    pub guilds: usize,
    pub channels: Vec<ChannelPull>,
}
#[derive(Clone, Debug)]
pub struct ObservedMessage {
    pub observation: Id,
    pub anchor: Id,
    pub created_at: Inline<NsTAIInterval>,
    pub edited_at: Option<Inline<NsTAIInterval>>,
    pub channel: Id,
    pub channel_name: Option<String>,
    pub author: String,
    pub content: String,
    pub reply_to: Option<Id>,
    pub attachments: BTreeSet<Id>,
    pub variant_index: usize,
    pub variant_count: usize,
}
#[derive(Clone, Debug)]
pub struct History {
    pub channel_id: Option<String>,
    pub messages: Vec<ObservedMessage>,
}
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub kind: i64,
}
#[derive(Clone, Debug)]
pub struct GuildChannels {
    pub id: String,
    pub name: String,
    pub channels: Vec<Channel>,
}

#[derive(Clone, Debug)]
pub struct ChannelListing {
    pub bot_in_any_guild: bool,
    pub guilds: Vec<GuildChannels>,
}

/// Direct Discord operations with explicit, host-configured bot credentials.
/// Resident reads require no token. Neither configuration nor commands consult the environment.
#[derive(Clone)]
pub struct Discord {
    storage: crate::storage::Storage,
    token: Option<String>,
}
impl std::fmt::Debug for Discord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discord")
            .field("storage", &self.storage)
            .field("token_configured", &self.token.is_some())
            .finish()
    }
}
impl Discord {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            storage,
            token: None,
        }
    }
    /// Accept resident secret bytes from a trusted launcher, never a path or MCP tool argument.
    pub fn with_token(mut self, token: String) -> Self {
        self.token = Some(token);
        self
    }
    fn storage(&self) -> DiscordStorage<'_> {
        DiscordStorage {
            storage: &self.storage,
            collection: None,
        }
    }
    fn token(&self) -> Result<&str> {
        self.token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| anyhow!("missing Discord bot token in trusted host configuration"))
    }
    /// Post literal text, after proving collection WRITE admission, then persist the returned observation.
    pub fn send(&self, channel_id: &str, text: &str) -> Result<SendReceipt> {
        discord_model::validate_snowflake(channel_id)?;
        if text.trim().is_empty() {
            bail!("empty message body");
        }
        send_with(
            self.storage(),
            self.token()?,
            channel_id,
            text,
            post_message,
        )
    }
    /// Observe the resident collection; this never lists guilds, fetches messages, or opens credentials.
    pub fn read(&self, options: ReadOptions) -> Result<History> {
        options.validate()?;
        self.storage()
            .with_session(|session| read_history(&session.view(), &options))
    }
    /// Pull one complete forward interval plus bounded recent edits, optionally for every visible channel.
    /// Per-channel failures in an all-visible pull are retained explicitly in the report.
    pub fn pull(&self, options: PullOptions) -> Result<PullReport> {
        options.validate()?;
        pull(self.storage(), self.token()?, options)
    }
    /// Pull one channel as [`Self::pull`] does, reading Discord through
    /// `source`. `floor` says where coverage begins when the channel has
    /// none: the pull then reads forward from just after that message id
    /// instead of taking the newest page as a bounded baseline, so nothing
    /// older than the floor is stored.
    pub fn pull_channel(
        &self,
        channel_id: &str,
        floor: Option<u64>,
        source: &mut dyn Source,
    ) -> Result<ChannelReceipt> {
        let options = PullOptions {
            channel_id: Some(channel_id.to_owned()),
            ..PullOptions::default()
        };
        options.validate()?;
        let storage = self.storage();
        storage.storage.scope(|owner| {
            let storage = DiscordStorage {
                storage: owner,
                collection: storage.collection,
            };
            let mut view = storage.with_session(|session| Ok(session.view()))?;
            pull_channel(
                storage,
                &mut view,
                source,
                channel_id,
                floor,
                options.fetch_limit,
                options.reconcile_limit,
            )
        })
    }
    /// Store one message object as Discord delivered it, over the gateway
    /// (MESSAGE_CREATE, or a complete MESSAGE_UPDATE) or from REST: exactly
    /// the observation a pull stores for the same message, attachments
    /// included, so a replayed event or an overlapping pull converges on it.
    /// Coverage is untouched; only a pull proves an interval complete.
    pub fn observe(&self, message: JsonValue, source: &mut dyn Source) -> Result<CollectionCommit> {
        let (fragment, message_id, channel_id) =
            message_fragment(message, |url, size| source.attachment(url, size))?;
        self.storage().publish(
            fragment,
            format!("discord: observed message {message_id} in channel {channel_id}"),
        )
    }
    /// Record the Discord user a bot token of this pile authenticates as, so
    /// readers can tell the pile's own messages from everyone else's.
    pub fn record_bot_account(&self, user_id: &str) -> Result<CollectionCommit> {
        self.storage().publish(
            discord_model::bot_account_fragment(user_id)?,
            format!("discord: bot account is user {user_id}"),
        )
    }
    /// Prove that this process can publish to the Discord collection.
    pub fn preflight_write(&self) -> Result<()> {
        self.storage().preflight_write()
    }
    pub fn channels_list(&self, guild: Option<&str>) -> Result<ChannelListing> {
        if let Some(guild) = guild {
            discord_model::validate_snowflake(guild)?;
        }
        list_channels(self.token()?, guild)
    }
}

#[derive(Clone, Copy)]
struct DiscordStorage<'a> {
    storage: &'a crate::storage::Storage,
    collection: Option<CollectionHandle>,
}

#[derive(Clone)]
struct CollectionView {
    facts: FactArchive,
    reader: PileSnapshot,
}

struct DiscordSession<'a> {
    pile: &'a mut Pile,
    collection: Collection<SimpleArchive>,
    rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
    signer: SigningKey,
    facts: FactArchive,
    reader: PileSnapshot,
}

impl DiscordSession<'_> {
    fn view(&self) -> CollectionView {
        CollectionView {
            facts: self.facts.clone(),
            reader: self.reader.clone(),
        }
    }

    fn commit(&mut self, mut fragment: Fragment, description: String) -> Result<CollectionCommit> {
        fragment.describe_with(entity! { metadata::description: description });
        let commit = self
            .pile
            .commit(self.collection, &self.signer, fragment)
            .context("publish Discord collection fragment")?;
        // Like every other write: derive this key's own leaves into the
        // views, and refuse to call the commit done if no reader can see it.
        // Merges are the maintenance daemon's.
        drop(
            pollster::block_on(crate::storage::ensure_downstream(
                self.pile,
                self.collection,
                &self.signer,
            ))
            .context("Discord fragment was committed, but ensuring its derived views failed")?,
        );
        self.reader = self
            .pile
            .snapshot()
            .context("freeze Discord fact collection after commit")?;
        self.facts = self
            .reader
            .collection(self.rank9)
            .context("observe maintained Discord fact collection after commit")?
            .view::<FactArchive>()
            .context("read maintained Discord fact collection after commit")?;
        Ok(commit)
    }
}

impl DiscordStorage<'_> {
    fn open_collection(
        &self,
        pile: &mut Pile,
        authority: VerifyingKey,
    ) -> Result<Collection<SimpleArchive>> {
        let Some(handle) = self.collection else {
            return open_configured(pile, DEFAULT_SCOPE_ID, authority);
        };
        let snapshot = pile
            .snapshot()
            .context("freeze store while opening exact Discord collection")?;
        open_exact_in(&snapshot, DEFAULT_SCOPE_ID, handle)
    }

    /// Prove that this process can publish to the selected collection before
    /// an outbound Discord side effect occurs.
    fn preflight_write(&self) -> Result<()> {
        self.storage.with_pile(|pile, signer| {
            let result = (|| {
                let collection = self.open_collection(pile, signer.verifying_key())?;
                let snapshot = pile
                    .snapshot()
                    .context("freeze Discord WRITE-admission preflight")?;
                if !collection
                    .writer_is_admitted(&snapshot, signer.verifying_key())
                    .context("check Discord collection WRITE admission")?
                {
                    bail!("durable signer is not admitted to WRITE the Discord collection");
                }
                Ok(())
            })();
            result
        })
    }

    fn with_session<T>(
        &self,
        operation: impl FnOnce(&mut DiscordSession<'_>) -> Result<T>,
    ) -> Result<T> {
        self.storage.with_pile(|pile, signer| {
            let result = (|| {
                let collection = self.open_collection(pile, signer.verifying_key())?;
                let descriptor_snapshot = pile.snapshot()?;
                let policy = collection.policy(&descriptor_snapshot)?;
                drop(descriptor_snapshot);
                let maintained_succinct =
                    pile.derive::<SuccinctArchiveBlob>(collection, (), policy.clone())?;
                let maintained_rank9 = pile.derive::<Rank9AcceleratedSuccinctArchiveBlob>(
                    maintained_succinct,
                    (),
                    policy,
                )?;
                let store_snapshot = pile
                    .snapshot()
                    .context("freeze resident Discord fact collection")?;
                let facts = store_snapshot
                    .collection(maintained_rank9)
                    .context("observe maintained Discord fact collection")?
                    .view::<FactArchive>()
                    .context("read maintained Discord fact collection")?;
                operation(&mut DiscordSession {
                    pile,
                    collection,
                    rank9: maintained_rank9,
                    signer: signer.clone(),
                    facts,
                    reader: store_snapshot,
                })
            })();
            result
        })
    }

    #[cfg(test)]
    fn view(&self) -> Result<CollectionView> {
        self.with_session(|session| Ok(session.view()))
    }

    fn publish(&self, fragment: Fragment, description: String) -> Result<CollectionCommit> {
        self.with_session(|session| session.commit(fragment, description))
    }
}

fn send_with(
    storage: DiscordStorage<'_>,
    token: &str,
    channel_id: &str,
    text: &str,
    post: impl FnOnce(&str, &str, &str) -> Result<JsonValue>,
) -> Result<SendReceipt> {
    discord_model::validate_snowflake(channel_id).context("invalid channel id")?;
    if text.trim().is_empty() {
        bail!("empty message body");
    }

    storage
        .preflight_write()
        .context("preflight Discord collection WRITE admission")?;
    let payload = post(token, channel_id, text)?;
    let messages = parse_messages(vec![payload], channel_id)?;
    let message = messages
        .first()
        .ok_or_else(|| anyhow!("Discord send response contained no message"))?;
    let message_id = message.external_id.as_str();
    let mut fragment = build_ingest_fragment(&messages, None, None, fetch_attachment_bytes)?;
    // The bot wrote this message, so its author is the pile's own account.
    fragment += discord_model::bot_account_fragment(&message.author_external_id)?;
    let commit = storage.publish(
        fragment,
        format!("discord: sent and observed message {message_id} in channel {channel_id}"),
    )?;
    Ok(SendReceipt {
        message_id: message_id.to_owned(),
        channel_id: channel_id.to_owned(),
        commit,
    })
}

pub(crate) fn post_message(token: &str, channel_id: &str, text: &str) -> Result<JsonValue> {
    let client = build_client()?;
    let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
    let response = client
        .post(&url)
        .header("Authorization", format!("Bot {token}"))
        .header("Content-Type", "application/json")
        .body(json!({ "content": text }).to_string())
        .send()
        .with_context(|| format!("POST {url}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("discord send failed ({status}): {body}");
    }
    response.json().context("parse send response")
}

#[derive(Debug, Clone)]
pub struct ReadOptions {
    pub channel_id: Option<String>,
    pub since: Option<String>,
    pub limit: usize,
    pub descending: bool,
}

/// Where ingestion reads Discord from: the REST API, or a stand-in under test.
pub trait Source {
    /// One page of a channel's messages, newest first, as Discord's
    /// `GET /channels/{id}/messages` returns it: with `after`, the `limit`
    /// messages right after that id; with `before`, the `limit` right before
    /// it; with neither, the newest `limit`.
    fn page(&mut self, channel_id: &str, request: PageRequest) -> Result<Vec<JsonValue>>;
    /// The bytes of one attachment URL, refused beyond `limit` bytes.
    fn attachment(&mut self, url: &str, limit: u64) -> Result<Vec<u8>>;
}

/// Discord's REST API, as the bot a token names.
pub struct Rest {
    token: String,
}

impl Rest {
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

impl Source for Rest {
    fn page(&mut self, channel_id: &str, request: PageRequest) -> Result<Vec<JsonValue>> {
        fetch_message_page(&self.token, channel_id, request)
    }

    fn attachment(&mut self, url: &str, limit: u64) -> Result<Vec<u8>> {
        fetch_attachment_bytes(url, limit)
    }
}

/// One message object as the fragment ingestion stores for it, with its
/// message and channel ids.
fn message_fragment(
    message: JsonValue,
    fetch: impl FnMut(&str, u64) -> Result<Vec<u8>>,
) -> Result<(Fragment, String, String)> {
    let channel_id = required_snowflake(&message, "channel_id", "message")?;
    let messages = parse_messages(vec![message], &channel_id)?;
    let message_id = messages
        .first()
        .map(|message| message.external_id.clone())
        .expect("one parsed message");
    let fragment = build_ingest_fragment(&messages, None, None, fetch)?;
    Ok((fragment, message_id, channel_id))
}

/// The fragment [`Discord::observe`] stores for one message object.
#[cfg(test)]
pub(crate) fn observed_fragment(
    message: JsonValue,
    fetch: impl FnMut(&str, u64) -> Result<Vec<u8>>,
) -> Result<Fragment> {
    message_fragment(message, fetch).map(|(fragment, _, _)| fragment)
}

fn pull(storage: DiscordStorage<'_>, token: &str, options: PullOptions) -> Result<PullReport> {
    let source = &mut Rest::new(token.to_owned());
    storage.storage.scope(|owner| {
        let storage = DiscordStorage {
            storage: owner,
            collection: storage.collection,
        };
        let mut view = storage.with_session(|session| Ok(session.view()))?;
        let Some(channel_id) = options.channel_id.as_deref() else {
            let channels = list_visible_text_channels(token)?;
            let mut report = PullReport {
                all_visible: true,
                guilds: channels
                    .iter()
                    .map(|channel| channel.guild_id.as_str())
                    .collect::<BTreeSet<_>>()
                    .len(),
                channels: Vec::new(),
            };
            for channel in channels {
                let result = pull_channel(
                    storage,
                    &mut view,
                    source,
                    &channel.id,
                    None,
                    options.fetch_limit,
                    options.reconcile_limit,
                )
                .map_err(|error| format!("{error:#}"));
                report.channels.push(ChannelPull {
                    channel_id: channel.id,
                    name: channel.name,
                    result,
                });
            }
            return Ok(report);
        };
        let result = pull_channel(
            storage,
            &mut view,
            source,
            channel_id,
            None,
            options.fetch_limit,
            options.reconcile_limit,
        )?;
        Ok(PullReport {
            all_visible: false,
            guilds: 0,
            channels: vec![ChannelPull {
                channel_id: channel_id.to_owned(),
                name: String::new(),
                result: Ok(result),
            }],
        })
    })
}

/// Fetch a complete forward interval and one bounded recent reconciliation
/// window. The interval is appended only after every semantic payload and file
/// has been staged successfully.
fn pull_channel(
    storage: DiscordStorage<'_>,
    view: &mut CollectionView,
    source: &mut dyn Source,
    channel_id: &str,
    floor: Option<u64>,
    fetch_limit: u32,
    reconcile_limit: u32,
) -> Result<ChannelReceipt> {
    discord_model::validate_snowflake(channel_id).context("invalid channel id")?;
    let channel = discord_model::channel_fragment(channel_id)?
        .root()
        .expect("intrinsic channel has one root");
    let prior = discord_model::channel_coverage(&view.facts, channel)?;
    // Coverage goes on from its frontier; a channel without any begins as a
    // baseline, at the floor when there is one.
    let (after, baseline) = match prior {
        Some(coverage) => (Some(coverage.through_inclusive), false),
        None => (floor, true),
    };
    let forward = fetch_complete_forward(after, baseline, fetch_limit, |request| {
        source.page(channel_id, request)
    })?;
    let recent_payloads = if prior.is_some() {
        source.page(
            channel_id,
            PageRequest {
                after: None,
                before: None,
                limit: reconcile_limit,
            },
        )?
    } else {
        Vec::new()
    };

    let mut payloads = forward.payloads;
    payloads.extend(recent_payloads);
    let messages = parse_messages(payloads, channel_id)?;
    if messages.is_empty() {
        return Ok(ChannelReceipt {
            channel_id: channel_id.to_owned(),
            observations: 0,
            coverage: None,
            commit: None,
            more: false,
        });
    }

    let fragment = build_ingest_fragment(
        &messages,
        forward.coverage,
        Some(&view.facts),
        |url, limit| source.attachment(url, limit),
    )?;
    let description = match forward.coverage {
        Some(interval) => format!(
            "discord: observed {} payloads in channel {channel_id}, covered ({}, {}]{}",
            messages.len(),
            interval.after_exclusive,
            interval.through_inclusive,
            if interval.baseline {
                " from bounded baseline"
            } else {
                ""
            },
        ),
        None => format!(
            "discord: reconciled {} recent payloads in channel {channel_id}",
            messages.len()
        ),
    };
    // The selected coverage remains frozen while REST payloads and attachments
    // are fetched. Reenter the local owner only after the fragment is complete.
    let (commit, after) = storage.with_session(|session| {
        let commit = session.commit(fragment, description)?;
        Ok((commit, session.view()))
    })?;
    *view = after;
    Ok(ChannelReceipt {
        channel_id: channel_id.to_owned(),
        observations: messages.len(),
        coverage: forward.coverage,
        commit: Some(commit),
        more: forward.more,
    })
}

/// One page of a channel's messages: at most `limit`, newest first, after
/// and before the given message ids.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageRequest {
    pub after: Option<u64>,
    pub before: Option<u64>,
    pub limit: u32,
}

#[derive(Debug)]
struct ForwardBatch {
    payloads: Vec<JsonValue>,
    coverage: Option<discord_model::CoverageInterval>,
    /// The page budget ran out with a full page: there is more to read.
    more: bool,
}

fn fetch_message_page(
    token: &str,
    channel_id: &str,
    request: PageRequest,
) -> Result<Vec<JsonValue>> {
    let mut url = format!(
        "{DISCORD_API_BASE}/channels/{channel_id}/messages?limit={}",
        request.limit.clamp(1, 100)
    );
    if let Some(after) = request.after {
        url.push_str("&after=");
        url.push_str(&after.to_string());
    }
    if let Some(before) = request.before {
        url.push_str("&before=");
        url.push_str(&before.to_string());
    }
    let response = build_client()?
        .get(&url)
        .header("Authorization", format!("Bot {token}"))
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("discord read failed ({status}): {body}");
    }
    response.json().context("parse Discord message page")
}

/// Read forward from `after` (the covered frontier, or the floor where
/// coverage is to begin): Discord answers `after=X` with the `limit` messages
/// right after X, newest first, so each page goes on from the newest id of the
/// one before, until a short page or [`FORWARD_PAGES`] pages. Every id in
/// `(after, newest read]` has then been read. Without `after`, the newest page
/// is a bounded baseline: `(oldest - 1, newest]`.
fn fetch_complete_forward<F>(
    after: Option<u64>,
    baseline: bool,
    limit: u32,
    mut fetch: F,
) -> Result<ForwardBatch>
where
    F: FnMut(PageRequest) -> Result<Vec<JsonValue>>,
{
    let limit = limit.clamp(1, 100);
    let Some(start) = after else {
        let payloads = fetch(PageRequest {
            after: None,
            before: None,
            limit,
        })?;
        let ids = checked_page_ids(&payloads, limit)?;
        let coverage = match (ids.iter().min(), ids.iter().max()) {
            (Some(oldest), Some(newest)) => Some(discord_model::CoverageInterval::new(
                oldest.saturating_sub(1),
                *newest,
                true,
            )?),
            _ => None,
        };
        return Ok(ForwardBatch {
            payloads,
            coverage,
            more: false,
        });
    };

    let mut payloads = Vec::new();
    let mut through = start;
    let mut more = false;
    for page in 1..=FORWARD_PAGES {
        let payload = fetch(PageRequest {
            after: Some(through),
            before: None,
            limit,
        })?;
        let ids = checked_page_ids(&payload, limit)?;
        if ids.iter().any(|id| *id <= through) {
            bail!("Discord after={through} page returned a non-forward message");
        }
        let Some(newest) = ids.iter().max().copied() else {
            break;
        };
        payloads.extend(payload);
        through = newest;
        if ids.len() < limit as usize {
            break;
        }
        more = page == FORWARD_PAGES;
    }
    let coverage = (through > start)
        .then(|| discord_model::CoverageInterval::new(start, through, baseline))
        .transpose()?;
    Ok(ForwardBatch {
        payloads,
        coverage,
        more,
    })
}

fn checked_page_ids(payloads: &[JsonValue], limit: u32) -> Result<Vec<u64>> {
    if payloads.len() > limit as usize {
        bail!(
            "Discord returned {} messages for a page limited to {limit}",
            payloads.len()
        );
    }
    let ids = payload_ids(payloads)?;
    if ids.iter().copied().collect::<BTreeSet<_>>().len() != ids.len() {
        bail!("Discord returned duplicate message ids within one page");
    }
    Ok(ids)
}

fn payload_ids(payloads: &[JsonValue]) -> Result<Vec<u64>> {
    payloads
        .iter()
        .map(|payload| {
            let raw = payload
                .get("id")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("Discord message payload missing id"))?;
            discord_model::validate_snowflake(raw)
                .with_context(|| format!("invalid Discord message id '{raw}'"))
        })
        .collect()
}

#[derive(Debug, Clone)]
struct IncomingMessage {
    external_id: String,
    channel_external_id: String,
    author_external_id: String,
    author_display_name: Option<String>,
    content: String,
    created_at: Inline<NsTAIInterval>,
    edited_at: Option<Inline<NsTAIInterval>>,
    reply_to_external_id: Option<String>,
    attachments: Vec<AttachmentSource>,
    /// A system notice (a pin, a member joining) rather than something
    /// somebody wrote.
    system: bool,
}

#[derive(Debug, Clone)]
struct AttachmentSource {
    source_id: String,
    /// Ephemeral transport locator. Discord signs and refreshes this value; it
    /// must never participate in semantic identity or equality.
    url: String,
    filename: String,
    content_type: Option<String>,
    /// The size Discord declares.
    size: Option<u64>,
}

fn parse_messages(
    payloads: Vec<JsonValue>,
    expected_channel_id: &str,
) -> Result<Vec<IncomingMessage>> {
    discord_model::validate_snowflake(expected_channel_id)
        .context("invalid expected channel id")?;
    let mut messages = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let external_id = required_snowflake(&payload, "id", "message")?;
        if let Some(actual_channel) = payload.get("channel_id").and_then(JsonValue::as_str) {
            discord_model::validate_snowflake(actual_channel)
                .context("invalid message channel_id")?;
            if actual_channel != expected_channel_id {
                bail!(
                    "Discord returned message {external_id} for channel {actual_channel}, expected {expected_channel_id}"
                );
            }
        }
        let content = payload
            .get("content")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_owned();
        let author = payload
            .get("author")
            .ok_or_else(|| anyhow!("message {external_id} missing author"))?;
        let author_external_id = required_snowflake(author, "id", "message author")?;
        let author_display_name = author
            .get("global_name")
            .and_then(JsonValue::as_str)
            .or_else(|| author.get("username").and_then(JsonValue::as_str))
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let timestamp = payload
            .get("timestamp")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("message {external_id} missing timestamp"))?;
        let created_at =
            parse_iso8601(timestamp).with_context(|| format!("parse timestamp '{timestamp}'"))?;
        let edited_at = payload
            .get("edited_timestamp")
            .and_then(JsonValue::as_str)
            .map(parse_iso8601)
            .transpose()
            .with_context(|| format!("parse edited timestamp for message {external_id}"))?;
        let reply_to_external_id = payload
            .get("referenced_message")
            .and_then(|value| value.get("id"))
            .and_then(JsonValue::as_str)
            .map(|raw| {
                discord_model::validate_snowflake(raw)
                    .with_context(|| format!("invalid reply target id '{raw}'"))
                    .map(|_| raw.to_owned())
            })
            .transpose()?;

        let attachments = match payload.get("attachments") {
            None | Some(JsonValue::Null) => Vec::new(),
            Some(JsonValue::Array(values)) => values
                .iter()
                .map(|attachment| {
                    let source_id = required_snowflake(attachment, "id", "attachment")?;
                    let url = attachment
                        .get("url")
                        .and_then(JsonValue::as_str)
                        .filter(|url| !url.is_empty())
                        .ok_or_else(|| anyhow!("attachment {source_id} missing URL"))?
                        .to_owned();
                    let filename = attachment
                        .get("filename")
                        .and_then(JsonValue::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| anyhow!("attachment {source_id} missing filename"))?
                        .to_owned();
                    let content_type = attachment
                        .get("content_type")
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned);
                    let size = attachment.get("size").and_then(JsonValue::as_u64);
                    Ok(AttachmentSource {
                        source_id,
                        url,
                        filename,
                        content_type,
                        size,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            Some(_) => bail!("message {external_id} attachments field is not an array"),
        };
        // A message without a type is a default one.
        let kind = payload.get("type").and_then(JsonValue::as_u64).unwrap_or(0);

        messages.push(IncomingMessage {
            external_id,
            channel_external_id: expected_channel_id.to_owned(),
            author_external_id,
            author_display_name,
            content,
            created_at,
            edited_at,
            reply_to_external_id,
            attachments,
            system: !WRITTEN_MESSAGE_TYPES.contains(&kind),
        });
    }
    messages
        .sort_by_key(|message| discord_model::validate_snowflake(&message.external_id).unwrap());
    Ok(messages)
}

fn required_snowflake(value: &JsonValue, field: &str, subject: &str) -> Result<String> {
    let raw = value
        .get(field)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("{subject} missing {field}"))?;
    discord_model::validate_snowflake(raw)
        .with_context(|| format!("invalid {subject} {field} '{raw}'"))?;
    Ok(raw.to_owned())
}

/// Construct one complete self-contained collection fragment.
///
/// The fetch callback makes the validation-before-publication boundary
/// directly testable. Any attachment error aborts construction; callers have
/// not opened a writer yet and therefore cannot publish a receipt. An
/// attachment `stored` already holds with its bytes is linked, not fetched
/// again; one declared larger than [`MAX_ATTACHMENT_BYTES`] is recorded by
/// name and size alone.
fn build_ingest_fragment<F>(
    messages: &[IncomingMessage],
    coverage: Option<discord_model::CoverageInterval>,
    stored: Option<&FactArchive>,
    mut fetch: F,
) -> Result<Fragment>
where
    F: FnMut(&str, u64) -> Result<Vec<u8>>,
{
    if messages.is_empty() {
        if coverage.is_some() {
            bail!("an ingestion receipt requires at least one observed message");
        }
        return Ok(Fragment::empty());
    }

    let expected_channel = messages[0].channel_external_id.as_str();
    for message in messages {
        if message.channel_external_id != expected_channel {
            bail!("one Discord ingestion COMMIT cannot span channels");
        }
    }
    if let Some(interval) = coverage {
        let covers_observation = messages.iter().any(|message| {
            let id = discord_model::validate_snowflake(&message.external_id)
                .expect("parsed messages have valid ids");
            id > interval.after_exclusive && id <= interval.through_inclusive
        });
        if !covers_observation {
            bail!("an ingestion interval must cover at least one staged message");
        }
    }

    let mut fragment = Fragment::empty();
    let channel = discord_model::channel_fragment(expected_channel)?;
    let channel_id = channel.root().expect("intrinsic channel has one root");
    fragment += channel;

    #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
    struct AttachmentKey {
        source_id: String,
        filename: String,
        content_type: Option<String>,
    }

    #[derive(Debug)]
    struct AttachmentTransport {
        urls: BTreeSet<String>,
        size: Option<u64>,
    }

    // Aggregate by stable Discord attachment id. Signed CDN URLs are merely
    // retryable transports and are intentionally absent from equality and
    // intrinsic identity.
    let mut transports: BTreeMap<AttachmentKey, AttachmentTransport> = BTreeMap::new();
    for source in messages.iter().flat_map(|message| &message.attachments) {
        let key = AttachmentKey {
            source_id: source.source_id.clone(),
            filename: file_capability::leaf_name(&source.filename),
            content_type: source.content_type.clone(),
        };
        transports
            .entry(key)
            .or_insert_with(|| AttachmentTransport {
                urls: BTreeSet::new(),
                size: source.size,
            })
            .urls
            .insert(source.url.clone());
    }

    let mut prepared_attachments: BTreeMap<AttachmentKey, (Id, Fragment)> = BTreeMap::new();
    for (key, transport) in transports {
        if let Some(facts) = stored {
            // The same Discord attachment, stored with its bytes by an
            // earlier pull: the very occurrence a download would rebuild.
            let source: discord_model::TextHandle = key.source_id.clone().to_blob().get_handle();
            let name: discord_model::TextHandle = key.filename.clone().to_blob().get_handle();
            let known = find!(
                attachment: Id,
                pattern!(facts, [{
                    ?attachment @
                    metadata::tag: archive::kind_attachment,
                    archive::attachment_source_id: &source,
                    archive::attachment_name: &name,
                    archive::attachment_file: _?file,
                }])
            )
            .next();
            if let Some(attachment_id) = known {
                prepared_attachments.insert(key, (attachment_id, Fragment::empty()));
                continue;
            }
        }
        if let Some(size) = transport.size.filter(|size| *size > MAX_ATTACHMENT_BYTES) {
            let attachment = entity! { _ @
                metadata::tag: archive::kind_attachment,
                archive::attachment_source_id: key.source_id.clone(),
                archive::attachment_name: key.filename.clone(),
                archive::attachment_size_bytes: size,
            };
            let attachment_id = attachment
                .root()
                .expect("attachment occurrence has one exported root");
            prepared_attachments.insert(key, (attachment_id, attachment));
            continue;
        }
        let mut failures = Vec::new();
        let mut bytes = None;
        for url in &transport.urls {
            match fetch(url, MAX_ATTACHMENT_BYTES) {
                Ok(value) => {
                    bytes = Some(value);
                    break;
                }
                Err(error) => failures.push(format!("{url}: {error:#}")),
            }
        }
        let bytes = bytes.ok_or_else(|| {
            anyhow!(
                "fetch Discord attachment {} failed via every observed URL: {}",
                key.source_id,
                failures.join("; ")
            )
        })?;
        // Discord's content type is untrusted protocol input: a malformed one
        // degrades to the generic binary type instead of blocking the channel.
        let media_type = match key.content_type.as_deref() {
            Some(content_type) => file_capability::normalize_media_type_or_default(content_type),
            None => file_capability::infer_media_type(Path::new(&key.filename)).to_owned(),
        };
        let file_fragment = file_capability::stage(bytes, &key.filename, &media_type)
            .with_context(|| {
                format!("construct canonical file for attachment {}", key.source_id)
            })?;
        let file_id = file_fragment
            .root()
            .expect("canonical file fragment has one root");
        let mut attachment = entity! { _ @
            metadata::tag: archive::kind_attachment,
            archive::attachment_source_id: key.source_id.clone(),
            archive::attachment_name: key.filename.clone(),
            archive::attachment_file: file_id,
        };
        let attachment_id = attachment
            .root()
            .expect("attachment occurrence has one exported root");
        attachment += file_fragment;
        prepared_attachments.insert(key, (attachment_id, attachment));
    }

    for message in messages {
        let message_anchor = discord_model::message_anchor_fragment(&message.external_id)?;
        let message_anchor_id = message_anchor
            .root()
            .expect("intrinsic message anchor has one root");
        fragment += message_anchor;

        let author = discord_model::user_fragment(&message.author_external_id)?;
        let author_id = author.root().expect("intrinsic user anchor has one root");
        fragment += author;
        if let Some(display_name) = &message.author_display_name {
            fragment += entity! { _ @
                metadata::tag: discord::kind_user_profile,
                discord::user: author_id,
                archive::author_name: display_name.clone(),
            };
        }

        let reply_to = match message.reply_to_external_id.as_deref() {
            Some(external) => {
                let anchor = discord_model::message_anchor_fragment(external)?;
                let id = anchor.root().expect("intrinsic reply anchor has one root");
                fragment += anchor;
                Some(id)
            }
            None => None,
        };

        let mut attachment_ids = Vec::with_capacity(message.attachments.len());
        for source in &message.attachments {
            let key = AttachmentKey {
                source_id: source.source_id.clone(),
                filename: file_capability::leaf_name(&source.filename),
                content_type: source.content_type.clone(),
            };
            let (id, attachment) = prepared_attachments
                .get(&key)
                .expect("every parsed attachment was prepared");
            attachment_ids.push(*id);
            fragment += attachment.clone();
        }

        fragment += entity! { _ @
            metadata::tag: archive::kind_message,
            discord::message: message_anchor_id,
            discord::channel: channel_id,
            archive::author: author_id,
            archive::content: message.content.clone(),
            metadata::created_at: message.created_at,
            archive::edited_at?: message.edited_at,
            archive::reply_to?: reply_to,
            archive::attachment*: attachment_ids,
        };
        if message.system {
            fragment += entity! { _ @
                metadata::tag: discord::kind_system_notice,
                discord::message: message_anchor_id,
            };
        }
    }

    // Keep this last: no receipt fragment exists until every semantic payload
    // and attachment above has validated and staged successfully.
    if let Some(interval) = coverage {
        fragment += discord_model::coverage_fragment(channel_id, interval);
    }
    Ok(fragment)
}

fn read_history(view: &CollectionView, options: &ReadOptions) -> Result<History> {
    let since = options
        .since
        .as_deref()
        .map(|value| parse_iso8601(value.trim()))
        .transpose()?;
    let channel_filter = options
        .channel_id
        .as_deref()
        .map(discord_model::channel_fragment)
        .transpose()?
        .map(|fragment| fragment.root().expect("intrinsic channel has one root"));
    let mut messages = discord_model::select_messages(&view.facts, channel_filter, since)?;

    if options.limit > 0 && messages.len() > options.limit {
        messages = messages.split_off(messages.len() - options.limit);
    }
    if options.descending {
        messages.reverse();
    }

    if messages.is_empty() {
        return Ok(History {
            channel_id: options.channel_id.clone(),
            messages: Vec::new(),
        });
    }
    let channel_names = discord_model::channel_labels(&view.facts, &view.reader)?;
    let author_names = discord_model::user_labels(&view.facts, &view.reader)?;
    let mut rows = Vec::new();
    for message in messages {
        let content =
            discord_model::read_text(&view.reader, message.content, "Discord message content")?;
        let author = author_names
            .get(&message.author)
            .cloned()
            .unwrap_or_else(|| format!("{}", message.author));
        rows.push(ObservedMessage {
            observation: message.observation,
            anchor: message.anchor,
            created_at: message.created_at,
            edited_at: message.edited_at,
            channel: message.channel,
            channel_name: channel_names.get(&message.channel).cloned(),
            author,
            content,
            reply_to: message.reply_to,
            attachments: message.attachments,
            variant_index: message.variant_index,
            variant_count: message.variant_count,
        });
    }
    Ok(History {
        channel_id: options.channel_id.clone(),
        messages: rows,
    })
}

struct VisibleChannel {
    id: String,
    name: String,
    guild_id: String,
}

fn list_visible_text_channels(token: &str) -> Result<Vec<VisibleChannel>> {
    let client = build_client()?;
    let guilds: Vec<JsonValue> = client
        .get(format!("{DISCORD_API_BASE}/users/@me/guilds"))
        .header("Authorization", format!("Bot {token}"))
        .send()
        .context("GET /users/@me/guilds")?
        .error_for_status()
        .context("guilds request failed")?
        .json()
        .context("parse guilds response")?;

    let mut out = Vec::new();
    for guild in guilds {
        let guild_id = guild.get("id").and_then(JsonValue::as_str).unwrap_or("");
        if discord_model::validate_snowflake(guild_id).is_err() {
            continue;
        }
        let channels: Vec<JsonValue> = client
            .get(format!("{DISCORD_API_BASE}/guilds/{guild_id}/channels"))
            .header("Authorization", format!("Bot {token}"))
            .send()
            .with_context(|| format!("GET /guilds/{guild_id}/channels"))?
            .error_for_status()
            .with_context(|| format!("channels request for guild {guild_id} failed"))?
            .json()
            .with_context(|| format!("parse channels for guild {guild_id}"))?;
        for channel in channels {
            let kind = channel
                .get("type")
                .and_then(JsonValue::as_i64)
                .unwrap_or(-1);
            if !matches!(kind, 0 | 5 | 15) {
                continue;
            }
            let id = channel.get("id").and_then(JsonValue::as_str).unwrap_or("");
            if discord_model::validate_snowflake(id).is_err() {
                continue;
            }
            out.push(VisibleChannel {
                id: id.to_owned(),
                name: channel
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
                    .to_owned(),
                guild_id: guild_id.to_owned(),
            });
        }
    }
    Ok(out)
}

fn list_channels(token: &str, guild_filter: Option<&str>) -> Result<ChannelListing> {
    if let Some(filter) = guild_filter {
        discord_model::validate_snowflake(filter).context("invalid guild filter")?;
    }
    let client = build_client()?;
    let guilds: Vec<JsonValue> = client
        .get(format!("{DISCORD_API_BASE}/users/@me/guilds"))
        .header("Authorization", format!("Bot {token}"))
        .send()
        .context("GET /users/@me/guilds")?
        .error_for_status()
        .context("guilds request failed")?
        .json()
        .context("parse guilds response")?;
    if guilds.is_empty() {
        return Ok(ChannelListing {
            bot_in_any_guild: false,
            guilds: Vec::new(),
        });
    }

    let mut result = Vec::new();
    for guild in guilds {
        let guild_id = guild
            .get("id")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("Discord guild missing id"))?;
        discord_model::validate_snowflake(guild_id).context("invalid Discord guild id")?;
        if guild_filter.is_some_and(|filter| filter != guild_id) {
            continue;
        }
        let guild_name = guild
            .get("name")
            .and_then(JsonValue::as_str)
            .unwrap_or("<unnamed>");

        let channels: Vec<JsonValue> = client
            .get(format!("{DISCORD_API_BASE}/guilds/{guild_id}/channels"))
            .header("Authorization", format!("Bot {token}"))
            .send()
            .with_context(|| format!("GET /guilds/{guild_id}/channels"))?
            .error_for_status()
            .with_context(|| format!("channels request for guild {guild_id} failed"))?
            .json()
            .with_context(|| format!("parse channels for guild {guild_id}"))?;
        let mut rows = Vec::new();
        for channel in &channels {
            let id = channel
                .get("id")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("Discord channel missing id"))?;
            discord_model::validate_snowflake(id).context("invalid Discord channel id")?;
            let name = channel
                .get("name")
                .and_then(JsonValue::as_str)
                .unwrap_or("<unnamed>");
            let kind = channel
                .get("type")
                .and_then(JsonValue::as_i64)
                .unwrap_or(-1);
            rows.push((kind, id, name));
        }
        rows.sort_by_key(|(kind, _, _)| match kind {
            4 => 0,
            0 | 5 => 1,
            15 => 2,
            _ => 3,
        });
        result.push(GuildChannels {
            id: guild_id.to_owned(),
            name: guild_name.to_owned(),
            channels: rows
                .into_iter()
                .map(|(kind, id, name)| Channel {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    kind,
                })
                .collect(),
        });
    }
    Ok(ChannelListing {
        bot_in_any_guild: true,
        guilds: result,
    })
}

fn fetch_attachment_bytes(url: &str, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let response = Client::builder()
        .user_agent("triblespace-discord/0.2")
        .timeout(ATTACHMENT_TIMEOUT)
        .build()
        .context("build reqwest client")?
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        bail!("GET {url} failed: status={status} body={body}");
    }
    let mut bytes = Vec::new();
    response
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .context("read attachment body")?;
    if bytes.len() as u64 > limit {
        bail!("attachment at {url} is larger than {limit} bytes");
    }
    Ok(bytes)
}

fn build_client() -> Result<Client> {
    Client::builder()
        .user_agent("triblespace-discord/0.2")
        .timeout(Duration::from_secs(30))
        .build()
        .context("build reqwest client")
}

pub(super) fn parse_iso8601(value: &str) -> Result<Inline<NsTAIInterval>> {
    let epoch = Epoch::from_gregorian_str(value)
        .map_err(|error| anyhow!("parse ISO8601 '{value}': {error}"))?;
    Ok(epoch_interval(epoch))
}

fn epoch_interval(epoch: Epoch) -> Inline<NsTAIInterval> {
    (epoch, epoch)
        .try_to_inline()
        .expect("point interval encodes")
}

pub(super) fn format_interval(interval: Inline<NsTAIInterval>) -> String {
    let (lower, _): (Epoch, Epoch) = interval.try_from_inline().expect("valid TAI interval");
    lower.to_gregorian_str(TimeScale::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::files::KIND_FILE;
    use std::fs::File;
    use triblespace::prelude::inlineencodings::U256BE;
    fn message_json(
        id: &str,
        channel: &str,
        content: &str,
        edited: Option<&str>,
        attachments: JsonValue,
    ) -> JsonValue {
        json!({
            "id": id,
            "channel_id": channel,
            "content": content,
            "author": {
                "id": "100000000000000010",
                "username": "Ada",
                "global_name": "Ada Lovelace"
            },
            "timestamp": "2026-08-07T08:00:00Z",
            "edited_timestamp": edited,
            "attachments": attachments,
            "referenced_message": null
        })
    }

    fn fresh_storage(directory: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let pile = directory.path().join("discord.pile");
        let key = directory.path().join("discord.key");
        File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        (pile, key)
    }

    fn test_storage(storage: &crate::storage::Storage) -> DiscordStorage<'_> {
        DiscordStorage {
            storage,
            collection: None,
        }
    }

    #[test]
    fn direct_send_keeps_literal_body_and_returns_the_stored_remote_id() {
        let directory = tempfile::tempdir().unwrap();
        let (pile, key) = fresh_storage(&directory);
        let owner = crate::storage::Storage::new(pile.clone(), Some(key.clone()));
        let storage = test_storage(&owner);
        let channel = "100000000000000002";
        let receipt = send_with(
            storage,
            "fixture-token",
            channel,
            "@-",
            |token, actual_channel, body| {
                assert_eq!(token, "fixture-token");
                assert_eq!(actual_channel, channel);
                assert_eq!(body, "@-");
                Ok(message_json(
                    "100000000000000001",
                    channel,
                    body,
                    None,
                    json!([]),
                ))
            },
        )
        .unwrap();
        assert_eq!(receipt.message_id, "100000000000000001");
        assert_eq!(receipt.channel_id, channel);
        let history = read_history(&storage.view().unwrap(), &ReadOptions::default()).unwrap();
        assert_eq!(history.messages[0].content, "@-");
    }

    #[test]
    fn unauthorized_collection_fails_before_remote_post() {
        let directory = tempfile::tempdir().unwrap();
        let (pile_path, key) = fresh_storage(&directory);
        let root = SigningKey::from_bytes(&[0x41; 32]);
        let mut pile = Pile::open(&pile_path).unwrap();
        let collection = pile
            .collection(
                "discord",
                crate::collection_names::private_policy(root.verifying_key()),
            )
            .unwrap();
        pile.close().unwrap();

        let mut posts = 0;
        let result = send_with(
            DiscordStorage {
                storage: &crate::storage::Storage::new(pile_path.clone(), Some(key.clone())),
                collection: Some(collection.handle()),
            },
            "unused-token",
            "100000000000000002",
            "must not leave this process",
            |_, _, _| {
                posts += 1;
                Ok(json!({}))
            },
        );
        let error = result.unwrap_err();
        assert!(error.to_string().contains("WRITE admission"), "{error:#}");
        assert_eq!(posts, 0, "authorization failure must precede HTTP POST");
    }

    #[test]
    fn replayed_payload_has_identical_intrinsic_observation() {
        let payload = message_json(
            "100000000000000001",
            "100000000000000002",
            "hello",
            None,
            json!([]),
        );
        let first = parse_messages(vec![payload.clone()], "100000000000000002").unwrap();
        let second = parse_messages(vec![payload], "100000000000000002").unwrap();
        let interval =
            discord_model::CoverageInterval::new(100000000000000000, 100000000000000001, true)
                .unwrap();
        let first = build_ingest_fragment(&first, Some(interval), None, |_, _| {
            unreachable!("no attachments")
        })
        .unwrap();
        let second = build_ingest_fragment(&second, Some(interval), None, |_, _| {
            unreachable!("no attachments")
        })
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn volatile_payload_and_profile_changes_do_not_fork_message_semantics() {
        let directory = tempfile::tempdir().unwrap();
        let (pile, key) = fresh_storage(&directory);
        let owner = crate::storage::Storage::new(pile.clone(), Some(key.clone()));
        let storage = test_storage(&owner);
        let channel = "100000000000000004";
        let first = message_json(
            "100000000000000003",
            channel,
            "stable meaning",
            None,
            json!([]),
        );
        let mut second = first.clone();
        second["pinned"] = json!(true);
        second["reactions"] = json!([{"count": 42, "emoji": {"name": "✨"}}]);
        second["author"]["global_name"] = json!("Countess Lovelace");
        let messages = parse_messages(vec![first, second], channel).unwrap();
        storage
            .publish(
                build_ingest_fragment(&messages, None, None, |_, _| unreachable!("no attachments"))
                    .unwrap(),
                "volatile replay".to_owned(),
            )
            .unwrap();
        let view = storage.view().unwrap();
        let selected = discord_model::select_messages(
            &view.facts,
            Some(
                discord_model::channel_fragment(channel)
                    .unwrap()
                    .root()
                    .unwrap(),
            ),
            None,
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].variant_count, 1);
        let observations = find!(
            observation: Id,
            pattern!(&view.facts, [{
                ?observation @
                metadata::tag: archive::kind_message,
                discord::message: _?anchor,
            }])
        )
        .collect::<BTreeSet<_>>();
        assert_eq!(observations.len(), 1);

        let users = find!(
            user: Id,
            pattern!(&view.facts, [{
                ?user @ metadata::tag: discord::kind_user
            }])
        )
        .collect::<BTreeSet<_>>();
        let profiles = find!(
            profile: Id,
            pattern!(&view.facts, [{
                ?profile @ metadata::tag: discord::kind_user_profile
            }])
        )
        .collect::<BTreeSet<_>>();
        assert_eq!(users.len(), 1);
        assert_eq!(profiles.len(), 2);
        let label = discord_model::user_labels(&view.facts, &view.reader)
            .unwrap()
            .remove(users.first().unwrap())
            .unwrap();
        assert!(label.contains("Ada Lovelace"));
        assert!(label.contains("Countess Lovelace"));
    }

    #[test]
    fn refreshed_signed_attachment_urls_are_retryable_transport_only() {
        let directory = tempfile::tempdir().unwrap();
        let (pile, key) = fresh_storage(&directory);
        let owner = crate::storage::Storage::new(pile.clone(), Some(key.clone()));
        let storage = test_storage(&owner);
        let channel = "100000000000000006";
        let mut old = message_json(
            "100000000000000005",
            channel,
            "with file",
            None,
            json!([{
                "id": "100000000000000007",
                "url": "https://cdn.example/a-expired.bin?ex=old&hm=old",
                "filename": "folder/file.bin",
                "content_type": "application/octet-stream"
            }]),
        );
        let mut refreshed = old.clone();
        refreshed["attachments"][0]["url"] = json!("https://cdn.example/b-fresh.bin?ex=new&hm=new");
        // A volatile field changes too; neither change belongs to message
        // semantics.
        old["pinned"] = json!(false);
        refreshed["pinned"] = json!(true);

        let old_fragment = build_ingest_fragment(
            &parse_messages(vec![old.clone()], channel).unwrap(),
            None,
            None,
            |_, _| Ok(b"bytes".to_vec()),
        )
        .unwrap();
        let refreshed_fragment = build_ingest_fragment(
            &parse_messages(vec![refreshed.clone()], channel).unwrap(),
            None,
            None,
            |_, _| Ok(b"bytes".to_vec()),
        )
        .unwrap();
        assert_eq!(old_fragment, refreshed_fragment);

        let messages = parse_messages(vec![old, refreshed], channel).unwrap();
        let mut attempts = Vec::new();
        let fragment = build_ingest_fragment(&messages, None, None, |url, _| {
            attempts.push(url.to_owned());
            if url.contains("expired") {
                bail!("expired signature");
            }
            Ok(b"bytes".to_vec())
        })
        .unwrap();
        assert_eq!(attempts.len(), 2);
        storage
            .publish(fragment, "refreshed attachment URL".to_owned())
            .unwrap();
        let view = storage.view().unwrap();
        assert_eq!(
            discord_model::select_messages(&view.facts, None, None)
                .unwrap()
                .len(),
            1
        );
        let attachments = find!(
            attachment: Id,
            pattern!(&view.facts, [{
                ?attachment @ metadata::tag: archive::kind_attachment
            }])
        )
        .collect::<BTreeSet<_>>();
        assert_eq!(attachments.len(), 1);
        assert!(exists!(pattern!(&view.facts, [{
            _?file @ metadata::tag: &KIND_FILE
        }])));
    }

    #[test]
    fn attachment_failure_cannot_publish_coverage() {
        let directory = tempfile::tempdir().unwrap();
        let (pile, key) = fresh_storage(&directory);
        let owner = crate::storage::Storage::new(pile.clone(), Some(key.clone()));
        let storage = test_storage(&owner);
        let channel = "100000000000000009";
        let payload = message_json(
            "100000000000000008",
            channel,
            "with file",
            None,
            json!([{
                "id": "100000000000000010",
                "url": "https://cdn.example/file.bin",
                "filename": "file.bin",
                "content_type": "application/octet-stream"
            }]),
        );
        let messages = parse_messages(vec![payload], channel).unwrap();
        let interval =
            discord_model::CoverageInterval::new(100000000000000007, 100000000000000008, true)
                .unwrap();
        assert!(
            build_ingest_fragment(&messages, Some(interval), None, |_, _| bail!("offline"))
                .is_err()
        );
        assert!(storage.view().unwrap().facts.iter().next().is_none());

        let fragment =
            build_ingest_fragment(
                &messages,
                Some(interval),
                None,
                |_, _| Ok(b"bytes".to_vec()),
            )
            .unwrap();
        storage
            .publish(fragment, "complete test page".to_owned())
            .unwrap();
        let view = storage.view().unwrap();
        let channel_id = discord_model::channel_fragment(channel)
            .unwrap()
            .root()
            .unwrap();
        assert_eq!(
            discord_model::channel_coverage(&view.facts, channel_id)
                .unwrap()
                .unwrap()
                .through_inclusive,
            100000000000000008
        );
    }

    /// One channel as Discord pages it: `after` gives the `limit` ids right
    /// after it, `before` the `limit` right before it, neither the newest
    /// `limit`; every page newest first.
    fn discord_page(available: &[u64], request: PageRequest) -> Vec<JsonValue> {
        let mut ids = available
            .iter()
            .copied()
            .filter(|id| request.after.is_none_or(|after| *id > after))
            .filter(|id| request.before.is_none_or(|before| *id < before))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        let limit = request.limit as usize;
        let ids = if request.after.is_some() {
            ids.into_iter().take(limit).collect::<Vec<_>>()
        } else {
            ids.split_off(ids.len().saturating_sub(limit))
        };
        ids.into_iter()
            .rev()
            .map(|id| json!({"id": id.to_string()}))
            .collect()
    }

    #[test]
    fn forward_pagination_closes_a_gap_page_by_page() {
        let frontier = 100_000_u64;
        let available = ((frontier - 100)..=(frontier + 250)).collect::<Vec<_>>();
        let mut requests = Vec::new();
        let batch = fetch_complete_forward(Some(frontier), false, 100, |request| {
            requests.push(request);
            Ok(discord_page(&available, request))
        })
        .unwrap();
        let ingested = payload_ids(&batch.payloads)
            .unwrap()
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(ingested, ((frontier + 1)..=(frontier + 250)).collect());
        assert_eq!(
            batch.coverage,
            Some(discord_model::CoverageInterval::new(frontier, frontier + 250, false).unwrap())
        );
        assert!(!batch.more);
        let afters = requests
            .iter()
            .map(|request| request.after)
            .collect::<Vec<_>>();
        assert_eq!(
            afters,
            [Some(frontier), Some(frontier + 100), Some(frontier + 200)]
        );

        // Nothing new: no interval.
        let batch = fetch_complete_forward(Some(frontier + 250), false, 100, |request| {
            Ok(discord_page(&available, request))
        })
        .unwrap();
        assert!(batch.payloads.is_empty() && batch.coverage.is_none());
    }

    #[test]
    fn a_gap_longer_than_the_page_budget_is_closed_by_the_next_pull() {
        let frontier = 100_000_u64;
        let gap = 100 * FORWARD_PAGES as u64 + 30;
        let available = ((frontier + 1)..=(frontier + gap)).collect::<Vec<_>>();
        let first = fetch_complete_forward(Some(frontier), false, 100, |request| {
            Ok(discord_page(&available, request))
        })
        .unwrap();
        let reached = frontier + 100 * FORWARD_PAGES as u64;
        assert_eq!(first.payloads.len(), 100 * FORWARD_PAGES);
        assert_eq!(
            first.coverage,
            Some(discord_model::CoverageInterval::new(frontier, reached, false).unwrap())
        );
        assert!(first.more);
        let second = fetch_complete_forward(Some(reached), false, 100, |request| {
            Ok(discord_page(&available, request))
        })
        .unwrap();
        assert_eq!(second.payloads.len(), 30);
        assert!(!second.more);
    }

    #[test]
    fn first_page_is_an_explicit_bounded_baseline() {
        let available = (1_u64..=150).collect::<Vec<_>>();
        let batch = fetch_complete_forward(None, true, 100, |request| {
            Ok(discord_page(&available, request))
        })
        .unwrap();
        assert_eq!(batch.payloads.len(), 100);
        assert_eq!(
            batch.coverage,
            Some(discord_model::CoverageInterval::new(50, 150, true).unwrap())
        );

        // A floor instead: the baseline begins there, and reads forward.
        let batch = fetch_complete_forward(Some(120), true, 100, |request| {
            Ok(discord_page(&available, request))
        })
        .unwrap();
        assert_eq!(batch.payloads.len(), 30);
        assert_eq!(
            batch.coverage,
            Some(discord_model::CoverageInterval::new(120, 150, true).unwrap())
        );
    }

    #[test]
    fn a_large_attachment_is_recorded_without_its_bytes() {
        let channel = "100000000000000011";
        let payload = |url: &str| {
            message_json(
                "100000000000000012",
                channel,
                "a long video",
                None,
                json!([{
                    "id": "100000000000000013", "url": url, "filename": "video.mp4",
                    "content_type": "video/mp4", "size": MAX_ATTACHMENT_BYTES + 1
                }]),
            )
        };
        // Never downloaded, and the same whichever URL it came with.
        let never = |_: &str, _: u64| -> Result<Vec<u8>> { unreachable!("not downloaded") };
        let first = build_ingest_fragment(
            &parse_messages(vec![payload("https://cdn.example/a")], channel).unwrap(),
            None,
            None,
            never,
        )
        .unwrap();
        let second = build_ingest_fragment(
            &parse_messages(vec![payload("https://cdn.example/b")], channel).unwrap(),
            None,
            None,
            never,
        )
        .unwrap();
        assert_eq!(first, second);
        let recorded = find!(
            (attachment: Id, size: Inline<U256BE>),
            pattern!(&first, [{
                ?attachment @
                metadata::tag: archive::kind_attachment,
                archive::attachment_size_bytes: ?size,
            }])
        )
        .collect::<Vec<_>>();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            u64::try_from_inline(&recorded[0].1).unwrap(),
            MAX_ATTACHMENT_BYTES + 1
        );
        assert!(!exists!(pattern!(&first, [{
            _?attachment @ archive::attachment_file: _?file
        }])));

        // A malformed content type does not hold the message back either.
        let mut odd = payload("https://cdn.example/c");
        odd["attachments"][0]["size"] = json!(5);
        odd["attachments"][0]["content_type"] = json!("not a media type");
        build_ingest_fragment(
            &parse_messages(vec![odd], channel).unwrap(),
            None,
            None,
            |_, _| Ok(b"bytes".to_vec()),
        )
        .unwrap();
    }

    #[test]
    fn a_stored_attachment_is_linked_not_downloaded_again() {
        let directory = tempfile::tempdir().unwrap();
        let (pile, key) = fresh_storage(&directory);
        let owner = crate::storage::Storage::new(pile.clone(), Some(key.clone()));
        let storage = test_storage(&owner);
        let channel = "100000000000000014";
        let payload = |url: &str| {
            message_json(
                "100000000000000015",
                channel,
                "a photo",
                None,
                json!([{
                    "id": "100000000000000016", "url": url, "filename": "photo.png",
                    "content_type": "image/png", "size": 5
                }]),
            )
        };
        let downloaded = build_ingest_fragment(
            &parse_messages(vec![payload("https://cdn.example/a")], channel).unwrap(),
            None,
            None,
            |_, _| Ok(b"image".to_vec()),
        )
        .unwrap();
        storage
            .publish(downloaded.clone(), "downloaded".to_owned())
            .unwrap();
        let view = storage.view().unwrap();
        let linked = build_ingest_fragment(
            &parse_messages(vec![payload("https://cdn.example/b")], channel).unwrap(),
            None,
            Some(&view.facts),
            |_, _| unreachable!("already stored"),
        )
        .unwrap();
        // The same message observation, without the bytes again.
        let observation = |fragment: &Fragment| {
            find!(
                observation: Id,
                pattern!(fragment, [{ ?observation @ metadata::tag: archive::kind_message }])
            )
            .collect::<BTreeSet<_>>()
        };
        assert_eq!(observation(&linked), observation(&downloaded));
        assert!(!exists!(pattern!(&linked, [{
            _?file @ metadata::tag: &KIND_FILE
        }])));
    }

    #[test]
    fn system_notices_are_marked_and_written_messages_are_not() {
        let channel = "100000000000000017";
        let mut pinned = message_json("100000000000000018", channel, "", None, json!([]));
        pinned["type"] = json!(6);
        let mut reply = message_json("100000000000000019", channel, "yes", None, json!([]));
        reply["type"] = json!(19);
        let plain = message_json("100000000000000020", channel, "hi", None, json!([]));
        let fragment = build_ingest_fragment(
            &parse_messages(vec![pinned, reply, plain], channel).unwrap(),
            None,
            None,
            |_, _| unreachable!("no attachments"),
        )
        .unwrap();
        let notices = find!(
            anchor: Id,
            pattern!(&fragment, [{
                _?notice @
                metadata::tag: discord::kind_system_notice,
                discord::message: ?anchor,
            }])
        )
        .collect::<Vec<_>>();
        let pinned_anchor = discord_model::message_anchor_fragment("100000000000000018")
            .unwrap()
            .root()
            .unwrap();
        assert_eq!(notices, [pinned_anchor]);
    }

    #[test]
    fn latest_official_edit_wins_and_divergent_maxima_are_exposed() {
        let directory = tempfile::tempdir().unwrap();
        let (pile, key) = fresh_storage(&directory);
        let owner = crate::storage::Storage::new(pile.clone(), Some(key.clone()));
        let storage = test_storage(&owner);
        let channel = "100000000000000007";
        let original = message_json("100000000000000008", channel, "original", None, json!([]));
        let edited = message_json(
            "100000000000000008",
            channel,
            "edited",
            Some("2026-08-07T09:00:00Z"),
            json!([]),
        );
        let messages = parse_messages(vec![original, edited], channel).unwrap();
        storage
            .publish(
                build_ingest_fragment(&messages, None, None, |_, _| unreachable!("no attachments"))
                    .unwrap(),
                "original and edit".to_owned(),
            )
            .unwrap();
        let view = storage.view().unwrap();
        let channel_id = discord_model::channel_fragment(channel)
            .unwrap()
            .root()
            .unwrap();
        let rows = discord_model::select_messages(&view.facts, Some(channel_id), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            discord_model::read_text(&view.reader, rows[0].content, "content").unwrap(),
            "edited"
        );

        let divergent = message_json(
            "100000000000000008",
            channel,
            "different at same edit time",
            Some("2026-08-07T09:00:00Z"),
            json!([]),
        );
        let messages = parse_messages(vec![divergent], channel).unwrap();
        storage
            .publish(
                build_ingest_fragment(&messages, None, None, |_, _| unreachable!("no attachments"))
                    .unwrap(),
                "divergent edit".to_owned(),
            )
            .unwrap();
        let view = storage.view().unwrap();
        let rows = discord_model::select_messages(&view.facts, Some(channel_id), None).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.variant_count == 2));
        let contents = rows
            .iter()
            .map(|row| discord_model::read_text(&view.reader, row.content, "content").unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            contents,
            BTreeSet::from([
                "different at same edit time".to_owned(),
                "edited".to_owned(),
            ])
        );
    }

    #[test]
    fn malformed_locally_supplied_ids_are_rejected() {
        assert!(discord_model::validate_snowflake("01").is_err());
        assert!(discord_model::validate_snowflake("not-an-id").is_err());
        assert!(discord_model::validate_snowflake("18446744073709551616").is_err());
    }
}
