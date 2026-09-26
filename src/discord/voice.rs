//! The voice connection of `discord live`: the bot in a voice channel,
//! hanging off the process's one gateway session and speaking with one
//! resident Qwen3-TTS synthesizer from `voice`.
//!
//! The bot asks the gateway session ([`super::gateway`]) to join the voice
//! channel, and the session id, voice token and voice endpoint Discord replies
//! with go to songbird's voice driver, which does the rest (UDP, Opus, and
//! the DAVE end-to-end encryption Discord requires of every voice connection
//! since 2026-03-02). Being in the channel is a goal, not a single request:
//! the join is asked for again after a new session, when the bot is taken out
//! of the channel or the driver loses its connection, and whenever a join is
//! not confirmed by a working voice connection. No serenity: nothing here
//! needs a cache, commands or events beyond those.
//!
//! `discord say` queues a line by writing one file into the state
//! directory's `say/`; the speaker speaks the queue in order and moves each
//! file to `said/` once it has played, or to `failed/` when it cannot be
//! spoken. The queue is files so that any process, including a shell with
//! nothing but the discord binary, can speak, and so that a line survives a
//! restart of the resident process instead of vanishing with it. Speaking
//! runs beside the gateway, never in its way: a voice server update is
//! handled while a line plays. A line during which the driver loses or
//! re-establishes its voice connection, or the bot leaves the channel, is
//! spoken again; a drop the driver has not noticed yet when the line ends is
//! not. A synthesizer that breaks (a panic in synthesis), or lines failing
//! several times in a row, end the process, so that its supervisor starts it
//! again with a fresh model.
//!
//! Built with the `discord-voice` feature.

use super::gateway;
use super::live::{Beside, StateDir};
use crate::voice::synthesis::{ModelSources, Synthesizer};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::input::RawAdapter;
use songbird::tracks::PlayMode;
use songbird::{ConnectionInfo, CoreEvent, Driver};
use std::io::Cursor;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::time::Instant;

/// How often the speaker looks for queued lines.
const QUEUE_POLL: Duration = Duration::from_millis(250);
/// How long a join may go unconfirmed by a voice connection before it is
/// asked for again.
const JOIN_CONFIRM: Duration = Duration::from_secs(20);
/// Lines in a row that could not be spoken after which the process ends, so
/// that it starts again with a fresh model and voice driver.
const FAILED_IN_A_ROW: u32 = 3;

/// The state directory's queue: `say/` holds queued lines, `said/` spoken
/// ones and `failed/` the ones that could not be spoken.
impl StateDir {
    fn queue(&self) -> PathBuf {
        self.root().join("say")
    }

    /// Queue one line: written beside the queue under a dot name, then
    /// renamed in, so the resident process never reads half a line.
    pub fn enqueue(&self, text: &str) -> Result<PathBuf> {
        anyhow::ensure!(!text.trim().is_empty(), "a line to say must not be empty");
        let queue = self.queue();
        std::fs::create_dir_all(&queue)
            .with_context(|| format!("create the voice queue {}", queue.display()))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("read the clock")?
            .as_nanos();
        let name = format!("{stamp:024}-{}.txt", std::process::id());
        let staging = queue.join(format!(".{name}"));
        std::fs::write(&staging, text)
            .with_context(|| format!("write the queued line {}", staging.display()))?;
        let queued = queue.join(name);
        std::fs::rename(&staging, &queued)
            .with_context(|| format!("queue the line as {}", queued.display()))?;
        Ok(queued)
    }

    /// Queued lines in the order they were queued.
    fn pending(&self) -> Result<Vec<PathBuf>> {
        let queue = self.queue();
        let mut lines = Vec::new();
        let entries = match std::fs::read_dir(&queue) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(lines),
            Err(error) => {
                return Err(error).with_context(|| format!("list {}", queue.display()));
            }
        };
        for entry in entries {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !name.starts_with('.') && name.ends_with(".txt") {
                lines.push(path);
            }
        }
        lines.sort();
        Ok(lines)
    }

    /// Move a line out of the queue into `into` (`said` or `failed`).
    fn retire(&self, line: &Path, into: &str) -> Result<()> {
        let directory = self.root().join(into);
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("create {}", directory.display()))?;
        let name = line.file_name().context("a queued line has a file name")?;
        std::fs::rename(line, directory.join(name))
            .with_context(|| format!("move {} to {into}/", line.display()))
    }
}

/// Which voice channel to join, where the queue is, and who is told once the
/// voice connection is up.
pub struct Config {
    pub guild: NonZeroU64,
    pub channel: NonZeroU64,
    /// A text channel the greeting is posted in once the voice connection is
    /// up, so that people know to join.
    pub announce: Option<NonZeroU64>,
    pub greeting: String,
    /// The state directory whose queue is spoken.
    pub state: StateDir,
}

/// Start the voice connection's task beside the gateway session, which
/// `commands` reaches and whose every dispatch the task is handed. It runs
/// until the speech model cannot load or breaks, or until the dispatches stop
/// coming, and leaves the channel either way. Nothing else ends it: gateway
/// reconnects, lost voice connections and a line that could not be spoken
/// are all recovered from while the speech model stays loaded.
pub fn start(config: Config, token: String, commands: mpsc::UnboundedSender<Value>) -> Beside {
    let (dispatches, inbox) = mpsc::unbounded_channel();
    Beside {
        dispatches,
        task: tokio::spawn(run(config, token, inbox, commands)),
    }
}

async fn run(
    config: Config,
    token: String,
    mut dispatches: mpsc::UnboundedReceiver<Value>,
    commands: mpsc::UnboundedSender<Value>,
) -> Result<()> {
    let Config {
        guild,
        channel,
        announce,
        greeting,
        state,
    } = config;
    let driver = Arc::new(Mutex::new(Driver::new(songbird::Config::default())));
    let (lost, mut losses) = mpsc::unbounded_channel();
    let events = VoiceEvents {
        lost,
        interruptions: Arc::new(AtomicU64::new(0)),
    };
    for event in [CoreEvent::DriverDisconnect, CoreEvent::DriverReconnect] {
        driver
            .lock()
            .await
            .add_global_event(songbird::Event::Core(event), events.clone());
    }
    let (connected, is_connected) = watch::channel(false);
    let greet = announce.map(|channel| (token, channel, greeting));
    let mut speaker = tokio::spawn(speak_queue(
        SpeechWorker::start(ModelSources::from_environment()),
        state,
        Voice {
            driver: driver.clone(),
            connected: is_connected,
            interruptions: events.interruptions.clone(),
        },
        greet,
    ));

    let mut voice = VoiceJoin::new(guild, channel);
    let mut join = Rejoin::default();
    let (connects, mut connected_results) = mpsc::unbounded_channel();
    let mut attempt = 0u64;
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let outcome = loop {
        tokio::select! {
            spoke = &mut speaker => {
                break match spoke {
                    Ok(Ok(())) => Err(anyhow!("the speaker stopped")),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(anyhow!("the speaker task stopped: {error}")),
                };
            }
            dispatch = dispatches.recv() => {
                // The process is stopping.
                let Some(dispatch) = dispatch else {
                    break Ok(());
                };
                match dispatch["t"].as_str() {
                    // A new session: join at once. Its voice state is new too.
                    Some("READY") => join.now(Instant::now()),
                    // A resumed one keeps its voice state; join only if the
                    // bot is out of the channel.
                    Some("RESUMED") if !*connected.borrow() => join.soon(Instant::now()),
                    _ => {}
                }
                match voice.observe(&dispatch) {
                    Some(VoiceChange::Connect(info)) => {
                        eprintln!(
                            "[discord] joining voice channel {channel} in guild {guild} as user {:?}",
                            info.user_id
                        );
                        attempt += 1;
                        let pending = driver.lock().await.connect(info);
                        let connects = connects.clone();
                        let this = attempt;
                        tokio::spawn(async move {
                            let _ = connects.send((this, pending.await.map_err(|error| error.to_string())));
                        });
                    }
                    Some(VoiceChange::Left) => {
                        eprintln!("[discord] the bot is out of voice channel {channel}; joining again");
                        connected.send_replace(false);
                        join.soon(Instant::now());
                    }
                    None => {}
                }
            }
            Some((this, result)) = connected_results.recv() => {
                // Only the newest connection attempt counts.
                if this != attempt {
                    continue;
                }
                match result {
                    Ok(()) => {
                        eprintln!("[discord] voice connected");
                        connected.send_replace(true);
                        join.confirmed(Instant::now());
                    }
                    Err(error) => {
                        eprintln!("[discord] connecting the voice driver failed: {error}");
                        connected.send_replace(false);
                        join.soon(Instant::now());
                    }
                }
            }
            Some(reason) = losses.recv() => {
                eprintln!("[discord] the voice connection dropped ({reason}); joining again");
                connected.send_replace(false);
                join.soon(Instant::now());
            }
            _ = ticker.tick() => {
                if join.take(Instant::now(), *connected.borrow()) {
                    let _ = commands.send(gateway::voice_state(guild, Some(channel)));
                }
            }
        }
    };
    speaker.abort();
    driver.lock().await.leave();
    // Leave the channel on the gateway too, so the bot does not linger in it.
    let _ = commands.send(gateway::voice_state(guild, None));
    outcome
}

/// When to ask the gateway to join the voice channel: at once for a new
/// session, soon after the bot lost the channel, and again while a join is
/// not confirmed by a working voice connection, backing off from five
/// seconds to a minute. Only a connection that lasted [`JOIN_CONFIRM`] resets
/// the backoff: one lost soon after it connected (another process on the
/// same bot token taking the channel, a voice server refusing the connection
/// after its handshake) counts as one more attempt that did not work.
#[derive(Debug, Default)]
struct Rejoin {
    due: Option<Instant>,
    attempts: u32,
    /// Since when the current voice connection works.
    up_since: Option<Instant>,
}

impl Rejoin {
    /// A new session: join now, as a first attempt.
    fn now(&mut self, now: Instant) {
        self.due = Some(now);
        self.attempts = 0;
    }

    /// The bot is out of the channel: join after the current backoff, or
    /// sooner if a join was already due.
    fn soon(&mut self, now: Instant) {
        let lasted = self
            .up_since
            .take()
            .is_some_and(|since| now.saturating_duration_since(since) >= JOIN_CONFIRM);
        if lasted {
            self.attempts = 0;
        }
        let at = now + Self::wait(self.attempts);
        self.due = Some(self.due.map_or(at, |due| due.min(at)));
    }

    /// A voice connection works, as of `now`: nothing is due.
    fn confirmed(&mut self, now: Instant) {
        self.due = None;
        self.up_since.get_or_insert(now);
    }

    /// Whether a join is due now; if so, the next one is scheduled in case
    /// this one goes unconfirmed. A join that went out while the voice
    /// connection works (`connected`) is confirmed by that connection:
    /// Discord need not answer a join to the channel the bot is already in.
    fn take(&mut self, now: Instant, connected: bool) -> bool {
        if self.due.is_none_or(|due| now < due) {
            return false;
        }
        if connected && self.attempts > 0 {
            self.confirmed(now);
            return false;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.due = Some(now + JOIN_CONFIRM.max(Self::wait(self.attempts)));
        true
    }

    fn wait(attempts: u32) -> Duration {
        match attempts {
            0 => Duration::ZERO,
            n => Duration::from_secs(5 << (n - 1).min(4)).min(Duration::from_secs(60)),
        }
    }
}

/// What a dispatch means for the voice connection.
#[derive(Debug)]
enum VoiceChange {
    /// Everything the driver needs to connect.
    Connect(ConnectionInfo),
    /// The bot's own voice state says it is not in the channel.
    Left,
}

/// What songbird's driver needs to connect, gathered from the gateway's
/// dispatches: the bot's user (READY), its voice session (its own
/// VOICE_STATE_UPDATE) and a voice server (VOICE_SERVER_UPDATE), which may
/// arrive in either order. Every new voice server, from the first join, a
/// re-join, or Discord moving the voice server, yields one connection once
/// the session it belongs to is known.
struct VoiceJoin {
    guild: NonZeroU64,
    channel: NonZeroU64,
    user: Option<u64>,
    session_id: Option<String>,
    /// A voice server, as (token, endpoint), not yet handed to the driver.
    server: Option<(String, String)>,
}

impl VoiceJoin {
    fn new(guild: NonZeroU64, channel: NonZeroU64) -> Self {
        Self {
            guild,
            channel,
            user: None,
            session_id: None,
            server: None,
        }
    }

    fn observe(&mut self, dispatch: &Value) -> Option<VoiceChange> {
        let d = &dispatch["d"];
        match dispatch["t"].as_str() {
            Some("READY") => {
                // A new gateway session: the last one's voice state is gone.
                self.user = snowflake(&d["user"]["id"]);
                self.session_id = None;
                self.server = None;
            }
            Some("VOICE_STATE_UPDATE")
                if self.user.is_some()
                    && snowflake(&d["user_id"]) == self.user
                    && snowflake(&d["guild_id"]) == Some(self.guild.get()) =>
            {
                if snowflake(&d["channel_id"]) != Some(self.channel.get()) {
                    // The end of an older voice session, such as the last
                    // gateway session's, is not this one's.
                    let current = self.session_id.as_deref();
                    if current.is_some() && d["session_id"].as_str() != current {
                        return None;
                    }
                    // Disconnected, or moved elsewhere: this voice session
                    // is over, and so is any server it was given.
                    self.session_id = None;
                    self.server = None;
                    return Some(VoiceChange::Left);
                }
                self.session_id = d["session_id"].as_str().map(str::to_owned);
            }
            Some("VOICE_SERVER_UPDATE") if snowflake(&d["guild_id"]) == Some(self.guild.get()) => {
                // A null endpoint means the server is being allocated; a
                // second update follows with one.
                if let (Some(token), Some(endpoint)) = (d["token"].as_str(), d["endpoint"].as_str())
                {
                    self.server = Some((token.to_owned(), endpoint.to_owned()));
                }
            }
            _ => return None,
        }
        let user = NonZeroU64::new(self.user?)?;
        let session_id = self.session_id.clone()?;
        let (token, endpoint) = self.server.take()?;
        Some(VoiceChange::Connect(ConnectionInfo {
            channel_id: ChannelId(self.channel),
            endpoint,
            guild_id: GuildId(self.guild),
            session_id,
            token,
            user_id: UserId(user),
        }))
    }
}

/// What the driver does to the voice connection on its own: each reconnect
/// or loss is counted, since a line playing across one may not have been
/// heard, and a connection it gave up on is reported to the voice task.
#[derive(Clone)]
struct VoiceEvents {
    lost: mpsc::UnboundedSender<String>,
    interruptions: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl songbird::EventHandler for VoiceEvents {
    async fn act(&self, context: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        match context {
            songbird::EventContext::DriverReconnect(_) => {
                eprintln!("[discord] the voice driver reconnected on its own");
                self.interruptions.fetch_add(1, Ordering::SeqCst);
            }
            songbird::EventContext::DriverDisconnect(data) => {
                let requested = matches!(
                    data.reason,
                    Some(songbird::events::context_data::DisconnectReason::Requested)
                );
                if !requested {
                    self.interruptions.fetch_add(1, Ordering::SeqCst);
                    let _ = self
                        .lost
                        .send(format!("{:?}: {:?}", data.kind, data.reason));
                }
            }
            _ => {}
        }
        None
    }
}

/// What the speaker needs of the voice connection: the driver, whether it
/// works, and how often the driver has lost or re-established it.
struct Voice {
    driver: Arc<Mutex<Driver>>,
    connected: watch::Receiver<bool>,
    interruptions: Arc<AtomicU64>,
}

/// Speak the queue, one line at a time, whenever the voice connection works:
/// the greeting first, once. Fails when the speech model cannot load or
/// breaks, or when [`FAILED_IN_A_ROW`] lines in a row could not be spoken.
async fn speak_queue(
    speech: SpeechWorker,
    state: StateDir,
    mut voice: Voice,
    greet: Option<(String, NonZeroU64, String)>,
) -> Result<()> {
    speech.ready().await?;
    eprintln!("[discord] speech ready");
    let mut greet = greet;
    let mut failed_in_a_row = 0;
    loop {
        if voice.connected.wait_for(|up| *up).await.is_err() {
            return Ok(());
        }
        if let Some((token, channel, greeting)) = greet.take() {
            if let Err(error) = announce(token, channel, greeting).await {
                eprintln!("[discord] the greeting was not posted: {error:#}");
            }
        }
        let queued = match state.pending() {
            Ok(queued) => queued.into_iter().next(),
            Err(error) => {
                eprintln!("[discord] reading the queue failed: {error:#}");
                None
            }
        };
        let Some(queued) = queued else {
            tokio::time::sleep(QUEUE_POLL).await;
            continue;
        };
        let into = match speak_line(&mut voice, &speech, &queued).await {
            Spoken::Heard => {
                failed_in_a_row = 0;
                "said"
            }
            Spoken::Failed(error) => {
                eprintln!("[discord] line {} failed: {error:#}", queued.display());
                failed_in_a_row += 1;
                "failed"
            }
            Spoken::Broken(error) => {
                // Out of the queue first: if this line is what broke the
                // synthesizer, the next process must not meet it again.
                if let Err(retire) = state.retire(&queued, "failed") {
                    eprintln!("[discord] retiring {} failed: {retire:#}", queued.display());
                }
                return Err(error.context("the synthesizer broke"));
            }
            Spoken::Interrupted => {
                eprintln!(
                    "[discord] the voice connection dropped or was re-established mid-line; \
                     it is spoken again once it works"
                );
                continue;
            }
        };
        if let Err(error) = state.retire(&queued, into) {
            // A line that cannot leave the queue would be spoken forever.
            return Err(error.context("retire a spoken line"));
        }
        if failed_in_a_row >= FAILED_IN_A_ROW {
            return Err(anyhow!(
                "{failed_in_a_row} lines in a row could not be spoken"
            ));
        }
    }
}

/// How one line went.
enum Spoken {
    Heard,
    /// It cannot be spoken: unreadable, unsynthesizable, or the track failed.
    Failed(anyhow::Error),
    /// The synthesizer is gone; nothing more can be spoken by this process.
    Broken(anyhow::Error),
    /// The voice connection dropped, or was re-established, before or while
    /// it played; it stays queued.
    Interrupted,
}

async fn speak_line(voice: &mut Voice, speech: &SpeechWorker, queued: &Path) -> Spoken {
    let text = match std::fs::read_to_string(queued) {
        Ok(text) => text,
        Err(error) => return Spoken::Failed(anyhow!(error).context("read the queued line")),
    };
    let started = std::time::Instant::now();
    let (pcm, sample_rate) = match speech.speak(text.clone()).await {
        Ok(audio) => audio,
        Err(Unspoken::Line(error)) => return Spoken::Failed(error),
        Err(Unspoken::Synthesizer(error)) => return Spoken::Broken(error),
    };
    let seconds = pcm.len() as f64 / 4.0 / f64::from(sample_rate);
    eprintln!(
        "[discord] synthesized {seconds:.1} s of speech in {:.1} s: {}",
        started.elapsed().as_secs_f64(),
        text.trim()
    );
    if !*voice.connected.borrow_and_update() {
        return Spoken::Interrupted;
    }
    let interruptions = voice.interruptions.load(Ordering::SeqCst);
    let track = voice
        .driver
        .lock()
        .await
        .play_input(RawAdapter::new(Cursor::new(pcm), sample_rate, 1).into());
    let started = std::time::Instant::now();
    // How the line ended is part of what happened: a track that errored or
    // was stopped is not a line that was heard. songbird drops a track the
    // moment it ends, so the outcome comes from its own End and Error events,
    // never from polling a handle that is already gone.
    let (reported, outcome) = oneshot::channel();
    let reported = Arc::new(std::sync::Mutex::new(Some(reported)));
    for event in [songbird::TrackEvent::End, songbird::TrackEvent::Error] {
        if let Err(error) = track.add_event(
            songbird::Event::Track(event),
            TrackOutcome(reported.clone()),
        ) {
            let _ = track.stop();
            return Spoken::Failed(anyhow!("watch the spoken line: {error}"));
        }
    }
    let limit = Duration::from_secs_f64(seconds) + Duration::from_secs(30);
    let ended = tokio::select! {
        ended = tokio::time::timeout(limit, outcome) => ended,
        _ = voice.connected.wait_for(|up| !*up) => {
            // songbird pauses a track whose connection is gone rather than
            // ending it; stop it, and speak the line again once back.
            let _ = track.stop();
            return Spoken::Interrupted;
        }
    };
    let played = started.elapsed().as_secs_f64();
    if voice.interruptions.load(Ordering::SeqCst) != interruptions {
        // The driver lost or re-established the connection while the line
        // played, so part of it may have gone nowhere.
        return Spoken::Interrupted;
    }
    match ended {
        Ok(Ok(PlayMode::End)) => {
            eprintln!("[discord] played for {played:.1} s");
            Spoken::Heard
        }
        Ok(Ok(state)) => Spoken::Failed(anyhow!("the track ended {state:?} after {played:.1} s")),
        Ok(Err(_)) => Spoken::Failed(anyhow!("the track ended without reporting how")),
        Err(_) => {
            let _ = track.stop();
            Spoken::Failed(anyhow!("still playing after {played:.0} s"))
        }
    }
}

/// Reports a track's final play state once, from whichever of its End or
/// Error events fires.
struct TrackOutcome(Arc<std::sync::Mutex<Option<oneshot::Sender<PlayMode>>>>);

#[async_trait::async_trait]
impl songbird::EventHandler for TrackOutcome {
    async fn act(&self, context: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        if let songbird::EventContext::Track([(state, _), ..]) = context {
            let sender = self.0.lock().ok().and_then(|mut slot| slot.take());
            if let Some(sender) = sender {
                let _ = sender.send(state.playing.clone());
            }
        }
        None
    }
}

/// Post the greeting in a text channel once the voice connection is up, with
/// the same REST call `discord send` makes. It is not stored in the
/// collection (the voice connection runs without a pile) unless the channel
/// is also an intake channel, where the gateway delivers it like any message.
async fn announce(token: String, channel: NonZeroU64, text: String) -> Result<()> {
    let channel = channel.to_string();
    tokio::task::spawn_blocking(move || super::operations::post_message(&token, &channel, &text))
        .await
        .context("the greeting's task stopped")?
        .context("post the greeting")?;
    Ok(())
}

/// The resident Qwen3-TTS synthesizer, on a thread of its own: the model is
/// loaded once, before the first line, and never crosses a thread.
struct SpeechWorker {
    requests: std::sync::mpsc::Sender<Request>,
}

enum Request {
    Ready(oneshot::Sender<Result<()>>),
    Speak(String, oneshot::Sender<Result<(Vec<u8>, u32), Unspoken>>),
}

/// Why a line was not synthesized.
#[derive(Debug)]
enum Unspoken {
    /// This line cannot be spoken; the next one may.
    Line(anyhow::Error),
    /// The synthesizer is gone: a panic ended it (and may have left the
    /// model's lock poisoned), or its thread stopped.
    Synthesizer(anyhow::Error),
}

impl SpeechWorker {
    fn start(sources: ModelSources) -> Self {
        let (requests, inbox) = std::sync::mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name("discord-speech".to_owned())
            .spawn(move || {
                let synthesizer = Synthesizer::new(sources);
                let primed = synthesizer.prime();
                let primed_error = primed.as_ref().err().map(|error| format!("{error:#}"));
                for request in inbox {
                    match request {
                        Request::Ready(reply) => {
                            let _ = reply.send(match &primed_error {
                                None => Ok(()),
                                Some(error) => Err(anyhow!("load the voice model: {error}")),
                            });
                        }
                        Request::Speak(text, reply) => {
                            let spoken =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    synthesizer.synthesize(&text)
                                }));
                            let Ok(spoken) = spoken else {
                                // A panic can leave the resident synthesizer
                                // half updated and its lock poisoned: it
                                // speaks no more.
                                let _ = reply.send(Err(Unspoken::Synthesizer(anyhow!(
                                    "synthesis panicked"
                                ))));
                                return;
                            };
                            let _ = reply.send(
                                spoken
                                    .and_then(|clip| {
                                        wav_to_f32le(clip.wav.as_ref(), clip.sample_rate)
                                    })
                                    .map_err(Unspoken::Line),
                            );
                        }
                    }
                }
            })
            .expect("spawn the speech thread");
        Self { requests }
    }

    async fn ready(&self) -> Result<()> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Ready(reply))
            .map_err(|_| anyhow!("the speech thread stopped"))?;
        answer
            .await
            .map_err(|_| anyhow!("the speech thread stopped"))?
    }

    async fn speak(&self, text: String) -> Result<(Vec<u8>, u32), Unspoken> {
        let stopped = || Unspoken::Synthesizer(anyhow!("the speech thread stopped"));
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Speak(text, reply))
            .map_err(|_| stopped())?;
        answer.await.map_err(|_| stopped())?
    }
}

/// The synthesizer's 16-bit mono WAV as the interleaved f32 bytes songbird's
/// raw adapter reads; songbird resamples to Discord's 48 kHz itself.
fn wav_to_f32le(wav: &[u8], sample_rate: u32) -> Result<(Vec<u8>, u32)> {
    let samples = wav
        .get(44..)
        .filter(|_| wav.get(..4) == Some(b"RIFF"))
        .context("synthesis returned no 16-bit WAV body")?;
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for sample in samples.chunks_exact(2) {
        let value = f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.0;
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    Ok((pcm, sample_rate))
}

fn snowflake(value: &Value) -> Option<u64> {
    value.as_str().and_then(|id| id.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn join() -> VoiceJoin {
        VoiceJoin::new(NonZeroU64::new(7).unwrap(), NonZeroU64::new(8).unwrap())
    }

    fn ready(user: &str) -> Value {
        json!({"t": "READY", "d": {"user": {"id": user}}})
    }

    fn state(user: &str, session: &str) -> Value {
        json!({"t": "VOICE_STATE_UPDATE", "d": {
            "guild_id": "7", "channel_id": "8", "user_id": user, "session_id": session
        }})
    }

    fn server(token: &str, endpoint: Value) -> Value {
        json!({"t": "VOICE_SERVER_UPDATE", "d": {"guild_id": "7", "token": token, "endpoint": endpoint}})
    }

    fn halves(voice: Option<VoiceChange>) -> (String, String) {
        let Some(VoiceChange::Connect(info)) = voice else {
            panic!("expected a connection, got {voice:?}");
        };
        (info.session_id, info.token)
    }

    #[test]
    fn a_voice_connection_needs_its_session_and_a_server_in_either_order() {
        let mut voice = join();
        assert!(voice.observe(&ready("42")).is_none());
        assert!(voice.observe(&state("99", "someone else")).is_none());
        assert!(voice.observe(&state("42", "s1")).is_none());
        let Some(VoiceChange::Connect(info)) =
            voice.observe(&server("t1", json!("voice.example:443")))
        else {
            panic!("both halves are known");
        };
        assert_eq!(info.user_id, UserId(NonZeroU64::new(42).unwrap()));
        assert_eq!(info.channel_id, ChannelId(NonZeroU64::new(8).unwrap()));
        assert_eq!((info.session_id, info.token), ("s1".into(), "t1".into()));
        // A server is handed to the driver once.
        assert!(voice.observe(&state("42", "s1")).is_none());

        // A new gateway session reuses nothing of the old voice session, and
        // its server may come first.
        assert!(voice.observe(&ready("42")).is_none());
        assert!(voice
            .observe(&server("t2", json!("voice.example:443")))
            .is_none());
        assert_eq!(
            halves(voice.observe(&state("42", "s2"))),
            ("s2".into(), "t2".into())
        );

        // Discord moving the voice server: a new server for the same session.
        // A server still being allocated has no endpoint yet.
        assert!(voice.observe(&server("t3", Value::Null)).is_none());
        assert_eq!(
            halves(voice.observe(&server("t3", json!("elsewhere.example:443")))),
            ("s2".into(), "t3".into())
        );
    }

    #[test]
    fn the_bot_taken_out_of_the_channel_has_left_it() {
        let mut voice = join();
        voice.observe(&ready("42"));
        voice.observe(&state("42", "s1"));
        let out = json!({"t": "VOICE_STATE_UPDATE", "d": {
            "guild_id": "7", "channel_id": null, "user_id": "42", "session_id": "s1"
        }});
        assert!(matches!(voice.observe(&out), Some(VoiceChange::Left)));
        let moved = json!({"t": "VOICE_STATE_UPDATE", "d": {
            "guild_id": "7", "channel_id": "9", "user_id": "42", "session_id": "s1"
        }});
        assert!(matches!(voice.observe(&moved), Some(VoiceChange::Left)));
        // Someone else leaving is nothing to the bot's connection.
        let other = json!({"t": "VOICE_STATE_UPDATE", "d": {
            "guild_id": "7", "channel_id": null, "user_id": "99", "session_id": "x"
        }});
        assert!(voice.observe(&other).is_none());
        // Nor is an older voice session of the bot's ending after a new one
        // is in the channel.
        voice.observe(&state("42", "s2"));
        let stale = json!({"t": "VOICE_STATE_UPDATE", "d": {
            "guild_id": "7", "channel_id": null, "user_id": "42", "session_id": "s1"
        }});
        assert!(voice.observe(&stale).is_none());
        // The re-join's state and server connect again.
        assert!(voice.observe(&state("42", "s3")).is_none());
        assert_eq!(
            halves(voice.observe(&server("t4", json!("voice.example:443")))),
            ("s3".into(), "t4".into())
        );
    }

    #[test]
    fn a_join_is_asked_for_again_until_a_connection_confirms_it() {
        let start = Instant::now();
        let mut join = Rejoin::default();
        assert!(!join.take(start, false), "nothing is due before a session");
        join.now(start);
        assert!(join.take(start, false));
        assert!(!join.take(start + Duration::from_secs(19), false));
        // Unconfirmed: asked again, each wait longer, up to a minute.
        assert!(join.take(start + JOIN_CONFIRM, false));
        let mut at = start + JOIN_CONFIRM;
        for _ in 0..6 {
            at += Duration::from_secs(60);
            assert!(join.take(at, false));
        }
        join.confirmed(at);
        assert!(!join.take(at + Duration::from_secs(600), false));
        // A connection that lasted resets the backoff: losing the channel
        // joins again at once, and a failed join after the usual wait.
        let lost = at + JOIN_CONFIRM;
        join.soon(lost);
        assert!(join.take(lost, false));
        join.soon(lost);
        assert!(!join.take(lost + Duration::from_secs(4), false));
        assert!(join.take(lost + Duration::from_secs(5), false));

        // A new session asks once even while the old voice connection works;
        // that connection then confirms it instead of a repeat.
        let mut join = Rejoin::default();
        join.now(start);
        assert!(join.take(start, true));
        assert!(!join.take(start + JOIN_CONFIRM, true));
        assert!(!join.take(start + JOIN_CONFIRM * 10, false));
        assert_eq!(Rejoin::wait(1), Duration::from_secs(5));
        assert_eq!(Rejoin::wait(4), Duration::from_secs(40));
        assert_eq!(Rejoin::wait(9), Duration::from_secs(60));
    }

    /// Connections that work and are lost a second later (another process on
    /// the same token taking the channel back) are joins that did not work:
    /// the bot backs off instead of taking the channel back every second.
    #[test]
    fn a_connection_lost_right_after_it_connected_backs_off() {
        let mut at = Instant::now();
        let mut join = Rejoin::default();
        join.now(at);
        assert!(join.take(at, false));
        let mut waits = Vec::new();
        for _ in 0..6 {
            join.confirmed(at + Duration::from_secs(1));
            let lost = at + Duration::from_secs(2);
            join.soon(lost);
            let due = join.due.expect("a join is due after a loss");
            waits.push((due - lost).as_secs());
            assert!(!join.take(due - Duration::from_millis(1), false));
            at = due;
            assert!(join.take(at, false));
        }
        assert_eq!(waits, [5, 10, 20, 40, 60, 60]);
    }
}
