//! The say requests `discord live` plays in its voice channel, read from
//! the discord collection.
//!
//! `discord say` publishes a request ([`crate::discord::say_request_fragment`])
//! and the voice connection's speaker looks for new ones whenever the store
//! changes: it samples the store's snapshot every poll, and reads the
//! collection again only when collection records or blobs arrived since the
//! last look, the way `orient wait` notices news. It plays them oldest first
//! and records what became of each as an outcome linking the request
//! (started, then played or failed), so a request is played at most once
//! across restarts: a played or failed one is settled for good.
//!
//! What a restarted speaker does with what it finds unsettled is
//! [`crate::discord::say_turn`]: a request a previous speaker began but never
//! settled (it crashed, or was restarted, between playing it and recording
//! the outcome) may have been heard, and one asked for more than
//! [`crate::discord::SAY_STALE`] before the speaker started belongs to a
//! conversation that has moved on. Both are failed with their reason rather
//! than played. The started outcome is written beside the first synthesis,
//! not before it, so the first sound does not wait for a commit: a crash in
//! the moment before it lands leaves a request with no outcome at all, and a
//! restart within [`crate::discord::SAY_STALE`] plays that one again.
//!
//! Not here yet (a follow-up, not code): Drive speaks by streaming text
//! records (the framed-stream convention) into the program named by its
//! `--voice`. A `discord say-stream` reading that stream and publishing each
//! complete sentence as a say request would let Drive speak in the voice
//! channel without a change to Drive, and pushing what is heard to Drive's
//! organ as a sense would carry the other direction while Drive runs. The
//! request's words are already the Archive text fact Drive records for what
//! she said, so both sides would name the same words.

use super::Discord;
use crate::discord::{say_turn, PendingSay, SayOutcome};
use crate::storage::FacultySnapshot;
use anyhow::Result;
use std::num::NonZeroU64;
use triblespace::core::repo::{StoreChanges, StoreSnapshot};
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

/// The say requests of one voice channel, as one speaker plays them.
/// A clone shares the store and starts from the same last look.
#[derive(Clone)]
pub struct SayQueue {
    discord: Discord,
    channel: String,
    /// When the speaker started: what was begun or asked for before it is
    /// judged by [`say_turn`].
    started: Inline<NsTAIInterval>,
    /// The store as it was at the last look, if there was one.
    seen: Option<FacultySnapshot>,
}

impl SayQueue {
    pub fn new(discord: Discord, channel: NonZeroU64, started: Inline<NsTAIInterval>) -> Self {
        Self {
            discord,
            channel: channel.to_string(),
            started,
            seen: None,
        }
    }

    /// The oldest request to play now, or `None` when nothing arrived since
    /// the last look or nothing is left to play. Requests that must not be
    /// played are failed on the way, with their reason. A failed look is
    /// looked at again on the next call.
    pub fn next(&mut self) -> Result<Option<PendingSay>> {
        let sampled = self.discord.store_snapshot()?;
        if let Some(seen) = &self.seen {
            let changes = sampled.changes_since(seen);
            if !changes.contains(StoreChanges::COLLECTION_RECORDS)
                && !changes.contains(StoreChanges::BLOBS)
            {
                return Ok(None);
            }
        }
        let pending = self.discord.unsettled_speech(&self.channel)?;
        // Taken before the read: whatever arrives during or after it is a
        // change from here.
        self.seen = Some(sampled);
        for say in pending {
            match say_turn(&say, self.started) {
                None => return Ok(Some(say)),
                Some(reason) => {
                    eprintln!(
                        "[discord] say request {:X} is not played: {reason}",
                        say.request
                    );
                    self.record(say.request, SayOutcome::Failed(&reason))?;
                }
            }
        }
        Ok(None)
    }

    /// Record what became of a request.
    pub fn record(&self, request: Id, outcome: SayOutcome<'_>) -> Result<()> {
        self.discord.record_speech(request, outcome).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::{moment, SAY_STALE};
    use crate::voice::options::SpeechOptions;
    use std::path::PathBuf;

    const HERE: &str = "200000000000000201";
    const ELSEWHERE: &str = "200000000000000202";

    struct Scratch {
        _directory: tempfile::TempDir,
        pile: PathBuf,
        key: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            crate::test_support::clear_ambient_environment();
            let directory = tempfile::tempdir().unwrap();
            let pile = directory.path().join("say.pile");
            let key = directory.path().join("say.key");
            std::fs::File::create(&pile).unwrap();
            crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
            Self {
                _directory: directory,
                pile,
                key,
            }
        }

        /// What `discord say` uses: one store per call.
        fn asker(&self) -> Discord {
            Discord::new(self.pile.clone(), Some(self.key.clone()))
        }

        /// What `discord live` uses: one store for the life of the process.
        fn speaker(&self, channel: &str, started: Inline<NsTAIInterval>) -> SayQueue {
            let storage =
                crate::storage::Storage::shared(self.pile.clone(), Some(self.key.clone()));
            SayQueue::new(
                Discord::with_storage(storage),
                channel.parse().unwrap(),
                started,
            )
        }

        fn say(&self, channel: &str, text: &str) -> Id {
            self.asker()
                .request_speech(
                    Some("200000000000000300"),
                    channel,
                    text,
                    &SpeechOptions::default(),
                    Some("ada"),
                )
                .unwrap()
        }
    }

    fn now() -> Inline<NsTAIInterval> {
        moment(crate::clock::now().unwrap())
    }

    fn after(seconds: f64) -> Inline<NsTAIInterval> {
        moment(crate::clock::now().unwrap() + hifitime::Duration::from_seconds(seconds))
    }

    /// Requests are played oldest first, each channel's by its own speaker;
    /// a request recorded as played (its track completed) or failed (it was
    /// aborted) is settled for good, and a restarted speaker replays
    /// neither. The asker's writes reach a speaker holding one store open
    /// as they arrive.
    #[test]
    fn requests_play_in_order_once_and_each_channel_keeps_its_own() {
        let scratch = Scratch::new();
        let started = now();
        let mut here = scratch.speaker(HERE, started);
        assert_eq!(here.next().unwrap(), None);
        let first = scratch.say(HERE, "First.");
        let other = scratch.say(ELSEWHERE, "Somewhere else.");
        let second = scratch.say(HERE, "Second.");

        let next = here.next().unwrap().expect("the first request plays");
        assert_eq!((next.request, next.text.as_str()), (first, "First."));
        here.record(first, SayOutcome::Started).unwrap();
        here.record(first, SayOutcome::Played).unwrap();
        let next = here.next().unwrap().expect("the second request plays");
        assert_eq!((next.request, next.text.as_str()), (second, "Second."));
        here.record(second, SayOutcome::Started).unwrap();
        here.record(
            second,
            SayOutcome::Failed("the track ended Stop after 0.4 s"),
        )
        .unwrap();
        assert_eq!(here.next().unwrap(), None);

        // A restart replays nothing that was settled.
        let mut restarted = scratch.speaker(HERE, now());
        assert_eq!(restarted.next().unwrap(), None);
        assert!(scratch.asker().unsettled_speech(HERE).unwrap().is_empty());

        // The other channel's request was never this speaker's.
        let mut elsewhere = scratch.speaker(ELSEWHERE, started);
        let next = elsewhere.next().unwrap().expect("its own request plays");
        assert_eq!(
            (next.request, next.text.as_str()),
            (other, "Somewhere else.")
        );
    }

    /// A request a speaker began (and lost the voice connection on) is
    /// played again by the same speaker; one a previous speaker began and
    /// never settled, because it crashed between playing it and recording
    /// how it went, may have been heard and is failed instead of replayed.
    #[test]
    fn a_request_begun_by_a_previous_speaker_is_failed_not_replayed() {
        let scratch = Scratch::new();
        let mut speaker = scratch.speaker(HERE, now());
        let request = scratch.say(HERE, "Interrupted.");
        assert_eq!(speaker.next().unwrap().unwrap().request, request);
        speaker.record(request, SayOutcome::Started).unwrap();
        // The same speaker, after a lost connection: played again.
        assert_eq!(speaker.next().unwrap().unwrap().request, request);

        // The process dies here. Its successor starts after the start.
        let mut successor = scratch.speaker(HERE, after(0.001));
        assert_eq!(successor.next().unwrap(), None);
        assert!(scratch.asker().unsettled_speech(HERE).unwrap().is_empty());
        let reasons = failures(&scratch, request);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("previous speaker began"), "{reasons:?}");
    }

    /// A request asked for more than [`SAY_STALE`] before the speaker
    /// started is failed as stale at startup, never played; one asked for
    /// within the bound (while the process was restarting, say) plays.
    #[test]
    fn a_request_stale_at_startup_is_failed_with_its_reason() {
        let scratch = Scratch::new();
        let old = scratch.say(HERE, "Too late.");
        let mut late = scratch.speaker(HERE, after(SAY_STALE.as_secs_f64() + 1.0));
        assert_eq!(late.next().unwrap(), None);
        let reasons = failures(&scratch, old);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].starts_with("stale at startup"), "{reasons:?}");

        let recent = scratch.say(HERE, "Just in time.");
        let mut soon = scratch.speaker(HERE, after(SAY_STALE.as_secs_f64() / 2.0));
        assert_eq!(soon.next().unwrap().unwrap().request, recent);
    }

    /// The reasons the request was failed for.
    fn failures(scratch: &Scratch, request: Id) -> Vec<String> {
        scratch.asker().say_failures(request).unwrap()
    }
}
