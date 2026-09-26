//! A resident voice line in a Discord voice channel.
//!
//! One process holds three things: a minimal session on Discord's gateway,
//! songbird's voice driver, and one resident Qwen3-TTS session from `voice`.
//! The gateway session exists only to join a voice channel and keep that
//! membership alive: it identifies, heartbeats, asks to join, and hands the
//! session id, voice token and voice endpoint Discord replies with to
//! songbird's driver, which does the rest (UDP, Opus, and the DAVE end-to-end
//! encryption Discord requires of every voice connection since 2026-03-02).
//! No serenity: nothing here needs a cache, commands or events beyond those.
//!
//! `call join` runs the line. `call say` queues a line for it by writing one
//! file into the session directory's `say/`; the resident process speaks the
//! queue in order and moves each file to `said/` once it has played. The
//! queue is files so that any process, including a shell with nothing but
//! this binary, can speak, and so that a line survives a restart of the
//! resident process instead of vanishing with it.

use crate::voice::synthesis::{ModelSources, Synthesizer};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::input::RawAdapter;
use songbird::{Config, ConnectionInfo, Driver};
use std::io::Cursor;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

pub mod cli;

const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const API: &str = "https://discord.com/api/v10";
/// GUILDS | GUILD_VOICE_STATES: the two intents a voice join needs.
const INTENTS: u64 = 1 | (1 << 7);
/// How often the resident process looks for queued lines.
const QUEUE_POLL: Duration = Duration::from_millis(250);

/// The session directory: `say/` holds queued lines, `said/` spoken ones.
#[derive(Clone, Debug)]
pub struct Session {
    root: PathBuf,
}

impl Session {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The default session: one per user, beside the bot token.
    pub fn default_root() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/share/faculties/call")
    }

    fn queue(&self) -> PathBuf {
        self.root.join("say")
    }

    fn spoken(&self) -> PathBuf {
        self.root.join("said")
    }

    /// Queue one line: written beside the queue under a dot name, then
    /// renamed in, so the resident process never reads half a line.
    pub fn enqueue(&self, text: &str) -> Result<PathBuf> {
        anyhow::ensure!(!text.trim().is_empty(), "a line to say must not be empty");
        let queue = self.queue();
        std::fs::create_dir_all(&queue)
            .with_context(|| format!("create the call queue {}", queue.display()))?;
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

    fn retire(&self, line: &Path) -> Result<()> {
        let spoken = self.spoken();
        std::fs::create_dir_all(&spoken).with_context(|| format!("create {}", spoken.display()))?;
        let name = line.file_name().context("a queued line has a file name")?;
        std::fs::rename(line, spoken.join(name))
            .with_context(|| format!("move {} to said/", line.display()))
    }
}

/// Where the line runs, and who hears that it started.
pub struct Line {
    pub token: String,
    pub guild: NonZeroU64,
    pub channel: NonZeroU64,
    /// A text channel told when the line is open, since a bot cannot ring.
    pub announce: Option<NonZeroU64>,
    pub greeting: String,
    pub session: Session,
}

/// Run the line until the gateway ends it or the process is asked to stop.
pub async fn run(line: Line) -> Result<()> {
    let speech = SpeechWorker::start(ModelSources::from_environment());
    let mut gateway = Gateway::open(&line.token).await?;
    let info = gateway.join_voice(line.guild, line.channel).await?;
    eprintln!(
        "[call] joining voice channel {} in guild {} as user {:?}",
        line.channel, line.guild, info.user_id
    );
    let mut driver = Driver::new(Config::default());
    driver
        .connect(info)
        .await
        .map_err(|error| anyhow!("connect the voice driver: {error}"))?;
    eprintln!("[call] voice connected");
    speech.ready().await?;
    eprintln!("[call] speech ready");
    if let Some(channel) = line.announce {
        announce(&line.token, channel, &line.greeting).await?;
    }

    let mut ticker = tokio::time::interval(QUEUE_POLL);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("listen for SIGTERM")?;
    let mut watch = gateway.watch;
    let outcome = loop {
        tokio::select! {
            ended = &mut watch => {
                break match ended {
                    Ok(Ok(())) => Err(anyhow!("the gateway session ended")),
                    Ok(Err(error)) => Err(error.context("the gateway session failed")),
                    Err(error) => Err(anyhow!("the gateway task stopped: {error}")),
                };
            }
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
            _ = ticker.tick() => {
                for queued in line.session.pending()? {
                    speak_line(&mut driver, &speech, &queued).await?;
                    line.session.retire(&queued)?;
                }
            }
        }
    };
    driver.leave();
    // Leave the channel on the gateway too, so the bot does not linger in it.
    gateway
        .outgoing
        .send(json!({
            "op": 4,
            "d": {"guild_id": line.guild.to_string(), "channel_id": null,
                  "self_mute": false, "self_deaf": false}
        }))
        .ok();
    tokio::time::sleep(Duration::from_millis(500)).await;
    outcome
}

async fn speak_line(driver: &mut Driver, speech: &SpeechWorker, queued: &Path) -> Result<()> {
    let text = std::fs::read_to_string(queued)
        .with_context(|| format!("read the queued line {}", queued.display()))?;
    let started = std::time::Instant::now();
    let (pcm, sample_rate) = speech.speak(text.clone()).await?;
    let seconds = pcm.len() as f64 / 4.0 / f64::from(sample_rate);
    eprintln!(
        "[call] synthesized {seconds:.1} s of speech in {:.1} s: {}",
        started.elapsed().as_secs_f64(),
        text.trim()
    );
    let track = driver.play_input(RawAdapter::new(Cursor::new(pcm), sample_rate, 1).into());
    let started = std::time::Instant::now();
    // How the line ended is part of what happened: a track that errored or
    // was stopped is not a line that was heard.
    let ended = loop {
        match track.get_info().await {
            Ok(state) if !state.playing.is_done() => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(state) => break format!("{:?}", state.playing),
            Err(error) => break format!("track control ended: {error}"),
        }
    };
    eprintln!(
        "[call] played for {:.1} s, ended {ended}",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Tell a text channel the line is open.
async fn announce(token: &str, channel: NonZeroU64, text: &str) -> Result<()> {
    reqwest::Client::new()
        .post(format!("{API}/channels/{channel}/messages"))
        .header("Authorization", format!("Bot {token}"))
        .json(&json!({ "content": text }))
        .send()
        .await
        .context("post the greeting")?
        .error_for_status()
        .context("Discord refused the greeting")?;
    Ok(())
}

/// The resident Qwen3-TTS session, on a thread of its own: the model is
/// loaded once, before the first line, and never crosses a thread.
struct SpeechWorker {
    requests: std::sync::mpsc::Sender<Request>,
}

enum Request {
    Ready(oneshot::Sender<Result<()>>),
    Speak(String, oneshot::Sender<Result<(Vec<u8>, u32)>>),
}

impl SpeechWorker {
    fn start(sources: ModelSources) -> Self {
        let (requests, inbox) = std::sync::mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name("call-speech".to_owned())
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
                            let _ = reply.send(synthesizer.synthesize(&text).and_then(|clip| {
                                wav_to_f32le(clip.wav.as_ref(), clip.sample_rate)
                            }));
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

    async fn speak(&self, text: String) -> Result<(Vec<u8>, u32)> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Speak(text, reply))
            .map_err(|_| anyhow!("the speech thread stopped"))?;
        answer
            .await
            .map_err(|_| anyhow!("the speech thread stopped"))?
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

/// The one gateway session: a writer task, a heartbeat task, and a reader
/// that watches for the end of the session after the voice join is done.
struct Gateway {
    outgoing: mpsc::UnboundedSender<Value>,
    incoming: mpsc::UnboundedReceiver<Value>,
    user: Option<u64>,
    watch: tokio::task::JoinHandle<Result<()>>,
}

impl Gateway {
    async fn open(token: &str) -> Result<Self> {
        let (socket, _) = tokio_tungstenite::connect_async(GATEWAY)
            .await
            .context("connect to the Discord gateway")?;
        let (mut sink, mut stream) = socket.split();
        let hello = next_payload(&mut stream).await?;
        if hello["op"] != 10 {
            bail!("the gateway opened without HELLO: {hello}");
        }
        let interval = hello["d"]["heartbeat_interval"]
            .as_u64()
            .context("HELLO carries a heartbeat interval")?;

        let (outgoing, mut outbox) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(payload) = outbox.recv().await {
                if sink.send(Message::text(payload.to_string())).await.is_err() {
                    break;
                }
            }
        });
        let sequence = Arc::new(AtomicI64::new(-1));
        let beat = {
            let outgoing = outgoing.clone();
            let sequence = sequence.clone();
            move || {
                let last = sequence.load(Ordering::Relaxed);
                let d = if last < 0 { Value::Null } else { json!(last) };
                outgoing.send(json!({"op": 1, "d": d})).is_ok()
            }
        };
        {
            let beat = beat.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_millis(interval));
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    if !beat() {
                        break;
                    }
                }
            });
        }
        outgoing
            .send(json!({
                "op": 2,
                "d": {
                    "token": token,
                    "intents": INTENTS,
                    "properties": {"os": "linux", "browser": "faculties-call", "device": "faculties-call"}
                }
            }))
            .map_err(|_| anyhow!("the gateway writer stopped"))?;

        // Every payload goes to the joiner until it has what it needs; the
        // same task keeps reading afterwards and answers for the session.
        let (forward, incoming) = mpsc::unbounded_channel::<Value>();
        let watch = tokio::spawn(async move {
            loop {
                let payload = next_payload(&mut stream).await?;
                if let Some(value) = payload["s"].as_i64() {
                    sequence.store(value, Ordering::Relaxed);
                }
                match payload["op"].as_u64() {
                    Some(1) => {
                        beat();
                    }
                    Some(7) => bail!("the gateway asked for a reconnect"),
                    Some(9) => bail!("the gateway invalidated the session"),
                    _ => {}
                }
                let _ = forward.send(payload);
            }
        });
        Ok(Self {
            outgoing,
            incoming,
            user: None,
            watch,
        })
    }

    /// Ask to join `channel` and collect what the voice driver needs.
    async fn join_voice(
        &mut self,
        guild: NonZeroU64,
        channel: NonZeroU64,
    ) -> Result<ConnectionInfo> {
        let mut session_id = None;
        let mut server: Option<(String, String)> = None;
        loop {
            let payload = self
                .incoming
                .recv()
                .await
                .context("the gateway closed before the voice join completed")?;
            if payload["op"] != 0 {
                continue;
            }
            let d = &payload["d"];
            match payload["t"].as_str() {
                Some("READY") => {
                    let user = snowflake(&d["user"]["id"]).context("READY names the bot user")?;
                    self.user = Some(user);
                    self.outgoing
                        .send(json!({
                            "op": 4,
                            "d": {"guild_id": guild.to_string(), "channel_id": channel.to_string(),
                                  "self_mute": false, "self_deaf": false}
                        }))
                        .map_err(|_| anyhow!("the gateway writer stopped"))?;
                }
                Some("VOICE_STATE_UPDATE")
                    if self.user.is_some() && snowflake(&d["user_id"]) == self.user =>
                {
                    session_id = d["session_id"].as_str().map(str::to_owned);
                }
                Some("VOICE_SERVER_UPDATE") if snowflake(&d["guild_id"]) == Some(guild.get()) => {
                    // A null endpoint means the server is being allocated; a
                    // second update follows with one.
                    if let (Some(token), Some(endpoint)) =
                        (d["token"].as_str(), d["endpoint"].as_str())
                    {
                        server = Some((token.to_owned(), endpoint.to_owned()));
                    }
                }
                _ => {}
            }
            if let (Some(user), Some(session_id), Some((token, endpoint))) =
                (self.user, &session_id, &server)
            {
                return Ok(ConnectionInfo {
                    channel_id: ChannelId(channel),
                    endpoint: endpoint.clone(),
                    guild_id: GuildId(guild),
                    session_id: session_id.clone(),
                    token: token.clone(),
                    user_id: UserId(NonZeroU64::new(user).context("a nonzero bot user id")?),
                });
            }
        }
    }
}

/// The next JSON payload from the gateway; a close frame is an error that
/// says why Discord closed the session.
async fn next_payload<S>(stream: &mut S) -> Result<Value>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            None => bail!("the gateway closed the connection"),
            Some(Err(error)) => return Err(error).context("read from the gateway"),
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(text.as_str()).context("parse a gateway payload");
            }
            Some(Ok(Message::Close(frame))) => {
                bail!("the gateway closed the session: {frame:?}")
            }
            Some(Ok(_)) => {}
        }
    }
}

fn snowflake(value: &Value) -> Option<u64> {
    value.as_str().and_then(|id| id.parse().ok())
}
