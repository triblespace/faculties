//! Discord intake: the messages `discord live`'s gateway session reads, stored
//! in the discord faculty's own collection.
//!
//! Every message in the configured text channels, and in DMs to the bot when
//! asked, human or bot, is written through the same ingestion the faculty's
//! pull uses, so a gateway event, its replay after a resumed session and a
//! pull that overlaps it all converge on one observation. Nothing here decides
//! what deserves attention: orient reads the collection and does that. The
//! bot's own user id, from READY, is recorded before anything is pulled, so
//! orient can tell the bot's messages from everyone else's.
//!
//! The gateway only delivers what arrives while a session is up. After every
//! READY (the first one is the backfill on start) and RESUMED, each configured
//! channel and each DM channel seen before is therefore backfilled with the
//! faculty's coverage-based pull, and so again a minute after a live write or
//! a backfill failed, backing off to fifteen minutes while that goes on.
//!
//! Intake stores a channel from the moment it was first configured, and a DM
//! channel from the first message heard in it: the state directory keeps
//! that floor for each, and a channel's first pull reads forward from it
//! instead of bringing in its history, which would reach every window as
//! news. `discord read` is how history comes in when it is wanted.
//!
//! The pile and the downloads block, so intake runs on a thread of its own,
//! and nothing that happens there reaches the rest of the process: every
//! failure is logged, a panic included, and the work goes on.

use super::gateway::{DIRECT_MESSAGES, GUILD_MESSAGES, MESSAGE_CONTENT};
use crate::discord::{Discord, Source};
use anyhow::{Context, Result};
use serde_json::Value;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// The first wait before intake pulls again after a failure.
const RETRY_FIRST: Duration = Duration::from_secs(60);
/// The longest wait between such pulls.
const RETRY_LAST: Duration = Duration::from_secs(15 * 60);
/// Pulls of one channel per backfill, each up to the faculty's page budget.
const PULLS: usize = 5;
/// Discord's epoch, in milliseconds since the Unix epoch.
const DISCORD_EPOCH_MS: u128 = 1_420_070_400_000;

pub struct Intake {
    discord: Discord,
    source: Box<dyn Source + Send>,
    channels: Vec<NonZeroU64>,
    dms: bool,
    /// Where each channel's intake begins: `channels/<id>` for a configured
    /// channel and `dms/<id>` for a DM channel, each holding the message id
    /// the channel's first pull reads forward from.
    directory: PathBuf,
    /// The floor of a configured channel met for the first time.
    floor: u64,
    /// The bot's own user id, while it is not yet recorded.
    account: Option<String>,
}

/// One piece of work for the intake thread.
#[derive(Debug)]
pub enum Work {
    /// A MESSAGE_CREATE or MESSAGE_UPDATE dispatch's message object.
    Message(Value),
    /// The bot's own user id, from READY.
    Account(String),
    /// Pull every configured channel and every DM channel seen before.
    Backfill,
}

/// What one piece of work came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Done {
    Stored,
    /// A message outside the configured channels.
    Ignored,
    Recorded,
    /// Channels pulled, the failures among them, and whether any has more
    /// to read than this backfill took.
    Backfilled {
        channels: usize,
        failed: usize,
        more: bool,
    },
}

/// The floor from which every message sent at `time` or later is read: the
/// first Discord message id of that millisecond, less one.
pub fn floor_at(time: SystemTime) -> u64 {
    let milliseconds = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let since_epoch = u64::try_from(milliseconds.saturating_sub(DISCORD_EPOCH_MS)).unwrap_or(0);
    (since_epoch << 22).saturating_sub(1)
}

impl Intake {
    /// `floor` is where a configured channel met for the first time begins:
    /// [`floor_at`] the moment `discord live` starts.
    pub fn new(
        discord: Discord,
        source: Box<dyn Source + Send>,
        channels: Vec<NonZeroU64>,
        dms: bool,
        directory: PathBuf,
        floor: u64,
    ) -> Self {
        Self {
            discord,
            source,
            channels,
            dms,
            directory,
            floor,
            account: None,
        }
    }

    /// The gateway intents intake needs.
    pub fn intents(&self) -> u64 {
        let mut intents = 0;
        if !self.channels.is_empty() {
            intents |= GUILD_MESSAGES | MESSAGE_CONTENT;
        }
        if self.dms {
            intents |= DIRECT_MESSAGES;
        }
        intents
    }

    /// Prove that the discord collection opens and takes this signer's writes.
    pub fn preflight(&self) -> Result<()> {
        self.discord.preflight_write()
    }

    pub fn handle(&mut self, work: Work) -> Result<Done> {
        match work {
            Work::Message(message) => self.message(message),
            Work::Account(user) => {
                crate::discord::validate_snowflake(&user).context("the bot's user id")?;
                self.account = Some(user);
                self.record_account()?;
                Ok(Done::Recorded)
            }
            Work::Backfill => Ok(self.backfill()),
        }
    }

    fn record_account(&mut self) -> Result<()> {
        if let Some(user) = &self.account {
            self.discord.record_bot_account(user)?;
            self.account = None;
        }
        Ok(())
    }

    fn message(&mut self, message: Value) -> Result<Done> {
        let channel = snowflake(&message["channel_id"]).context("the message names its channel")?;
        // A message without a guild is a DM to the bot.
        let dm = message["guild_id"].is_null();
        if !(self.channels.iter().any(|id| id.get() == channel) || (self.dms && dm)) {
            return Ok(Done::Ignored);
        }
        if dm {
            // The first message heard in a DM channel is where its intake
            // begins; the channel is backfilled from then on.
            let id = snowflake(&message["id"]).context("the message has an id")?;
            self.floor_of("dms", channel, Some(id.saturating_sub(1)))?;
        }
        self.discord.observe(message, &mut *self.source)?;
        Ok(Done::Stored)
    }

    /// The floor kept for `channel` under `kind`, keeping `first` as it when
    /// there is none yet.
    fn floor_of(&self, kind: &str, channel: u64, first: Option<u64>) -> Result<Option<u64>> {
        let directory = self.directory.join(kind);
        let path = directory.join(channel.to_string());
        if let (Some(first), false) = (first, path.exists()) {
            std::fs::create_dir_all(&directory)
                .with_context(|| format!("create {}", directory.display()))?;
            // Written aside and renamed in, so a floor is never half there.
            let staging = directory.join(format!(".{channel}"));
            std::fs::write(&staging, first.to_string())
                .with_context(|| format!("write {}", staging.display()))?;
            std::fs::rename(&staging, &path)
                .with_context(|| format!("keep the floor in {}", path.display()))?;
        }
        Ok(std::fs::read_to_string(&path)
            .ok()
            .and_then(|floor| floor.trim().parse().ok()))
    }

    /// Every channel to backfill, with where its intake begins: the
    /// configured ones, then the DM channels seen before.
    fn backfill_channels(&self) -> Vec<(u64, Result<Option<u64>>)> {
        let mut channels: Vec<(u64, Result<Option<u64>>)> = self
            .channels
            .iter()
            .map(|id| {
                (
                    id.get(),
                    self.floor_of("channels", id.get(), Some(self.floor)),
                )
            })
            .collect();
        if self.dms {
            if let Ok(entries) = std::fs::read_dir(self.directory.join("dms")) {
                for entry in entries.flatten() {
                    let Some(id) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
                        continue;
                    };
                    if !channels.iter().any(|(channel, _)| *channel == id) {
                        channels.push((id, self.floor_of("dms", id, None)));
                    }
                }
            }
        }
        channels
    }

    fn backfill(&mut self) -> Done {
        let channels = self.backfill_channels();
        let count = channels.len();
        // Nothing is pulled before the bot's own account is known, or its
        // earlier messages would be stored as somebody else's.
        if let Err(error) = self.record_account() {
            eprintln!(
                "[discord] recording the bot account failed, so nothing is pulled: {error:#}"
            );
            return Done::Backfilled {
                channels: count,
                failed: count,
                more: false,
            };
        }
        let mut failed = 0;
        let mut more = false;
        for (channel, floor) in channels {
            let floor = match floor {
                Ok(floor) => floor,
                Err(error) => {
                    failed += 1;
                    eprintln!(
                        "[discord] where Discord channel {channel} begins is unknown: {error:#}"
                    );
                    continue;
                }
            };
            let mut stored = 0;
            for pull in 1..=PULLS {
                match self
                    .discord
                    .pull_channel(&channel.to_string(), floor, &mut *self.source)
                {
                    Ok(receipt) => {
                        stored += receipt.observations;
                        if !receipt.more {
                            break;
                        }
                        more |= pull == PULLS;
                    }
                    Err(error) => {
                        failed += 1;
                        eprintln!(
                            "[discord] backfilling Discord channel {channel} failed: {error:#}"
                        );
                        break;
                    }
                }
            }
            if stored > 0 {
                eprintln!("[discord] backfilled {stored} Discord messages in channel {channel}");
            }
        }
        Done::Backfilled {
            channels: count,
            failed,
            more,
        }
    }
}

/// When intake pulls again on its own: soon after a live write failed, and
/// again, each wait longer, while backfills fail or leave more to read; never
/// once a backfill caught up.
#[derive(Debug, Default)]
struct Retry {
    due: Option<Instant>,
    wait: Duration,
}

impl Retry {
    /// A live write failed: pull after the current wait, unless one is due.
    fn soon(&mut self, now: Instant) {
        self.due.get_or_insert(now + self.wait.max(RETRY_FIRST));
    }

    /// A backfill did not catch up: pull again after a longer wait.
    fn again(&mut self, now: Instant) {
        self.wait = (self.wait * 2).clamp(RETRY_FIRST, RETRY_LAST);
        self.due = Some(now + self.wait);
    }

    /// Everything is stored.
    fn caught_up(&mut self) {
        *self = Self::default();
    }

    /// The next piece of work: what arrives, or a backfill once one is due.
    /// None once the inbox is closed.
    fn next(&mut self, inbox: &Receiver<Work>) -> Option<Work> {
        let Some(due) = self.due else {
            return inbox.recv().ok();
        };
        match inbox.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(work) => Some(work),
            Err(RecvTimeoutError::Timeout) => {
                self.due = None;
                Some(Work::Backfill)
            }
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

/// The intake thread, fed through [`Worker::send`].
pub struct Worker {
    sender: Option<std::sync::mpsc::Sender<Work>>,
    /// Set when the process stops: queued backfills are passed over, since the
    /// next start pulls everything again anyway.
    stopping: Arc<AtomicBool>,
    /// Resolves when the thread has finished.
    finished: Option<tokio::sync::oneshot::Receiver<()>>,
    stopped: bool,
}

pub fn start(mut intake: Intake) -> Worker {
    let (sender, inbox) = std::sync::mpsc::channel::<Work>();
    let stopping = Arc::new(AtomicBool::new(false));
    let (done, finished) = tokio::sync::oneshot::channel();
    let passing = stopping.clone();
    std::thread::Builder::new()
        .name("discord-intake".to_owned())
        .spawn(move || {
            let mut retry = Retry::default();
            while let Some(work) = retry.next(&inbox) {
                if passing.load(Ordering::SeqCst) && matches!(work, Work::Backfill) {
                    continue;
                }
                let what = match &work {
                    Work::Message(message) => format!(
                        "Discord message {}",
                        message["id"].as_str().unwrap_or("without an id")
                    ),
                    Work::Account(user) => format!("the bot account {user}"),
                    Work::Backfill => "the backfill".to_owned(),
                };
                let handled =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| intake.handle(work)));
                match handled {
                    Ok(Ok(Done::Stored)) => eprintln!("[discord] stored {what}"),
                    Ok(Ok(Done::Backfilled {
                        channels,
                        failed,
                        more,
                    })) => {
                        if failed > 0 {
                            eprintln!("[discord] backfill: {failed} of {channels} channels failed");
                        }
                        if failed > 0 || more {
                            retry.again(Instant::now());
                        } else {
                            retry.caught_up();
                        }
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        eprintln!(
                            "[discord] storing {what} failed: {error:#}; a backfill retries it"
                        );
                        retry.soon(Instant::now());
                    }
                    Err(_) => {
                        eprintln!("[discord] storing {what} panicked; intake goes on");
                        retry.soon(Instant::now());
                    }
                }
            }
            let _ = done.send(());
        })
        .expect("spawn the intake thread");
    Worker {
        sender: Some(sender),
        stopping,
        finished: Some(finished),
        stopped: false,
    }
}

impl Worker {
    /// A worker whose work goes to `sender`, with no thread of its own.
    #[cfg(test)]
    pub fn from_sender(sender: std::sync::mpsc::Sender<Work>) -> Self {
        Self {
            sender: Some(sender),
            stopping: Arc::new(AtomicBool::new(false)),
            finished: None,
            stopped: false,
        }
    }

    pub fn send(&mut self, work: Work) {
        let sent = self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.send(work).is_ok());
        if !sent && !self.stopped {
            eprintln!("[discord] the intake thread has stopped; Discord messages are not stored");
            self.stopped = true;
        }
    }

    /// Let queued messages be stored, for at most `limit`; queued backfills
    /// are passed over. A thread still busy after that is left behind, and
    /// ends with the process.
    pub async fn stop(mut self, limit: Duration) {
        self.stopping.store(true, Ordering::SeqCst);
        drop(self.sender.take());
        let Some(finished) = self.finished.take() else {
            return;
        };
        if tokio::time::timeout(limit, finished).await.is_err() {
            eprintln!("[discord] intake still busy after {limit:?}; leaving it");
        }
    }
}

fn snowflake(value: &Value) -> Option<u64> {
    value.as_str().and_then(|id| id.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::{PageRequest, ReadOptions};
    use anyhow::anyhow;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    const CHANNEL: u64 = 100000000000000200;
    const GUILD: &str = "100000000000000300";
    const AUTHOR: &str = "100000000000000400";
    /// Where a configured channel met for the first time begins.
    const FLOOR: u64 = 100000000000000000;

    /// Discord without a network: pages of stored message objects per
    /// channel, paged as Discord pages them (`after` gives the messages
    /// right after it, every page newest first), and attachment bytes per
    /// URL. Every page request is recorded.
    #[derive(Clone, Default)]
    struct Fake {
        messages: Arc<Mutex<BTreeMap<String, Vec<Value>>>>,
        attachments: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
        pages: Arc<Mutex<Vec<(String, PageRequest)>>>,
    }

    impl Source for Fake {
        fn page(&mut self, channel_id: &str, request: PageRequest) -> Result<Vec<Value>> {
            self.pages
                .lock()
                .unwrap()
                .push((channel_id.to_owned(), request));
            let mut page: Vec<Value> = self
                .messages
                .lock()
                .unwrap()
                .get(channel_id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|message| {
                    let id = id_of(message);
                    request.after.is_none_or(|after| id > after)
                        && request.before.is_none_or(|before| id < before)
                })
                .collect();
            page.sort_by_key(id_of);
            let limit = request.limit as usize;
            let mut page = if request.after.is_some() {
                page.into_iter().take(limit).collect()
            } else {
                page.split_off(page.len().saturating_sub(limit))
            };
            page.reverse();
            Ok(page)
        }

        fn attachment(&mut self, url: &str, _limit: u64) -> Result<Vec<u8>> {
            self.attachments
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow!("no such attachment {url}"))
        }
    }

    fn id_of(message: &Value) -> u64 {
        message["id"].as_str().unwrap().parse().unwrap()
    }

    /// A message object as REST returns it.
    fn rest(id: &str, channel: u64, content: &str, attachment_url: Option<&str>) -> Value {
        let attachments = match attachment_url {
            Some(url) => json!([{
                "id": format!("{id}9"), "url": url, "filename": "photo.png",
                "content_type": "image/png", "size": 5
            }]),
            None => json!([]),
        };
        json!({
            "id": id,
            "channel_id": channel.to_string(),
            "type": 0,
            "content": content,
            "author": {"id": AUTHOR, "username": "ada", "global_name": "Ada"},
            // One second per message id, so the stored order is the id order.
            "timestamp": format!("2026-09-26T08:00:{:02}Z", id.parse::<u64>().unwrap() % 60),
            "edited_timestamp": null,
            "attachments": attachments,
            "referenced_message": null,
        })
    }

    /// The same message as a gateway MESSAGE_CREATE carries it: the guild,
    /// the author's member record and mentions besides, and the attachment
    /// behind a differently signed URL.
    fn gateway(message: &Value) -> Value {
        let mut message = message.clone();
        message["guild_id"] = json!(GUILD);
        message["member"] = json!({"roles": [], "joined_at": "2026-01-01T00:00:00Z"});
        message["mentions"] = json!([]);
        if let Some(attachment) = message["attachments"].get_mut(0) {
            attachment["url"] = json!("https://cdn.example/photo.png?ex=gateway");
        }
        message
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        discord: Discord,
        fake: Fake,
        state: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            crate::test_support::clear_ambient_environment();
            let directory = tempfile::tempdir().unwrap();
            let pile = directory.path().join("intake.pile");
            let key = directory.path().join("intake.key");
            std::fs::File::create(&pile).unwrap();
            crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
            let fake = Fake::default();
            for (url, bytes) in [
                ("https://cdn.example/photo.png?ex=gateway", b"image"),
                ("https://cdn.example/photo.png?ex=rest", b"image"),
            ] {
                fake.attachments
                    .lock()
                    .unwrap()
                    .insert(url.to_owned(), bytes.to_vec());
            }
            Self {
                discord: Discord::new(pile, Some(key)),
                fake,
                state: directory.path().join("intake"),
                _directory: directory,
            }
        }

        fn intake(&self, channels: &[u64], dms: bool) -> Intake {
            Intake::new(
                self.discord.clone(),
                Box::new(self.fake.clone()),
                channels
                    .iter()
                    .map(|id| NonZeroU64::new(*id).unwrap())
                    .collect(),
                dms,
                self.state.clone(),
                FLOOR,
            )
        }

        fn serve(&self, channel: u64, messages: Vec<Value>) {
            self.fake
                .messages
                .lock()
                .unwrap()
                .insert(channel.to_string(), messages);
        }

        /// Every stored message in `channel`: (content, attachments, variants).
        fn stored(&self, channel: u64) -> Vec<(String, usize, usize)> {
            self.discord
                .read(ReadOptions {
                    channel_id: Some(channel.to_string()),
                    limit: 0,
                    ..ReadOptions::default()
                })
                .unwrap()
                .messages
                .into_iter()
                .map(|message| {
                    (
                        message.content,
                        message.attachments.len(),
                        message.variant_count,
                    )
                })
                .collect()
        }
    }

    #[test]
    fn a_gateway_message_is_stored_once_under_replay_and_an_overlapping_pull() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], false);
        assert_eq!(intake.intents(), GUILD_MESSAGES | MESSAGE_CONTENT);
        let first = rest(
            "100000000000000501",
            CHANNEL,
            "look at this",
            Some("https://cdn.example/photo.png?ex=rest"),
        );
        // What the gateway adds, and the attachment's signed URL, are not
        // part of the message: both shapes are one observation.
        let bytes = |_: &str, _: u64| Ok(b"image".to_vec());
        assert_eq!(
            crate::discord::operations::observed_fragment(gateway(&first), bytes).unwrap(),
            crate::discord::operations::observed_fragment(first.clone(), bytes).unwrap()
        );
        assert_eq!(
            intake.handle(Work::Message(gateway(&first))).unwrap(),
            Done::Stored
        );
        assert_eq!(fixture.stored(CHANNEL), [("look at this".to_owned(), 1, 1)]);

        // A resumed session replays the event: nothing new.
        assert_eq!(
            intake.handle(Work::Message(gateway(&first))).unwrap(),
            Done::Stored
        );
        assert_eq!(fixture.stored(CHANNEL), [("look at this".to_owned(), 1, 1)]);

        // A backfill whose REST pages overlap the live message converges on
        // the same observation, and adds only what the gateway missed.
        let second = rest("100000000000000502", CHANNEL, "and this", None);
        fixture.serve(CHANNEL, vec![first.clone(), second]);
        assert_eq!(
            intake.handle(Work::Backfill).unwrap(),
            Done::Backfilled {
                channels: 1,
                failed: 0,
                more: false
            }
        );
        assert_eq!(
            fixture.stored(CHANNEL),
            [
                ("look at this".to_owned(), 1, 1),
                ("and this".to_owned(), 0, 1)
            ]
        );
        // The next backfill starts after the covered frontier, and the same
        // messages once more change nothing.
        intake.handle(Work::Backfill).unwrap();
        let pages = fixture.fake.pages.lock().unwrap().clone();
        assert_eq!(
            pages[0].1.after,
            Some(FLOOR),
            "the first pull reads forward from the channel's floor"
        );
        assert!(pages
            .iter()
            .any(|(_, request)| request.after == Some(100000000000000502)));
        assert_eq!(fixture.stored(CHANNEL).len(), 2);
    }

    #[test]
    fn only_configured_channels_and_asked_for_dms_are_stored() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], false);
        let elsewhere = rest("100000000000000601", CHANNEL + 1, "elsewhere", None);
        assert_eq!(
            intake.handle(Work::Message(gateway(&elsewhere))).unwrap(),
            Done::Ignored
        );
        let dm = rest("100000000000000602", CHANNEL + 2, "a DM", None);
        assert_eq!(
            intake.handle(Work::Message(dm.clone())).unwrap(),
            Done::Ignored
        );
        assert!(fixture.stored(CHANNEL + 2).is_empty());

        // With DMs asked for, a DM is stored and its channel remembered, so a
        // later backfill (after a restart, say) pulls it too.
        let mut intake = fixture.intake(&[CHANNEL], true);
        assert_eq!(
            intake.intents(),
            GUILD_MESSAGES | MESSAGE_CONTENT | DIRECT_MESSAGES
        );
        assert_eq!(intake.handle(Work::Message(dm)).unwrap(), Done::Stored);
        assert_eq!(fixture.stored(CHANNEL + 2), [("a DM".to_owned(), 0, 1)]);
        let mut restarted = fixture.intake(&[CHANNEL], true);
        restarted.handle(Work::Backfill).unwrap();
        let pulled: Vec<String> = fixture
            .fake
            .pages
            .lock()
            .unwrap()
            .iter()
            .map(|(channel, _)| channel.clone())
            .collect();
        assert!(pulled.contains(&CHANNEL.to_string()));
        assert!(pulled.contains(&(CHANNEL + 2).to_string()));
        assert!(!pulled.contains(&(CHANNEL + 1).to_string()));
        // DMs alone ask for no privileged intent.
        assert_eq!(fixture.intake(&[], true).intents(), DIRECT_MESSAGES);
    }

    #[test]
    fn a_message_whose_attachment_cannot_be_fetched_waits_for_the_backfill() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], false);
        let message = rest(
            "100000000000000701",
            CHANNEL,
            "with a file",
            Some("https://cdn.example/photo.png?ex=rest"),
        );
        let mut live = gateway(&message);
        live["attachments"][0]["url"] = json!("https://cdn.example/expired");
        assert!(intake.handle(Work::Message(live)).is_err());
        assert!(fixture.stored(CHANNEL).is_empty());
        fixture.serve(CHANNEL, vec![message]);
        intake.handle(Work::Backfill).unwrap();
        assert_eq!(fixture.stored(CHANNEL), [("with a file".to_owned(), 1, 1)]);
    }

    /// A channel's history from before intake met it stays out (it would all
    /// be news); what came after, the process up or not, comes in. A DM channel
    /// begins with the first message heard in it.
    #[test]
    fn a_channel_is_stored_from_its_floor_and_a_dm_channel_from_its_first_message() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], true);
        fixture.serve(
            CHANNEL,
            vec![
                rest(&(FLOOR - 2).to_string(), CHANNEL, "history", None),
                rest(&(FLOOR - 1).to_string(), CHANNEL, "more history", None),
                rest(&(FLOOR + 1).to_string(), CHANNEL, "while down", None),
            ],
        );
        let dm = CHANNEL + 2;
        let heard = rest(&(FLOOR + 10).to_string(), dm, "hello bot", None);
        fixture.serve(
            dm,
            vec![
                rest(&(FLOOR + 5).to_string(), dm, "an old DM", None),
                heard.clone(),
                rest(&(FLOOR + 11).to_string(), dm, "while down", None),
            ],
        );
        assert_eq!(intake.handle(Work::Message(heard)).unwrap(), Done::Stored);
        // A restarted process keeps the floors it began with.
        let mut restarted = fixture.intake(&[CHANNEL], true);
        restarted.handle(Work::Backfill).unwrap();
        assert_eq!(fixture.stored(CHANNEL), [("while down".to_owned(), 0, 1)]);
        assert_eq!(
            fixture.stored(dm),
            [
                ("hello bot".to_owned(), 0, 1),
                ("while down".to_owned(), 0, 1)
            ]
        );
    }

    /// A gap far longer than one page (a long outage) closes in one
    /// backfill, and every message in it is stored.
    #[test]
    fn a_long_gap_is_closed_by_one_backfill() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], false);
        let messages: Vec<Value> = (1..=350)
            .map(|n| rest(&(FLOOR + n).to_string(), CHANNEL, &format!("m{n}"), None))
            .collect();
        fixture.serve(CHANNEL, messages[..1].to_vec());
        intake.handle(Work::Backfill).unwrap();
        assert_eq!(fixture.stored(CHANNEL).len(), 1);
        fixture.serve(CHANNEL, messages);
        assert_eq!(
            intake.handle(Work::Backfill).unwrap(),
            Done::Backfilled {
                channels: 1,
                failed: 0,
                more: false
            }
        );
        assert_eq!(fixture.stored(CHANNEL).len(), 350);
    }

    #[test]
    fn failures_bring_the_next_backfill_forward_and_back_off() {
        let start = Instant::now();
        let mut retry = Retry::default();
        retry.soon(start);
        assert_eq!(retry.due, Some(start + RETRY_FIRST));
        // More failed writes do not push it further away.
        retry.soon(start + Duration::from_secs(30));
        assert_eq!(retry.due, Some(start + RETRY_FIRST));
        let mut waits = Vec::new();
        for _ in 0..6 {
            retry.again(start);
            waits.push((retry.due.unwrap() - start).as_secs());
        }
        assert_eq!(waits, [60, 120, 240, 480, 900, 900]);
        retry.caught_up();
        assert_eq!(retry.due, None);

        // A due backfill arrives as work when nothing else does.
        let (sender, inbox) = std::sync::mpsc::channel();
        retry.due = Some(Instant::now());
        assert!(matches!(retry.next(&inbox), Some(Work::Backfill)));
        assert_eq!(retry.due, None);
        sender.send(Work::Account("1".to_owned())).unwrap();
        assert!(matches!(retry.next(&inbox), Some(Work::Account(_))));
        drop(sender);
        assert!(retry.next(&inbox).is_none());
    }

    /// Discord that takes its time: every page request waits.
    struct Slow;

    impl Source for Slow {
        fn page(&mut self, _: &str, _: PageRequest) -> Result<Vec<Value>> {
            std::thread::sleep(Duration::from_secs(3));
            Ok(Vec::new())
        }

        fn attachment(&mut self, _: &str, _: u64) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn stopping_is_bounded_while_a_backfill_runs() {
        let fixture = Fixture::new();
        let intake = Intake::new(
            fixture.discord.clone(),
            Box::new(Slow),
            vec![NonZeroU64::new(CHANNEL).unwrap()],
            false,
            fixture.state.clone(),
            FLOOR,
        );
        let mut worker = start(intake);
        worker.send(Work::Backfill);
        worker.send(Work::Backfill);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stopping = Instant::now();
        worker.stop(Duration::from_millis(200)).await;
        assert!(stopping.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn the_bot_account_is_recorded() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], false);
        assert_eq!(
            intake
                .handle(Work::Account("100000000000000800".to_owned()))
                .unwrap(),
            Done::Recorded
        );
        assert!(intake
            .handle(Work::Account("not a snowflake".to_owned()))
            .is_err());
    }
}
