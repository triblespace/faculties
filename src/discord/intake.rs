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
//! that floor for each, and nothing at or below it is stored, whether a pull
//! or the gateway brings it (an edit of an older message, the recent page a
//! backfill reconciles), since a channel's history would reach every window
//! as news. A floor that is there but cannot be read passes its channel over
//! rather than reading its history. `discord read` is how history comes in
//! when it is wanted.
//!
//! A message whose live write failed, for want of its channel's floor too, is
//! kept by id in the state directory (`intake/unstored`), and every backfill
//! fetches it again by that id until it is stored, Discord no longer has it
//! or its channel is no longer intake's: the recent page a backfill
//! reconciles need not reach back to it.
//!
//! The pile and the downloads block, so intake runs on a thread of its own,
//! and nothing that happens there reaches the rest of the process: every
//! failure is logged, a panic included, and the work goes on.

use super::gateway::{DIRECT_MESSAGES, GUILD_MESSAGES, MESSAGE_CONTENT};
use crate::discord::{Discord, PageRequest, Source};
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
/// Where intake keeps the messages whose live write failed, one file
/// `<channel>-<message>` each: [`NEW_DM`] for a new message in a DM channel,
/// empty for any other.
const UNSTORED: &str = "unstored";
/// What the file of a kept new message in a DM channel holds.
const NEW_DM: &str = "new DM";

pub struct Intake {
    discord: Discord,
    source: Box<dyn Source + Send>,
    channels: Vec<NonZeroU64>,
    dms: bool,
    /// Where each channel's intake begins: `channels/<id>` for a configured
    /// channel and `dms/<id>` for a DM channel, each holding the message id
    /// nothing at or below which is stored; and `unstored/<channel>-<id>`
    /// for each message whose live write failed.
    directory: PathBuf,
    /// The floor of a configured channel met for the first time.
    floor: u64,
    /// The bot's own user id, while it is not yet recorded.
    account: Option<String>,
}

/// One piece of work for the intake thread.
#[derive(Debug)]
pub enum Work {
    /// A MESSAGE_CREATE dispatch's message object.
    Message(Value),
    /// A MESSAGE_UPDATE dispatch's message object: an edit, possibly of a
    /// message older than where its channel's intake begins.
    Update(Value),
    /// The bot's own user id, from READY.
    Account(String),
    /// Pull every configured channel and every DM channel seen before.
    Backfill,
}

/// What one piece of work came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Done {
    Stored,
    /// A message outside the configured channels, or from before where its
    /// channel's intake begins.
    Ignored,
    Recorded,
    /// Channels pulled, the failures among them, whether any has more to
    /// read than this backfill took, and how many messages whose live write
    /// failed are still not stored.
    Backfilled {
        channels: usize,
        failed: usize,
        more: bool,
        unstored: usize,
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
            Work::Message(message) => self.message(message, true),
            Work::Update(message) => self.message(message, false),
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

    /// Store a message the gateway delivered: `created` for a new one, else
    /// an edit.
    fn message(&mut self, message: Value, created: bool) -> Result<Done> {
        let channel = snowflake(&message["channel_id"]).context("the message names its channel")?;
        let id = snowflake(&message["id"]).context("the message has an id")?;
        // A message without a guild is a DM to the bot. The first message
        // heard in a DM channel is where its intake begins; an edit of an
        // older one is not.
        let dm = message["guild_id"].is_null();
        let new_dm = dm && created;
        let first = created.then(|| id.saturating_sub(1));
        let floor = match self.floor_for(channel, dm, first) {
            Ok(Some(floor)) => floor,
            Ok(None) => return Ok(Done::Ignored),
            // The channel is intake's (nothing else reads a floor), but where
            // it begins cannot be read or kept: the message is kept by id and
            // fetched again once it can, where the floor is checked again.
            Err(error) => {
                self.keep(channel, id, new_dm);
                return Err(error);
            }
        };
        self.store(message, channel, id, floor, new_dm)
    }

    /// Store one message of `channel` unless it is at or below the channel's
    /// floor. A failed write is kept by id, to be fetched again; a stored
    /// message is no longer kept.
    fn store(
        &mut self,
        message: Value,
        channel: u64,
        id: u64,
        floor: u64,
        new_dm: bool,
    ) -> Result<Done> {
        if id <= floor {
            return Ok(Done::Ignored);
        }
        match self.discord.observe(message, &mut *self.source) {
            Ok(_) => {
                let marker = self.marker(channel, id);
                match std::fs::remove_file(&marker) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => eprintln!(
                        "[discord] message {id} is stored, but {} stays: {error}",
                        marker.display()
                    ),
                }
                Ok(Done::Stored)
            }
            Err(error) => {
                self.keep(channel, id, new_dm);
                Err(error)
            }
        }
    }

    /// Where a message whose live write failed is kept.
    fn marker(&self, channel: u64, id: u64) -> PathBuf {
        self.directory
            .join(UNSTORED)
            .join(format!("{channel}-{id}"))
    }

    /// Keep a message whose live write failed, to be fetched again by its
    /// id. A new message in a DM channel says so (`new_dm`): the first one
    /// of a DM channel whose floor could not be kept then still begins it
    /// when it is fetched again, and an edit heard later does not take that
    /// away. Only a DM's can: a channel intake no longer has configured is
    /// taken for a DM channel when its message is fetched again, and must
    /// not begin as one.
    fn keep(&self, channel: u64, id: u64, new_dm: bool) {
        use std::io::Write;
        let marker = self.marker(channel, id);
        let kept = std::fs::create_dir_all(self.directory.join(UNSTORED)).and_then(|()| {
            let mut file = std::fs::OpenOptions::new();
            if new_dm {
                file.write(true).create(true).truncate(true);
            } else {
                file.write(true).create_new(true);
            }
            match file.open(&marker) {
                Ok(mut file) if new_dm => file.write_all(NEW_DM.as_bytes()),
                Ok(_) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(error) => Err(error),
            }
        });
        if let Err(keeping) = kept {
            eprintln!(
                "[discord] message {id} in channel {channel} is not kept to be fetched again \
                 ({}: {keeping}); only a backfill's pages may bring it",
                marker.display()
            );
        }
    }

    /// Where intake stores `channel` from, or None where it does not store
    /// it: a configured channel's floor, kept the first time it is met; a DM
    /// channel's (with DMs asked for), kept at `first` when no message was
    /// heard in it before.
    fn floor_for(&self, channel: u64, dm: bool, first: Option<u64>) -> Result<Option<u64>> {
        if self.channels.iter().any(|id| id.get() == channel) {
            return self.keep_floor("channels", channel, self.floor).map(Some);
        }
        if !(self.dms && dm) {
            return Ok(None);
        }
        match (self.floor("dms", channel)?, first) {
            (Some(floor), _) => Ok(Some(floor)),
            (None, Some(first)) => self.keep_floor("dms", channel, first).map(Some),
            (None, None) => Ok(None),
        }
    }

    /// The floor kept for `channel` under `kind`, None while there is none.
    /// A floor that is there but cannot be read is an error, never "none":
    /// the channel is then passed over, not read from its history.
    fn floor(&self, kind: &str, channel: u64) -> Result<Option<u64>> {
        let path = self.directory.join(kind).join(channel.to_string());
        match std::fs::read_to_string(&path) {
            Ok(floor) => floor
                .trim()
                .parse()
                .map(Some)
                .with_context(|| format!("{} holds no message id", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    /// The floor kept for `channel` under `kind`, keeping `first` as it when
    /// there is none yet.
    fn keep_floor(&self, kind: &str, channel: u64, first: u64) -> Result<u64> {
        if let Some(floor) = self.floor(kind, channel)? {
            return Ok(floor);
        }
        let directory = self.directory.join(kind);
        let path = directory.join(channel.to_string());
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("create {}", directory.display()))?;
        // Written aside and renamed in, so a floor is never half there.
        let staging = directory.join(format!(".{channel}"));
        std::fs::write(&staging, first.to_string())
            .with_context(|| format!("write {}", staging.display()))?;
        std::fs::rename(&staging, &path)
            .with_context(|| format!("keep the floor in {}", path.display()))?;
        Ok(first)
    }

    /// Every channel to backfill, with where its intake begins: the
    /// configured ones, then the DM channels seen before; and whether the
    /// DM channels seen before could not be listed, which the backfill
    /// counts as one channel that failed.
    fn backfill_channels(&self) -> (Vec<(u64, Result<u64>)>, bool) {
        let mut channels: Vec<(u64, Result<u64>)> = self
            .channels
            .iter()
            .map(|id| (id.get(), self.keep_floor("channels", id.get(), self.floor)))
            .collect();
        let mut unlisted = false;
        if self.dms {
            let directory = self.directory.join("dms");
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries.flatten().collect(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => {
                    eprintln!(
                        "[discord] the DM channels seen before are unknown ({}: {error}); \
                         none is backfilled",
                        directory.display()
                    );
                    unlisted = true;
                    Vec::new()
                }
            };
            for entry in entries {
                let Some(id) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
                    continue;
                };
                if channels.iter().any(|(channel, _)| *channel == id) {
                    continue;
                }
                match self.floor("dms", id) {
                    Ok(Some(floor)) => channels.push((id, Ok(floor))),
                    Ok(None) => {}
                    Err(error) => channels.push((id, Err(error))),
                }
            }
        }
        (channels, unlisted)
    }

    /// Fetch every message whose live write failed again, by its id, and
    /// store it; one Discord no longer has, or that is not intake's to
    /// store, is let go. How many are still not stored.
    fn retry_unstored(&mut self) -> usize {
        let directory = self.directory.join(UNSTORED);
        let entries: Vec<_> = match std::fs::read_dir(&directory) {
            Ok(entries) => entries.flatten().collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
            Err(error) => {
                eprintln!(
                    "[discord] the messages to fetch again are unknown ({}: {error})",
                    directory.display()
                );
                return 1;
            }
        };
        let mut unstored = 0;
        for entry in entries {
            let name = entry.file_name();
            let Some((channel, id)) = name
                .to_str()
                .and_then(|name| name.split_once('-'))
                .and_then(|(channel, id)| Some((channel.parse().ok()?, id.parse().ok()?)))
            else {
                continue;
            };
            // A marker that cannot be read is taken for an edit's: it begins
            // no DM channel.
            let new_dm =
                std::fs::read_to_string(entry.path()).is_ok_and(|content| content.trim() == NEW_DM);
            match self.refetch(channel, id, new_dm) {
                Ok(Done::Stored) => {
                    eprintln!("[discord] stored Discord message {id}, fetched again");
                }
                Ok(_) => {
                    eprintln!(
                        "[discord] Discord message {id} in channel {channel} is gone or not \
                         intake's to store; not fetched again"
                    );
                    if let Err(error) = std::fs::remove_file(entry.path()) {
                        eprintln!(
                            "[discord] removing {} failed: {error}",
                            entry.path().display()
                        );
                    }
                }
                Err(error) => {
                    unstored += 1;
                    eprintln!(
                        "[discord] fetching Discord message {id} in channel {channel} again \
                         failed: {error:#}"
                    );
                }
            }
        }
        unstored
    }

    /// One message fetched again by its id (the one page of one message
    /// right after the id before it) and stored as the gateway's would be:
    /// Ignored when it is not intake's to store or Discord no longer has it.
    fn refetch(&mut self, channel: u64, id: u64, new_dm: bool) -> Result<Done> {
        // Whether the channel is still intake's comes first, so a message in
        // a channel intake no longer serves is let go without asking Discord,
        // which may refuse that channel. REST names no guild: a channel intake
        // does not have configured is a DM channel, stored once a message was
        // heard in it, or from this one when it was the first.
        let first = new_dm.then(|| id.saturating_sub(1));
        let Some(floor) = self.floor_for(channel, true, first)? else {
            return Ok(Done::Ignored);
        };
        if id <= floor {
            return Ok(Done::Ignored);
        }
        let page = self.source.page(
            &channel.to_string(),
            PageRequest {
                after: Some(id.saturating_sub(1)),
                before: None,
                limit: 1,
            },
        )?;
        let Some(message) = page
            .into_iter()
            .find(|message| snowflake(&message["id"]) == Some(id))
        else {
            return Ok(Done::Ignored);
        };
        self.store(message, channel, id, floor, new_dm)
    }

    fn backfill(&mut self) -> Done {
        let (channels, unlisted) = self.backfill_channels();
        let count = channels.len() + usize::from(unlisted);
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
                unstored: 0,
            };
        }
        let mut failed = usize::from(unlisted);
        let mut more = false;
        for (channel, floor) in channels {
            let floor = match floor {
                Ok(floor) => floor,
                Err(error) => {
                    failed += 1;
                    eprintln!(
                        "[discord] where Discord channel {channel} begins is unknown, so it \
                         is passed over: {error:#}"
                    );
                    continue;
                }
            };
            let mut stored = 0;
            for pull in 1..=PULLS {
                match self.discord.pull_channel(
                    &channel.to_string(),
                    Some(floor),
                    &mut *self.source,
                ) {
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
            unstored: self.retry_unstored(),
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
                    Work::Update(message) => format!(
                        "the edit of Discord message {}",
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
                        unstored,
                    })) => {
                        if failed > 0 {
                            eprintln!("[discord] backfill: {failed} of {channels} channels failed");
                        }
                        if unstored > 0 {
                            eprintln!(
                                "[discord] backfill: {unstored} messages whose live write \
                                 failed are still not stored"
                            );
                        }
                        if failed > 0 || more || unstored > 0 {
                            retry.again(Instant::now());
                        } else {
                            retry.caught_up();
                        }
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        eprintln!(
                            "[discord] storing {what} failed: {error:#}; a backfill fetches it \
                             again"
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
                more: false,
                unstored: 0
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

    /// Nothing at or below a channel's floor is stored, however it comes:
    /// the recent page every backfill after the first reconciles reaches
    /// back past the floor in a quiet channel, and the gateway delivers edits
    /// of older messages. A DM channel heard of first through such an edit
    /// has not begun.
    #[test]
    fn nothing_at_or_below_a_floor_is_stored_on_any_path() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], true);
        let dm = CHANNEL + 2;
        fixture.serve(
            CHANNEL,
            vec![
                rest(&(FLOOR - 2).to_string(), CHANNEL, "history", None),
                rest(&(FLOOR - 1).to_string(), CHANNEL, "more history", None),
                rest(&(FLOOR + 1).to_string(), CHANNEL, "after", None),
            ],
        );
        let heard = rest(&(FLOOR + 10).to_string(), dm, "hello bot", None);
        fixture.serve(
            dm,
            vec![
                rest(&(FLOOR + 5).to_string(), dm, "an old DM", None),
                heard.clone(),
            ],
        );
        assert_eq!(intake.handle(Work::Message(heard)).unwrap(), Done::Stored);
        for _ in 0..3 {
            intake.handle(Work::Backfill).unwrap();
        }
        let reconciled = |channel: u64| {
            fixture
                .fake
                .pages
                .lock()
                .unwrap()
                .iter()
                .any(|(id, request)| {
                    *id == channel.to_string()
                        && request.after.is_none()
                        && request.before.is_none()
                })
        };
        assert!(reconciled(CHANNEL) && reconciled(dm));
        assert_eq!(fixture.stored(CHANNEL), [("after".to_owned(), 0, 1)]);
        assert_eq!(fixture.stored(dm), [("hello bot".to_owned(), 0, 1)]);

        // Edits of older messages, over the gateway.
        let edited = |id: u64, channel: u64, content: &str| {
            let mut message = rest(&id.to_string(), channel, content, None);
            message["edited_timestamp"] = json!("2026-09-26T09:00:00Z");
            message
        };
        let history = gateway(&edited(FLOOR - 1, CHANNEL, "more history, edited"));
        assert_eq!(intake.handle(Work::Update(history)).unwrap(), Done::Ignored);
        let old_dm = edited(FLOOR + 5, dm, "an old DM, edited");
        assert_eq!(intake.handle(Work::Update(old_dm)).unwrap(), Done::Ignored);
        assert_eq!(fixture.stored(CHANNEL), [("after".to_owned(), 0, 1)]);
        assert_eq!(fixture.stored(dm), [("hello bot".to_owned(), 0, 1)]);

        let other = CHANNEL + 3;
        fixture.serve(
            other,
            vec![
                rest(&(FLOOR + 12).to_string(), other, "long ago", None),
                rest(&(FLOOR + 13).to_string(), other, "later", None),
            ],
        );
        let old = edited(FLOOR + 12, other, "long ago, edited");
        assert_eq!(intake.handle(Work::Update(old)).unwrap(), Done::Ignored);
        intake.handle(Work::Backfill).unwrap();
        assert!(fixture.stored(other).is_empty());
        assert!(!fixture.state.join("dms").join(other.to_string()).exists());
    }

    /// Coverage a pull asked for brought in below a channel's floor, before
    /// intake met the channel, does not move where intake begins.
    #[test]
    fn older_coverage_does_not_lower_a_floor() {
        let fixture = Fixture::new();
        let pulled = vec![
            rest(&(FLOOR - 5).to_string(), CHANNEL, "pulled", None),
            rest(&(FLOOR - 4).to_string(), CHANNEL, "pulled too", None),
        ];
        fixture.serve(CHANNEL, pulled.clone());
        fixture
            .discord
            .pull_channel(&CHANNEL.to_string(), None, &mut fixture.fake.clone())
            .unwrap();
        let mut later = pulled;
        later.extend([
            rest(&(FLOOR - 2).to_string(), CHANNEL, "history", None),
            rest(&(FLOOR - 1).to_string(), CHANNEL, "more history", None),
            rest(&(FLOOR + 1).to_string(), CHANNEL, "after", None),
        ]);
        fixture.serve(CHANNEL, later);
        let mut intake = fixture.intake(&[CHANNEL], false);
        intake.handle(Work::Backfill).unwrap();
        intake.handle(Work::Backfill).unwrap();
        assert_eq!(
            fixture.stored(CHANNEL),
            [
                ("pulled".to_owned(), 0, 1),
                ("pulled too".to_owned(), 0, 1),
                ("after".to_owned(), 0, 1)
            ]
        );
    }

    /// A floor that is there but holds no message id passes its channel
    /// over, on a backfill and live alike, instead of reading its history.
    #[test]
    fn an_unreadable_floor_passes_its_channel_over() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], true);
        let dm = CHANNEL + 2;
        for (kind, channel, floor) in [("channels", CHANNEL, "not a message id"), ("dms", dm, "")] {
            let directory = fixture.state.join(kind);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join(channel.to_string()), floor).unwrap();
        }
        for channel in [CHANNEL, dm] {
            fixture.serve(
                channel,
                vec![
                    rest(&(FLOOR - 1).to_string(), channel, "history", None),
                    rest(&(FLOOR + 1).to_string(), channel, "after", None),
                ],
            );
        }
        assert_eq!(
            intake.handle(Work::Backfill).unwrap(),
            Done::Backfilled {
                channels: 2,
                failed: 2,
                more: false,
                unstored: 0
            }
        );
        let live = gateway(&rest(&(FLOOR + 2).to_string(), CHANNEL, "live", None));
        assert!(intake.handle(Work::Message(live)).is_err());
        let live_dm = rest(&(FLOOR + 3).to_string(), dm, "live DM", None);
        assert!(intake.handle(Work::Message(live_dm)).is_err());
        assert!(fixture.fake.pages.lock().unwrap().is_empty());
        assert!(fixture.stored(CHANNEL).is_empty());
        assert!(fixture.stored(dm).is_empty());
        assert_eq!(
            std::fs::read_to_string(fixture.state.join("channels").join(CHANNEL.to_string()))
                .unwrap(),
            "not a message id"
        );
    }

    /// A live write that failed is fetched again by its message id, even an
    /// edit of a message older than the recent page a backfill reconciles,
    /// and backfills go on until it is stored; one Discord no longer has is
    /// let go.
    #[test]
    fn a_failed_live_write_is_fetched_again_by_its_id() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], false);
        let mut messages: Vec<Value> = (1..=160)
            .map(|n| rest(&(FLOOR + n).to_string(), CHANNEL, &format!("m{n}"), None))
            .collect();
        fixture.serve(CHANNEL, messages.clone());
        let caught_up = Done::Backfilled {
            channels: 1,
            failed: 0,
            more: false,
            unstored: 0,
        };
        assert_eq!(intake.handle(Work::Backfill).unwrap(), caught_up);

        // Message 101 is edited to carry a photo, and the live write fails
        // on the photo's expired URL.
        let mut edited = rest(
            &(FLOOR + 101).to_string(),
            CHANNEL,
            "m101, edited",
            Some("https://cdn.example/photo.png?ex=rest"),
        );
        edited["edited_timestamp"] = json!("2026-09-26T09:00:00Z");
        let mut live = gateway(&edited);
        live["attachments"][0]["url"] = json!("https://cdn.example/expired");
        assert!(intake.handle(Work::Update(live)).is_err());
        messages[100] = edited;
        fixture.serve(CHANNEL, messages);

        // Fetched again while the photo still cannot be had: not caught up.
        let photo = fixture
            .fake
            .attachments
            .lock()
            .unwrap()
            .remove("https://cdn.example/photo.png?ex=rest")
            .unwrap();
        assert_eq!(
            intake.handle(Work::Backfill).unwrap(),
            Done::Backfilled {
                channels: 1,
                failed: 0,
                more: false,
                unstored: 1
            }
        );
        fixture
            .fake
            .attachments
            .lock()
            .unwrap()
            .insert("https://cdn.example/photo.png?ex=rest".to_owned(), photo);
        assert_eq!(intake.handle(Work::Backfill).unwrap(), caught_up);
        assert!(fixture
            .stored(CHANNEL)
            .contains(&("m101, edited".to_owned(), 1, 1)));
        let by_id = PageRequest {
            after: Some(FLOOR + 100),
            before: None,
            limit: 1,
        };
        let fetched = || {
            fixture
                .fake
                .pages
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, request)| *request == by_id)
                .count()
        };
        assert_eq!(fetched(), 2);
        intake.handle(Work::Backfill).unwrap();
        assert_eq!(fetched(), 2, "a stored message is not fetched again");

        // A message whose write failed and that Discord then deleted.
        let mut gone = gateway(&rest(
            &(FLOOR + 161).to_string(),
            CHANNEL,
            "gone",
            Some("https://cdn.example/expired"),
        ));
        gone["attachments"][0]["url"] = json!("https://cdn.example/expired");
        assert!(intake.handle(Work::Message(gone)).is_err());
        assert_eq!(intake.handle(Work::Backfill).unwrap(), caught_up);
        assert!(std::fs::read_dir(fixture.state.join(UNSTORED))
            .unwrap()
            .next()
            .is_none());
    }

    /// A message kept to be fetched again in a channel intake no longer
    /// serves is let go without asking Discord, which may refuse that
    /// channel for good: the backfill catches up instead of repeating.
    #[test]
    fn a_kept_message_in_a_channel_no_longer_served_is_let_go() {
        struct Refusing {
            inner: Fake,
            refused: String,
        }
        impl Source for Refusing {
            fn page(&mut self, channel_id: &str, request: PageRequest) -> Result<Vec<Value>> {
                anyhow::ensure!(channel_id != self.refused, "403 Missing Access");
                self.inner.page(channel_id, request)
            }
            fn attachment(&mut self, url: &str, limit: u64) -> Result<Vec<u8>> {
                self.inner.attachment(url, limit)
            }
        }
        let fixture = Fixture::new();
        let other = CHANNEL + 7;
        let mut intake = fixture.intake(&[CHANNEL, other], false);
        fixture.serve(
            CHANNEL,
            vec![rest(&(FLOOR + 1).to_string(), CHANNEL, "a", None)],
        );
        let mut live = gateway(&rest(
            &(FLOOR + 2).to_string(),
            other,
            "b",
            Some("https://cdn.example/expired"),
        ));
        live["attachments"][0]["url"] = json!("https://cdn.example/expired");
        assert!(intake.handle(Work::Message(live)).is_err());
        assert!(fixture
            .state
            .join(UNSTORED)
            .read_dir()
            .unwrap()
            .next()
            .is_some());
        let mut restarted = Intake::new(
            fixture.discord.clone(),
            Box::new(Refusing {
                inner: fixture.fake.clone(),
                refused: other.to_string(),
            }),
            vec![NonZeroU64::new(CHANNEL).unwrap()],
            false,
            fixture.state.clone(),
            FLOOR,
        );
        let caught_up = Done::Backfilled {
            channels: 1,
            failed: 0,
            more: false,
            unstored: 0,
        };
        assert_eq!(restarted.handle(Work::Backfill).unwrap(), caught_up);
        assert!(fixture
            .state
            .join(UNSTORED)
            .read_dir()
            .unwrap()
            .next()
            .is_none());
        assert!(!fixture
            .fake
            .pages
            .lock()
            .unwrap()
            .iter()
            .any(|(channel, _)| *channel == other.to_string()));
        assert_eq!(restarted.handle(Work::Backfill).unwrap(), caught_up);
    }

    /// A live event intake cannot place, because its channel's floor cannot
    /// be read or kept, is kept by id like a failed write: an edit of an old
    /// message heard while the floor is unreadable, and the first message of
    /// a DM channel whose floor cannot be kept, which still begins it.
    #[test]
    fn a_live_event_whose_floor_cannot_be_had_is_fetched_again_by_its_id() {
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[CHANNEL], true);
        let mut messages: Vec<Value> = (1..=160)
            .map(|n| rest(&(FLOOR + n).to_string(), CHANNEL, &format!("m{n}"), None))
            .collect();
        fixture.serve(CHANNEL, messages.clone());
        intake.handle(Work::Backfill).unwrap();
        let floor_file = fixture.state.join("channels").join(CHANNEL.to_string());
        let good = std::fs::read_to_string(&floor_file).unwrap();
        std::fs::write(&floor_file, "").unwrap();
        let mut edited = rest(&(FLOOR + 101).to_string(), CHANNEL, "m101, edited", None);
        edited["edited_timestamp"] = json!("2026-09-26T09:00:00Z");
        assert!(intake.handle(Work::Update(gateway(&edited))).is_err());
        messages[100] = edited;
        fixture.serve(CHANNEL, messages);

        // The DM state is a file where its directory belongs: no DM floor
        // can be kept.
        let dm = CHANNEL + 2;
        let first = rest(&(FLOOR + 170).to_string(), dm, "hello bot", None);
        fixture.serve(
            dm,
            vec![
                rest(&(FLOOR + 165).to_string(), dm, "an old DM", None),
                first.clone(),
            ],
        );
        let dms = fixture.state.join("dms");
        std::fs::write(&dms, "").unwrap();
        assert!(intake.handle(Work::Message(first)).is_err());
        assert_eq!(
            fixture.state.join(UNSTORED).read_dir().unwrap().count(),
            2,
            "both are kept by id"
        );

        // Still unreadable: nothing is read, and the backfill has not caught up.
        let Done::Backfilled {
            failed, unstored, ..
        } = intake.handle(Work::Backfill).unwrap()
        else {
            panic!("a backfill");
        };
        assert_eq!((failed, unstored), (2, 2));
        assert!(fixture
            .stored(CHANNEL)
            .iter()
            .all(|(content, _, _)| content != "m101, edited"));

        // Repaired: both are fetched by id and stored; the DM channel begins
        // with its first message.
        std::fs::write(&floor_file, good).unwrap();
        std::fs::remove_file(&dms).unwrap();
        assert_eq!(
            intake.handle(Work::Backfill).unwrap(),
            Done::Backfilled {
                channels: 1,
                failed: 0,
                more: false,
                unstored: 0
            }
        );
        assert!(fixture
            .stored(CHANNEL)
            .contains(&("m101, edited".to_owned(), 0, 1)));
        assert_eq!(fixture.stored(dm), [("hello bot".to_owned(), 0, 1)]);
        assert_eq!(
            std::fs::read_to_string(dms.join(dm.to_string())).unwrap(),
            (FLOOR + 169).to_string()
        );
    }

    /// DM channels that cannot be listed are a backfill that failed, not one
    /// that caught up: it is tried again.
    #[test]
    fn dm_channels_that_cannot_be_listed_fail_the_backfill() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let mut intake = fixture.intake(&[], true);
        let dm = CHANNEL + 2;
        let heard = rest(&(FLOOR + 10).to_string(), dm, "hello bot", None);
        fixture.serve(dm, vec![heard.clone()]);
        assert_eq!(intake.handle(Work::Message(heard)).unwrap(), Done::Stored);
        let dms = fixture.state.join("dms");
        std::fs::set_permissions(&dms, std::fs::Permissions::from_mode(0o000)).unwrap();
        let done = intake.handle(Work::Backfill);
        std::fs::set_permissions(&dms, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            done.unwrap(),
            Done::Backfilled {
                channels: 1,
                failed: 1,
                more: false,
                unstored: 0
            }
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
                more: false,
                unstored: 0
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
