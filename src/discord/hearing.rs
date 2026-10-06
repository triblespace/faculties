//! What the voice connection of `discord live` hears: the speech of everybody
//! in the voice channel but the bot itself, cut into utterances, transcribed,
//! and stored with its audio as messages in the voice channel, each under the
//! user who said it.
//!
//! songbird's receive path decodes every speaker's audio to 16 kHz mono and
//! hands it over once per 20 ms tick, keyed by SSRC; Discord says whose SSRC
//! is whose as each user starts speaking. The handler on the driver ([`Ears`])
//! maps the SSRCs Discord names, never the bot's own (which the driver learns
//! as it connects), and copies their audio out of a tick into a channel before
//! it returns; an SSRC Discord names nobody for is never heard. A thread of
//! its own ([`Listener`]) runs the energy segmenter `hear` uses on each heard
//! user's stream (Discord clients stop sending during silence, and a silent
//! tick counts as 20 ms of zeros), and transcribes every finished utterance
//! with `mary::hear`'s Voxtral streaming transcriber on CUDA, which loads
//! once, before the first one ([`Transcribe`] is the seam its backend changes
//! behind). The utterance goes to intake, the one writer of the discord
//! collection ([`super::intake::Work::Utterance`]), and is kept in the state
//! directory, transcript and audio, when that write fails or intake is gone.
//!
//! Logs carry who, where, when and how much, never what was said.
//!
//! Built with the `discord-hearing` feature.

use super::intake::{self, Inbox, Work};
use crate::discord::Utterance;
use crate::hear::segmenter::{Segment, Segmenter, VadConfig};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

/// The rate everything here runs at: the rate Voxtral hears, which songbird
/// decodes to ([`decode_mode`]), the segmenter cuts at, and the audio is kept
/// at.
pub const RATE: usize = mary::models::voxtral::config::SAMPLE_RATE;
/// Samples in one 20 ms voice tick at [`RATE`].
const TICK: usize = RATE / 50;
/// A heard user's stream that went this long without a tick (its SSRC
/// unknown meanwhile) ends, and what comes after begins a new one, so an
/// utterance's time is never taken from a clock that stood still.
const GAP_MS: u64 = 1_000;
/// Let an ordinary conversational pause remain inside a Discord utterance.
/// This adds 500 ms to an isolated silence close versus shared VAD defaults;
/// retained tail, preroll, max duration and explicit close rules stay unchanged.
const END_SILENCE_MS: usize = 1_200;

/// What hearing runs with.
pub struct Config {
    /// The Voxtral model pile, weights and tokenizer.
    pub model: PathBuf,
    /// Where an utterance intake cannot take is kept.
    pub unstored: PathBuf,
}

/// The songbird decoding hearing needs: mono at [`RATE`], as 16-bit samples
/// the [`Listener`] turns into f32.
pub fn decode_mode() -> songbird::driver::DecodeMode {
    songbird::driver::DecodeMode::Decode(songbird::driver::DecodeConfig::new(
        songbird::driver::Channels::Mono,
        songbird::driver::SampleRate::Hz16000,
    ))
}

/// Start hearing on `driver`, which must decode with [`decode_mode`]: the
/// handler goes on the driver, and the thread that segments, transcribes and
/// hands utterances to `intake` runs until the driver lets the handler go.
pub fn start(
    config: Config,
    channel: u64,
    intake: Inbox,
    driver: &mut songbird::Driver,
) -> Result<()> {
    let (heard, moments) = mpsc::channel();
    let receiver = Receiver {
        ears: Arc::new(Mutex::new(Ears::default())),
        heard,
    };
    for event in [
        songbird::CoreEvent::DriverConnect,
        songbird::CoreEvent::DriverReconnect,
        songbird::CoreEvent::SpeakingStateUpdate,
        songbird::CoreEvent::VoiceTick,
        songbird::CoreEvent::ClientDisconnect,
    ] {
        driver.add_global_event(songbird::Event::Core(event), receiver.clone());
    }
    let (model, unstored) = (config.model, config.unstored);
    std::thread::Builder::new()
        .name("discord-hearing".to_owned())
        .spawn(move || {
            let load = move || -> Result<Box<dyn Transcribe>> {
                Ok(Box::new(voxtral::Voxtral::load(&model)?))
            };
            hear(moments, load, channel, &intake, &unstored);
        })
        .context("spawn the hearing thread")?;
    Ok(())
}

/// One speaker's share of a voice tick, or their leaving.
#[derive(Debug, PartialEq)]
pub enum Heard {
    /// 20 ms (or a packet's worth) of what they said, mono at [`RATE`].
    Audio { user: u64, samples: Vec<i16> },
    /// They are in the call and sent nothing this tick.
    Silence { user: u64 },
    /// They left the call.
    Left { user: u64 },
}

/// What one voice tick (or one departure) brought, and when.
#[derive(Debug)]
pub struct Moment {
    /// Wall time, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub heard: Vec<Heard>,
}

/// What one SSRC sent in a tick, as the handler looks it up.
pub enum Sent<'a> {
    Audio(&'a [i16]),
    /// Known in the call, nothing decoded this tick.
    Silent,
    /// Not in this tick at all.
    Absent,
}

/// Which SSRCs are heard, and as whom: those Discord said belong to a user,
/// but never the bot's own. Everything else in a tick is never looked at.
#[derive(Debug, Default)]
pub struct Ears {
    /// The SSRC the bot sends with on its current connection.
    own: Option<u32>,
    speakers: HashMap<u32, u64>,
}

impl Ears {
    /// The driver connected (again) and sends with `ssrc`: that SSRC is the
    /// bot's own, and is not heard.
    pub fn connected(&mut self, ssrc: u32) {
        self.own = Some(ssrc);
        self.speakers.remove(&ssrc);
    }

    /// Discord says `ssrc` is `user` (a speaking state update). An SSRC of
    /// nobody named, or the bot's own, is forgotten, so that its audio is
    /// never taken for anybody's.
    pub fn speaking(&mut self, ssrc: u32, user: Option<u64>) {
        match user.filter(|_| self.own != Some(ssrc)) {
            Some(user) => {
                // A user has one SSRC at a time.
                self.speakers.retain(|_, heard| *heard != user);
                self.speakers.insert(ssrc, user);
            }
            None => {
                self.speakers.remove(&ssrc);
            }
        }
    }

    /// `user` left the call: their SSRC is forgotten. Whether they were
    /// heard, so that what they were saying is closed.
    pub fn left(&mut self, user: u64) -> bool {
        let before = self.speakers.len();
        self.speakers.retain(|_, heard| *heard != user);
        self.speakers.len() != before
    }

    /// One tick of the heard users, each looked up by SSRC through `sent`:
    /// only their SSRCs are ever asked about, and only their audio copied.
    pub fn tick<'a>(&self, sent: impl Fn(u32) -> Sent<'a>) -> Vec<Heard> {
        let mut heard = Vec::new();
        for (&ssrc, &user) in &self.speakers {
            match sent(ssrc) {
                Sent::Audio(samples) => heard.push(Heard::Audio {
                    user,
                    samples: samples.to_vec(),
                }),
                Sent::Silent => heard.push(Heard::Silence { user }),
                Sent::Absent => {}
            }
        }
        heard
    }
}

/// The handler on the driver: learns the bot's own SSRC as the driver
/// connects, maps the others as Discord names them, and copies each tick's
/// audio of the heard users into the hearing thread's channel without
/// waiting for it.
#[derive(Clone)]
struct Receiver {
    ears: Arc<Mutex<Ears>>,
    heard: mpsc::Sender<Moment>,
}

impl Receiver {
    fn ears(&self) -> std::sync::MutexGuard<'_, Ears> {
        self.ears
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn send(&self, heard: Vec<Heard>) {
        if !heard.is_empty() {
            // A hearing thread that is gone (its model did not load) hears
            // nothing more; the voice connection goes on.
            let _ = self.heard.send(Moment {
                at_ms: now_ms(),
                heard,
            });
        }
    }
}

#[async_trait::async_trait]
impl songbird::EventHandler for Receiver {
    async fn act(&self, context: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        match context {
            songbird::EventContext::DriverConnect(connect)
            | songbird::EventContext::DriverReconnect(connect) => {
                self.ears().connected(connect.ssrc);
            }
            songbird::EventContext::SpeakingStateUpdate(speaking) => {
                self.ears()
                    .speaking(speaking.ssrc, speaking.user_id.map(|user| user.0));
            }
            songbird::EventContext::VoiceTick(tick) => {
                let heard = self.ears().tick(|ssrc| match tick.speaking.get(&ssrc) {
                    Some(data) => data
                        .decoded_voice
                        .as_deref()
                        .map_or(Sent::Silent, Sent::Audio),
                    None if tick.silent.contains(&ssrc) => Sent::Silent,
                    None => Sent::Absent,
                });
                self.send(heard);
            }
            songbird::EventContext::ClientDisconnect(gone) => {
                let user = gone.user_id.0;
                if self.ears().left(user) {
                    self.send(vec![Heard::Left { user }]);
                }
            }
            _ => {}
        }
        None
    }
}

/// A finished utterance of one user, mono at [`RATE`].
#[derive(Debug)]
pub struct Spoken {
    pub user: u64,
    /// When it began, in milliseconds since the Unix epoch.
    pub start_ms: u64,
    pub samples: Vec<f32>,
}

/// One heard user's stream: the segmenter running over it, and the wall time
/// of its first sample, from which its utterances are timed.
struct Stream {
    segmenter: Segmenter,
    origin_ms: u64,
    fed: u64,
    committed: usize,
}

impl Stream {
    fn new(origin_ms: u64) -> Self {
        Self {
            segmenter: Segmenter::new(
                RATE,
                VadConfig::default().with_end_silence_ms(END_SILENCE_MS),
            ),
            origin_ms,
            fed: 0,
            committed: 0,
        }
    }

    /// Where the stream's clock stands.
    fn now_ms(&self) -> u64 {
        self.origin_ms + self.fed * 1000 / RATE as u64
    }

    fn push(&mut self, user: u64, samples: &[f32], emit: &mut impl FnMut(Progress)) {
        let origin_ms = self.origin_ms;
        let mut completed = Vec::new();
        self.segmenter
            .push(samples, &mut |segment| completed.push(segment));
        for segment in completed {
            emit(Progress::Finished(spoken(user, origin_ms, segment)));
            self.committed = 0;
        }
        if let Some((start, prefix)) = self.segmenter.committed_prefix() {
            if prefix.len() > self.committed {
                emit(Progress::Samples {
                    user,
                    start_ms: origin_ms + (start as f64 / RATE as f64 * 1000.0).round() as u64,
                    offset: self.committed,
                    samples: prefix[self.committed..].to_vec(),
                });
                self.committed = prefix.len();
            }
        }
        self.fed += samples.len() as u64;
    }

    /// The stream ends: what was being said is finished.
    fn close(mut self, user: u64, emit: &mut impl FnMut(Progress)) {
        let origin_ms = self.origin_ms;
        self.segmenter
            .flush(&mut |segment| emit(Progress::Finished(spoken(user, origin_ms, segment))));
    }
}

fn spoken(user: u64, origin_ms: u64, segment: Segment) -> Spoken {
    Spoken {
        user,
        start_ms: origin_ms + (segment.start_s * 1000.0).round() as u64,
        samples: segment.samples,
    }
}

/// Private handoff on the existing hearing thread; no partial intake output.
enum Progress {
    Samples {
        user: u64,
        start_ms: u64,
        offset: usize,
        samples: Vec<f32>,
    },
    Finished(Spoken),
}

/// The heard users' streams, cut into utterances by the energy segmenter
/// `hear` uses. Only what [`Ears`] let through ever reaches it.
#[derive(Default)]
pub struct Listener {
    streams: HashMap<u64, Stream>,
}

impl Listener {
    pub fn hear(&mut self, moment: Moment, emit: &mut impl FnMut(Spoken)) {
        self.hear_progress(moment, &mut |progress| {
            if let Progress::Finished(spoken) = progress {
                emit(spoken);
            }
        });
    }

    fn hear_progress(&mut self, moment: Moment, emit: &mut impl FnMut(Progress)) {
        for heard in moment.heard {
            match heard {
                Heard::Audio { user, samples } => {
                    let samples: Vec<f32> = samples
                        .iter()
                        .map(|&sample| f32::from(sample) / 32768.0)
                        .collect();
                    self.feed(user, moment.at_ms, &samples, emit);
                }
                Heard::Silence { user } => self.feed(user, moment.at_ms, &[0.0; TICK], emit),
                Heard::Left { user } => {
                    if let Some(stream) = self.streams.remove(&user) {
                        stream.close(user, emit);
                    }
                }
            }
        }
    }

    fn feed(&mut self, user: u64, at_ms: u64, samples: &[f32], emit: &mut impl FnMut(Progress)) {
        let stalled = self
            .streams
            .get(&user)
            .is_some_and(|stream| at_ms > stream.now_ms() + GAP_MS);
        if stalled {
            if let Some(stream) = self.streams.remove(&user) {
                stream.close(user, emit);
            }
        }
        self.streams
            .entry(user)
            .or_insert_with(|| Stream::new(at_ms))
            .push(user, samples, emit);
    }

    /// Hearing ends: everything being said is finished.
    pub fn flush(&mut self, emit: &mut impl FnMut(Spoken)) {
        self.flush_progress(&mut |progress| {
            if let Progress::Finished(spoken) = progress {
                emit(spoken);
            }
        });
    }

    fn flush_progress(&mut self, emit: &mut impl FnMut(Progress)) {
        for (user, stream) in self.streams.drain() {
            stream.close(user, emit);
        }
    }
}

/// What turns one utterance, mono samples at [`RATE`], into text. The model
/// and its backend sit behind it, so the backend can change without touching
/// what calls it.
pub trait Transcribe {
    fn listen(&self) -> Result<Box<dyn Transcription + '_>>;

    fn transcribe(&self, samples: &[f32]) -> Result<String> {
        let mut session = self.listen()?;
        session.push(samples)?;
        session.finish()
    }
}

/// One utterance, borrowed from a model owned outside all active sessions.
pub trait Transcription {
    fn push(&mut self, samples: &[f32]) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<String>;
}

struct Active<'a> {
    start_ms: u64,
    fed: usize,
    compute_seconds: f64,
    session: Result<Box<dyn Transcription + 'a>>,
}

impl<'a> Active<'a> {
    fn new(ear: &'a dyn Transcribe, start_ms: u64) -> Self {
        let started = Instant::now();
        let session = ear.listen();
        Self {
            start_ms,
            fed: 0,
            compute_seconds: started.elapsed().as_secs_f64(),
            session,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        let started = Instant::now();
        if let Ok(session) = &mut self.session {
            if let Err(error) = session.push(samples) {
                // Retain the failure until the full captured utterance closes.
                // No partial transcript is published and no audio is dropped.
                self.session = Err(error);
            }
        }
        self.fed += samples.len();
        self.compute_seconds += started.elapsed().as_secs_f64();
    }
}

fn advance<'a>(
    ear: &'a dyn Transcribe,
    active: &mut Option<(u64, Active<'a>)>,
    progress: Progress,
    channel: u64,
    intake: &Inbox,
    unstored: &Path,
) {
    match progress {
        Progress::Samples {
            user,
            start_ms,
            offset,
            samples,
        } => {
            // A deferred/preempted utterance may never restart mid-prefix.
            // Its authoritative complete PCM still lives in its Segmenter.
            if active.is_none() && offset == 0 {
                *active = Some((user, Active::new(ear, start_ms)));
            }
            if let Some((owner, current)) = active.as_mut() {
                if *owner == user {
                    if current.start_ms != start_ms || current.fed != offset {
                        current.session = Err(anyhow::anyhow!(
                            "hearing committed-prefix boundary mismatch"
                        ));
                    }
                    current.push(&samples);
                }
            }
        }
        Progress::Finished(spoken) => {
            if active
                .as_ref()
                .is_some_and(|(user, _)| *user == spoken.user)
            {
                let (_, mut current) = active.take().expect("matching owner");
                let result =
                    if current.start_ms != spoken.start_ms || current.fed > spoken.samples.len() {
                        Err(anyhow::anyhow!(
                            "hearing committed-prefix boundary mismatch"
                        ))
                    } else {
                        current.push(&spoken.samples[current.fed..]);
                        let started = Instant::now();
                        let result = current.session.and_then(|session| session.finish());
                        current.compute_seconds += started.elapsed().as_secs_f64();
                        result
                    };
                store_result(
                    result,
                    spoken,
                    current.compute_seconds,
                    channel,
                    intake,
                    unstored,
                );
            } else {
                // Release unpublished recognition BEFORE the batch factory.
                // Drop only model state/text: the preempted user's full PCM is
                // retained and will batch at its unchanged completion boundary.
                // A failed slot holds only an error, not a Transcription: keep
                // it until its owner closes so fallback cannot hide the error.
                if active
                    .as_ref()
                    .is_some_and(|(_, current)| current.session.is_ok())
                {
                    *active = None;
                }
                let started = Instant::now();
                let result = ear.transcribe(&spoken.samples);
                store_result(
                    result,
                    spoken,
                    started.elapsed().as_secs_f64(),
                    channel,
                    intake,
                    unstored,
                );
            }
        }
    }
}

/// The hearing thread: loads the transcriber, then cuts what comes in into
/// utterances and stores each, until the handler lets go of the channel.
fn hear(
    moments: mpsc::Receiver<Moment>,
    load: impl FnOnce() -> Result<Box<dyn Transcribe>>,
    channel: u64,
    intake: &Inbox,
    unstored: &Path,
) {
    let started = Instant::now();
    let ear = match load() {
        Ok(ear) => ear,
        Err(error) => {
            eprintln!("[discord] hearing is off: the transcriber did not load: {error:#}");
            return;
        }
    };
    eprintln!(
        "[discord] hearing ready (transcriber loaded in {:.1} s)",
        started.elapsed().as_secs_f64()
    );
    // One loaded owner and at most one borrowed recognition session. All GPU
    // work stays on this thread; deferred users keep their exact captured PCM.
    let mut active = None;
    let mut listener = Listener::default();
    let mut emit = |progress| advance(&*ear, &mut active, progress, channel, intake, unstored);
    for moment in moments.iter() {
        listener.hear_progress(moment, &mut emit);
    }
    listener.flush_progress(&mut emit);
    debug_assert!(active.is_none());
}

/// Transcribe one utterance and hand it to intake; keep it in `unstored`
/// when intake is gone, and keep its audio there when it cannot be
/// transcribed. One with no words in it is not stored.
#[cfg(test)]
fn store(ear: &mut dyn Transcribe, spoken: Spoken, channel: u64, intake: &Inbox, unstored: &Path) {
    let started = Instant::now();
    let transcribed = ear.transcribe(&spoken.samples);
    let took = started.elapsed().as_secs_f64();
    store_result(transcribed, spoken, took, channel, intake, unstored);
}

fn store_result(
    transcribed: Result<String>,
    spoken: Spoken,
    took: f64,
    channel: u64,
    intake: &Inbox,
    unstored: &Path,
) {
    let seconds = spoken.samples.len() as f64 / RATE as f64;
    let mut utterance = Utterance {
        channel,
        user: spoken.user,
        start_ms: spoken.start_ms,
        transcript: String::new(),
        wav: wav_pcm16(&spoken.samples),
    };
    match transcribed {
        Ok(transcript) if transcript.trim().is_empty() => {
            eprintln!(
                "[discord] heard {seconds:.1} s from user {} with no words in it ({took:.1} s); \
                 not stored",
                spoken.user
            );
            return;
        }
        Ok(transcript) => utterance.transcript = transcript.trim().to_owned(),
        Err(error) => {
            eprintln!(
                "[discord] transcribing {seconds:.1} s from user {} failed: {error:#}; its \
                 audio is kept in {}",
                spoken.user,
                unstored.display()
            );
            intake::keep_utterance(unstored, &utterance);
            return;
        }
    }
    eprintln!(
        "[discord] heard {seconds:.1} s from user {}, transcribed in {took:.1} s ({} \
         characters)",
        spoken.user,
        utterance.transcript.chars().count()
    );
    if let Err(Work::Utterance(utterance)) = intake.send(Work::Utterance(utterance)) {
        eprintln!(
            "[discord] intake has stopped; the utterance is kept in {}",
            unstored.display()
        );
        intake::keep_utterance(unstored, &utterance);
    }
}

/// `discord hear`: a recorded clip, mono at [`RATE`], heard the way the
/// voice channel is but without Discord or a pile: one speaker's 20 ms ticks
/// through the same [`Listener`] and transcriber. Each utterance is written
/// to `out` with where it began in the clip, how long it is, how long it took
/// to transcribe, and its words, since the clip is the caller's own. Nothing
/// is stored.
pub fn hear_clip(model: &Path, clip: &[f32], out: &mut crate::out::Out<'_>) -> Result<()> {
    let started = Instant::now();
    let mut ear = voxtral::Voxtral::load(model)?;
    out.line(format!(
        "transcriber loaded in {:.1} s",
        started.elapsed().as_secs_f64()
    ))?;
    let mut listener = Listener::default();
    let mut spoken = Vec::new();
    for (tick, samples) in (0..).zip(clip.chunks(TICK)) {
        let samples = samples.iter().map(|&s| (s * 32768.0) as i16).collect();
        let moment = Moment {
            at_ms: 20 * tick,
            heard: vec![Heard::Audio { user: 0, samples }],
        };
        listener.hear(moment, &mut |s| spoken.push(s));
    }
    listener.flush(&mut |s| spoken.push(s));
    for spoken in spoken {
        let started = Instant::now();
        let text = ear.transcribe(&spoken.samples)?;
        out.line(format!(
            "at {:.2} s, {:.2} s of audio transcribed in {:.1} s: {}",
            spoken.start_ms as f64 / 1000.0,
            spoken.samples.len() as f64 / RATE as f64,
            started.elapsed().as_secs_f64(),
            text.trim()
        ))?;
    }
    Ok(())
}

/// A 16-bit mono PCM WAV of `samples` at [`RATE`]. They came from 16-bit PCM
/// divided by 32768, so they go back exactly.
pub fn wav_pcm16(samples: &[f32]) -> Vec<u8> {
    let data = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&(RATE as u32).to_le_bytes());
    wav.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        let pcm = (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
        wav.extend_from_slice(&pcm.to_le_bytes());
    }
    wav
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// mary's Voxtral-Mini-4B-Realtime, streaming on CUDA (`mary::hear`, with
/// mary's `voxtral-cuda`), behind [`Transcribe`].
mod voxtral {
    use super::{Transcribe, Transcription};
    use anyhow::{Context, Result};
    use std::path::Path;

    /// How far the text lags the audio, mary's default for the stream.
    const DELAY_MS: usize = 480;

    pub struct Voxtral {
        ears: mary::hear::Ears,
    }

    impl Voxtral {
        /// Load the weights and the tokenizer from the model pile, and
        /// compile the stream's kernels before the first utterance.
        pub fn load(model: &Path) -> Result<Self> {
            let ears = mary::hear::Ears::load(model)
                .with_context(|| format!("load Voxtral from {}", model.display()))?;
            Ok(Self { ears })
        }
    }

    struct Session<'a> {
        ears: &'a mary::hear::Ears,
        listening: Option<mary::hear::Listening<'a>>,
        text: String,
    }

    impl Transcribe for Voxtral {
        fn listen(&self) -> Result<Box<dyn Transcription + '_>> {
            Ok(Box::new(Session {
                ears: &self.ears,
                listening: Some(self.ears.listen(DELAY_MS)),
                text: String::new(),
            }))
        }
    }

    impl Transcription for Session<'_> {
        fn push(&mut self, mut samples: &[f32]) -> Result<()> {
            while !samples.is_empty() {
                let listening = self.listening.as_mut().expect("active stream");
                anyhow::ensure!(
                    !listening.is_finished(),
                    "Voxtral ended before the utterance's remaining audio"
                );
                if listening.is_full() {
                    self.text += &self.listening.take().unwrap().finish();
                    self.listening = Some(self.ears.listen(DELAY_MS));
                    continue;
                }
                let (now, later) = samples.split_at(samples.len().min(listening.room()));
                self.text += &listening.push(now);
                samples = later;
            }
            Ok(())
        }

        fn finish(mut self: Box<Self>) -> Result<String> {
            self.text += &self.listening.take().expect("finish once").finish();
            Ok(self.text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const JP: u64 = 100000000000000400;
    const OTHER: u64 = 100000000000000500;
    const BOT: u64 = 100000000000000600;

    // Uses only existing 1ead APIs, allowing a test-only baseline transplant:
    // Listener -> advance -> fake Transcribe decides the actual boundary.
    #[test]
    fn ordinary_pause_stays_one_final_utterance_with_exact_pcm() {
        let ear = Meter::default();
        let mut active = None;
        let mut listener = Listener::default();
        let directory = tempfile::tempdir().unwrap();
        let (sender, queue) = mpsc::channel();
        let worker = intake::Worker::from_sender(sender);
        let intake = worker.inbox().unwrap();
        let script = [
            (false, 600),
            (true, 800),
            (false, 900),
            (true, 800),
            (false, 1200),
        ];
        let moments = ticks(JP, 0, &script);
        let all_pcm: Vec<f32> = moments
            .iter()
            .flat_map(|m| match &m.heard[0] {
                Heard::Audio { samples, .. } => samples
                    .iter()
                    .map(|&s| f32::from(s) / 32768.0)
                    .collect::<Vec<_>>(),
                Heard::Silence { .. } => vec![0.0; TICK],
                Heard::Left { .. } => unreachable!(),
            })
            .collect();
        let mut captured = Vec::new();
        let mut published = Vec::new();
        let mut pushed_before_pause = false;
        let mut published_early = false;
        for moment in moments {
            let end_ms = moment.at_ms + 20;
            listener.hear_progress(moment, &mut |progress| {
                if let Progress::Finished(ref spoken) = progress {
                    captured.push((spoken.user, spoken.start_ms, spoken.samples.clone()));
                }
                advance(&ear, &mut active, progress, 7, &intake, directory.path());
            });
            if end_ms == 1400 {
                pushed_before_pause = ear.attempts.borrow().iter().any(|p| !p.is_empty());
            }
            while let Ok(work) = queue.try_recv() {
                let Work::Utterance(item) = work else {
                    panic!("unexpected intake work")
                };
                published_early |= end_ms < 4300;
                published.push(item);
            }
        }
        // Expected behavioral RED on 1ead: two publications, not one. This
        // assertion precedes the no-early-publication checks.
        assert_eq!(published.len(), 1, "900 ms pause must not split the turn");
        assert_eq!(captured.len(), 1);
        assert!(
            pushed_before_pause,
            "recognition must stay online during speech"
        );
        assert!(!published_early, "no partial news before the 1200 ms close");
        assert!(active.is_none());
        assert_eq!(ear.peak.get(), 1);
        assert_eq!(ear.live.get(), 0);
        assert_eq!(*ear.finished.borrow(), [0]);
        // Three onset frames complete at660ms; the unchanged240ms preroll
        // begins at420ms. Second burst ends3100ms; keep exactly200ms tail.
        let expected = &all_pcm[RATE * 420 / 1000..RATE * 3300 / 1000];
        let (user, start, pcm) = &captured[0];
        assert_eq!((*user, *start), (JP, 420));
        assert_eq!(pcm, expected);
        assert_eq!(ear.attempts.borrow().as_slice(), &[expected.to_vec()]);
        let pause = (RATE * (1400 - 420) / 1000)..(RATE * (2300 - 420) / 1000);
        assert!(pcm[pause].iter().all(|&s| s == 0.0));
        assert!(pcm[pcm.len() - RATE / 5..].iter().all(|&s| s == 0.0));
        assert!(
            pcm[pcm.len() - RATE / 5 - TICK..pcm.len() - RATE / 5]
                .iter()
                .any(|&s| s != 0.0),
            "tail must follow actual speech"
        );
        assert_eq!((published[0].user, published[0].start_ms), (JP, 420));
        assert_eq!(published[0].transcript, "complete 0");
        assert_eq!(published[0].wav, wav_pcm16(expected));
        listener.flush_progress(&mut |p| {
            advance(&ear, &mut active, p, 7, &intake, directory.path());
        });
        assert!(
            queue.try_recv().is_err(),
            "shutdown must not republish the turn"
        );
    }

    #[test]
    fn failed_online_owner_is_not_retried_or_lost_when_another_user_finishes() {
        let ear = Meter {
            fail_first: true,
            ..Meter::default()
        };
        let mut active = None;
        let mut listener = Listener::default();
        let directory = tempfile::tempdir().unwrap();
        let (sender, queue) = mpsc::channel();
        let worker = intake::Worker::from_sender(sender);
        let intake = worker.inbox().unwrap();
        let original = listen(ticks(JP, 0, &[(false, 600), (true, 800)]));
        let mut emit = |p| advance(&ear, &mut active, p, 7, &intake, directory.path());
        for moment in ticks(JP, 0, &[(false, 600), (true, 800)]) {
            listener.hear_progress(moment, &mut emit);
        }
        assert_eq!(ear.live.get(), 0, "failed session released immediately");
        for moment in ticks(OTHER, 0, &[(false, 600), (true, 800), (false, 1200)]) {
            listener.hear_progress(moment, &mut emit);
        }
        listener.hear_progress(
            Moment {
                at_ms: 1400,
                heard: vec![Heard::Left { user: JP }],
            },
            &mut emit,
        );
        listener.flush_progress(&mut emit);
        assert!(active.is_none());
        assert_eq!(ear.peak.get(), 1);
        assert_eq!(ear.live.get(), 0);
        assert_eq!(
            ear.attempts.borrow().len(),
            2,
            "failed owner was not batch-retried"
        );
        assert_eq!(*ear.finished.borrow(), [1]);
        let Ok(Work::Utterance(other)) = queue.try_recv() else {
            panic!("unrelated user missing")
        };
        assert_eq!(other.user, OTHER);
        assert_eq!(other.transcript, "complete 1");
        assert!(
            queue.try_recv().is_err(),
            "failure cannot become invented success"
        );
        let path = directory
            .path()
            .join(format!("7-{JP}-{}.wav", original[0].start_ms));
        assert_eq!(
            std::fs::read(path).unwrap(),
            wav_pcm16(&original[0].samples)
        );
    }

    #[derive(Default)]
    struct Meter {
        live: std::cell::Cell<usize>,
        peak: std::cell::Cell<usize>,
        attempts: std::cell::RefCell<Vec<Vec<f32>>>,
        finished: std::cell::RefCell<Vec<usize>>,
        fail_first: bool,
    }
    struct MeterSession<'a> {
        owner: &'a Meter,
        id: usize,
    }
    impl Drop for MeterSession<'_> {
        fn drop(&mut self) {
            self.owner.live.set(self.owner.live.get() - 1);
        }
    }
    impl Transcribe for Meter {
        fn listen(&self) -> Result<Box<dyn Transcription + '_>> {
            let id = self.attempts.borrow().len();
            self.attempts.borrow_mut().push(Vec::new());
            self.live.set(self.live.get() + 1);
            self.peak.set(self.peak.get().max(self.live.get()));
            Ok(Box::new(MeterSession { owner: self, id }))
        }
    }
    impl Transcription for MeterSession<'_> {
        fn push(&mut self, samples: &[f32]) -> Result<()> {
            self.owner.attempts.borrow_mut()[self.id].extend_from_slice(samples);
            if self.owner.fail_first && self.id == 0 {
                anyhow::bail!("injected early error");
            }
            Ok(())
        }
        fn finish(self: Box<Self>) -> Result<String> {
            self.owner.finished.borrow_mut().push(self.id);
            Ok(format!("complete {}", self.id))
        }
    }

    #[test]
    fn one_live_session_even_for_stranded_users_and_nonowner_completion() {
        let ear = Meter::default();
        // Type inferred from the actual adapter, so this identical test also
        // runs on f752's per-user runtime without a mechanical runtime shim.
        let mut active = Default::default();
        let mut listener = Listener::default();
        let directory = tempfile::tempdir().unwrap();
        let (sender, queue) = mpsc::channel();
        let worker = intake::Worker::from_sender(sender);
        let intake = worker.inbox().unwrap();
        let mut captured = Vec::new();
        let mut emit = |progress: Progress| {
            if let Progress::Finished(ref s) = progress {
                captured.push((s.user, s.start_ms, s.samples.clone()));
            }
            advance(&ear, &mut active, progress, 7, &intake, directory.path());
        };
        // JP strands an active utterance. OTHER speaks while it remains open.
        for moment in ticks(JP, 0, &[(false, 600), (true, 800)]) {
            listener.hear_progress(moment, &mut emit);
        }
        assert_eq!(ear.live.get(), 1);
        assert!(
            !ear.attempts.borrow()[0].is_empty(),
            "single speaker remains online"
        );
        for moment in ticks(OTHER, 0, &[(false, 600), (true, 800)]) {
            listener.hear_progress(moment, &mut emit);
        }
        assert!(queue.try_recv().is_err(), "no partial publication");
        for moment in ticks(OTHER, 1400, &[(false, 1200)]) {
            listener.hear_progress(moment, &mut emit);
        }
        // The old online owner's later nonzero-offset suffix must not begin
        // a new partial session. Its final batch must start from full PCM.
        for moment in ticks(JP, 1400, &[(true, 200)]) {
            listener.hear_progress(moment, &mut emit);
        }
        listener.hear_progress(
            Moment {
                at_ms: 1600,
                heard: vec![Heard::Left { user: JP }],
            },
            &mut emit,
        );
        for moment in ticks(OTHER, 2600, &[(false, 600), (true, 800), (false, 1200)]) {
            listener.hear_progress(moment, &mut emit);
        }
        listener.flush_progress(&mut emit);
        assert!(
            ear.peak.get() <= 1,
            "more than one Transcription object alive"
        );
        assert_eq!(ear.live.get(), 0);
        assert_eq!(captured.len(), 3);
        assert_eq!(
            ear.attempts.borrow().len(),
            4,
            "one abandoned prefix, three complete attempts"
        );
        assert_eq!(*ear.finished.borrow(), [1, 2, 3]);
        for ((user, start, pcm), id) in captured.iter().zip([1, 2, 3]) {
            assert_eq!(
                ear.attempts.borrow()[id],
                *pcm,
                "successful attempt must receive full exact PCM"
            );
            let Ok(Work::Utterance(item)) = queue.try_recv() else {
                panic!("final utterance missing")
            };
            assert_eq!((item.user, item.start_ms), (*user, *start));
            assert_eq!(item.wav, wav_pcm16(pcm));
            assert_eq!(item.transcript, format!("complete {id}"));
        }
        let attempts = ear.attempts.borrow();
        assert!(attempts[0].len() < attempts[2].len());
        assert_eq!(attempts[0], attempts[2][..attempts[0].len()]);
        assert!(queue.try_recv().is_err());
    }

    #[test]
    fn normal_vad_maximum_fits_listening_room_without_truncation() {
        use mary::models::voxtral::config::{
            delay_tokens, N_FFT, N_LEFT_PAD_TOKENS, OFFLINE_BUFFER_TOKENS, SAMPLES_PER_TOK,
        };
        let tail =
            (delay_tokens(480) + 1 + OFFLINE_BUFFER_TOKENS) * SAMPLES_PER_TOK + SAMPLES_PER_TOK;
        let room =
            (mary::hear::MAX_TOKENS - N_LEFT_PAD_TOKENS - 1) * SAMPLES_PER_TOK - tail - N_FFT;
        assert!(
            room > 28 * RATE + TICK,
            "normal VAD segments must not fill Listening"
        );
    }

    #[derive(Default)]
    struct Recorded {
        sessions: std::cell::RefCell<Vec<Vec<f32>>>,
        finished: std::cell::RefCell<Vec<usize>>,
    }
    struct Recording<'a> {
        owner: &'a Recorded,
        id: usize,
    }
    impl Transcribe for Recorded {
        fn listen(&self) -> Result<Box<dyn Transcription + '_>> {
            let id = self.sessions.borrow().len();
            self.sessions.borrow_mut().push(Vec::new());
            Ok(Box::new(Recording { owner: self, id }))
        }
    }
    impl Transcription for Recording<'_> {
        fn push(&mut self, samples: &[f32]) -> Result<()> {
            self.owner.sessions.borrow_mut()[self.id].extend_from_slice(samples);
            Ok(())
        }
        fn finish(self: Box<Self>) -> Result<String> {
            self.owner.finished.borrow_mut().push(self.id);
            Ok(format!("session {}", self.id))
        }
    }

    fn exact_progress(moments: Vec<Moment>) -> usize {
        let owner = Recorded::default();
        let mut active = None;
        let mut listener = Listener::default();
        let directory = tempfile::tempdir().unwrap();
        let (sender, queue) = mpsc::channel();
        let worker = intake::Worker::from_sender(sender);
        let intake = worker.inbox().unwrap();
        let mut completed = Vec::new();
        let mut emit = |progress: Progress| {
            if let Progress::Finished(ref spoken) = progress {
                completed.push((spoken.user, spoken.start_ms, spoken.samples.clone()));
            }
            advance(&owner, &mut active, progress, 7, &intake, directory.path());
        };
        for moment in moments {
            listener.hear_progress(moment, &mut emit);
        }
        listener.flush_progress(&mut emit);
        assert!(active.is_none(), "borrowed session survived final flush");
        let finished = owner.finished.borrow();
        assert_eq!(finished.len(), completed.len());
        for ((user, start, samples), &id) in completed.iter().zip(finished.iter()) {
            let actual = &owner.sessions.borrow()[id];
            assert_eq!(
                actual.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                samples.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                "fed PCM differs from captured segment"
            );
            let Ok(Work::Utterance(item)) = queue.try_recv() else {
                panic!("missing finalized intake")
            };
            assert_eq!((item.user, item.start_ms), (*user, *start));
            assert_eq!(item.transcript, format!("session {id}"));
            assert_eq!(item.wav, wav_pcm16(samples));
        }
        assert!(queue.try_recv().is_err());
        completed.len()
    }

    #[test]
    fn committed_pcm_matches_capture_on_resume_trim_max_partial_gap_left_and_flush() {
        assert_eq!(
            exact_progress(ticks(
                JP,
                0,
                &[
                    (false, 600),
                    (true, 800),
                    (false, 500),
                    (true, 600),
                    (false, 1200),
                    (true, 900),
                    (false, 1200),
                ]
            )),
            2,
            "resume keeps intervening silence; final close trims only its tail"
        );
        assert_eq!(
            exact_progress(ticks(JP, 0, &[(false, 600), (true, 29000), (false, 1200),])),
            2,
            "max-duration close starts a separate session"
        );
        let mut partial = ticks(JP, 0, &[(false, 600), (true, 800)]);
        partial.push(Moment {
            at_ms: 1400,
            heard: vec![Heard::Audio {
                user: JP,
                samples: vec![1234; 17],
            }],
        });
        assert_eq!(exact_progress(partial), 1, "flush preserves partial frame");
        let mut gap = ticks(JP, 0, &[(false, 600), (true, 800)]);
        gap.extend(ticks(JP, 6400, &[(false, 600), (true, 800)]));
        gap.push(Moment {
            at_ms: 7800,
            heard: vec![Heard::Left { user: JP }],
        });
        assert_eq!(exact_progress(gap), 2, "gap and Left finalize separately");
    }

    #[test]
    fn arbitrary_single_feed_and_interleaved_users_keep_session_order() {
        let mut moments = ticks(JP, 0, &[(false, 600), (true, 600)]);
        // This ONE feed closes the already committed utterance, opens and
        // closes another, and leaves a third active. No callback-size premise.
        let samples = [(false, 1200), (true, 900), (false, 1200), (true, 900)]
            .into_iter()
            .flat_map(|(loud, ms)| {
                if loud {
                    tone(ms).into_iter().flatten().collect::<Vec<_>>()
                } else {
                    vec![0; RATE * ms / 1000]
                }
            })
            .collect();
        moments.push(Moment {
            at_ms: 1200,
            heard: vec![Heard::Audio { user: JP, samples }],
        });
        assert_eq!(exact_progress(moments), 3);
        let a = ticks(
            JP,
            0,
            &[(false, 600), (true, 1400), (false, 1200), (true, 600)],
        );
        let b = ticks(
            OTHER,
            0,
            &[(false, 600), (true, 800), (false, 1200), (true, 1200)],
        );
        let interleaved = a
            .into_iter()
            .zip(b)
            .map(|(mut a, b)| {
                a.heard.extend(b.heard);
                a
            })
            .collect();
        assert_eq!(
            exact_progress(interleaved),
            4,
            "distinct concurrent and repeated sessions"
        );
    }

    #[test]
    fn online_failure_keeps_whole_audio_and_next_utterance_resets() {
        struct FailsFirst(std::cell::Cell<bool>);
        struct Failing;
        impl Transcription for Failing {
            fn push(&mut self, _: &[f32]) -> Result<()> {
                anyhow::bail!("injected push failure")
            }
            fn finish(self: Box<Self>) -> Result<String> {
                panic!("failed stream must not finish")
            }
        }
        impl Transcribe for FailsFirst {
            fn listen(&self) -> Result<Box<dyn Transcription + '_>> {
                if !self.0.replace(true) {
                    Ok(Box::new(Failing))
                } else {
                    Ok(Box::new(FixedSession(Some(Ok("next utterance".into())))))
                }
            }
        }
        let script = [
            (false, 600),
            (true, 800),
            (false, 1200),
            (true, 800),
            (false, 1200),
        ];
        let expected = listen(ticks(JP, 0, &script));
        let directory = tempfile::tempdir().unwrap();
        let (sender, queue) = mpsc::channel();
        let worker = intake::Worker::from_sender(sender);
        let intake = worker.inbox().unwrap();
        let (heard, moments) = mpsc::channel();
        for moment in ticks(JP, 0, &script) {
            heard.send(moment).unwrap();
        }
        drop(heard);
        hear(
            moments,
            || Ok(Box::new(FailsFirst(std::cell::Cell::new(false)))),
            7,
            &intake,
            directory.path(),
        );
        let path = directory
            .path()
            .join(format!("7-{JP}-{}.wav", expected[0].start_ms));
        assert_eq!(
            std::fs::read(path).unwrap(),
            wav_pcm16(&expected[0].samples)
        );
        let Ok(Work::Utterance(item)) = queue.try_recv() else {
            panic!("second utterance missing")
        };
        assert_eq!(item.transcript, "next utterance");
        assert_eq!(item.wav, wav_pcm16(&expected[1].samples));
        assert!(queue.try_recv().is_err());
    }

    // The actual owner loop must process PCM while the sender is still speaking,
    // but it must not publish an utterance until the unchanged VAD closes it.
    #[test]
    fn owner_pushes_before_finalization_without_partial_intake() {
        struct Online(mpsc::Sender<()>);
        struct OnlineSession(mpsc::Sender<()>);
        impl Transcribe for Online {
            fn listen(&self) -> Result<Box<dyn Transcription + '_>> {
                Ok(Box::new(OnlineSession(self.0.clone())))
            }
        }
        impl Transcription for OnlineSession {
            fn push(&mut self, _: &[f32]) -> Result<()> {
                let _ = self.0.send(());
                Ok(())
            }
            fn finish(self: Box<Self>) -> Result<String> {
                Ok("complete words".into())
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let (sender, queue) = mpsc::channel();
        let worker = intake::Worker::from_sender(sender);
        let intake = worker.inbox().unwrap();
        let (heard, moments) = mpsc::channel();
        let (pushed, progress) = mpsc::channel();
        let feeder = std::thread::spawn(move || {
            for moment in ticks(JP, 0, &[(false, 600), (true, 1200)]) {
                heard.send(moment).unwrap();
            }
            let online = progress
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok();
            let partial = queue.try_recv().is_ok();
            for moment in ticks(JP, 1800, &[(false, 1200)]) {
                heard.send(moment).unwrap();
            }
            drop(heard);
            (online, partial, queue)
        });
        hear(
            moments,
            || Ok(Box::new(Online(pushed))),
            7,
            &intake,
            directory.path(),
        );
        let (online, partial, queue) = feeder.join().unwrap();
        assert!(online, "no PCM reached recognition before VAD finalization");
        assert!(!partial, "partial transcript escaped to intake");
        let Ok(Work::Utterance(utterance)) = queue.try_recv() else {
            panic!("final intake missing")
        };
        assert_eq!(utterance.transcript, "complete words");
        assert!(queue.try_recv().is_err());
    }

    /// `ms` of a 220 Hz tone, loud enough to be speech, as ticks of samples.
    fn tone(ms: usize) -> Vec<Vec<i16>> {
        let samples: Vec<i16> = (0..RATE * ms / 1000)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                ((t * 220.0 * std::f32::consts::TAU).sin() * 0.3 * 32767.0) as i16
            })
            .collect();
        samples.chunks(TICK).map(<[i16]>::to_vec).collect()
    }

    /// A stream of ticks for one user from `at_ms`, one every 20 ms: silence
    /// for `quiet` ms, then speech for `loud` ms, and so on.
    fn ticks(user: u64, at_ms: u64, script: &[(bool, usize)]) -> Vec<Moment> {
        let mut moments = Vec::new();
        let mut at_ms = at_ms;
        for &(loud, ms) in script {
            let heard: Vec<Heard> = if loud {
                tone(ms)
                    .into_iter()
                    .map(|samples| Heard::Audio { user, samples })
                    .collect()
            } else {
                (0..ms / 20).map(|_| Heard::Silence { user }).collect()
            };
            for heard in heard {
                moments.push(Moment {
                    at_ms,
                    heard: vec![heard],
                });
                at_ms += 20;
            }
        }
        moments
    }

    fn listen(moments: Vec<Moment>) -> Vec<Spoken> {
        let mut listener = Listener::default();
        let mut spoken = Vec::new();
        for moment in moments {
            listener.hear(moment, &mut |s| spoken.push(s));
        }
        listener.flush(&mut |s| spoken.push(s));
        spoken
    }

    /// Everybody Discord names is heard, each as themselves, and the bot
    /// never: its SSRC is forgotten once the driver says it is the bot's, and
    /// so is the new one after a reconnect. An SSRC Discord names nobody for
    /// is never even looked up.
    #[test]
    fn everybody_but_the_bot_is_heard_each_as_themselves() {
        let mut ears = Ears::default();
        ears.speaking(9, Some(BOT));
        ears.connected(9);
        ears.speaking(1, Some(JP));
        ears.speaking(2, Some(OTHER));
        ears.speaking(3, None);
        let loud = tone(20).remove(0);
        let asked = std::cell::RefCell::new(BTreeSet::new());
        let talking = std::cell::Cell::new(false);
        let sent = |ssrc: u32| {
            asked.borrow_mut().insert(ssrc);
            if talking.get() {
                Sent::Audio(loud.as_slice())
            } else {
                Sent::Silent
            }
        };

        // Everybody is quiet for 600 ms, then talks for 1.4 s.
        let mut listener = Listener::default();
        let mut spoken = Vec::new();
        for i in 0..100 {
            talking.set(i >= 30);
            let heard = ears.tick(&sent);
            let of = |user| {
                if talking.get() {
                    Heard::Audio {
                        user,
                        samples: loud.clone(),
                    }
                } else {
                    Heard::Silence { user }
                }
            };
            assert_eq!(heard.len(), 2);
            assert!(heard.contains(&of(JP)) && heard.contains(&of(OTHER)));
            listener.hear(
                Moment {
                    at_ms: 20 * i,
                    heard,
                },
                &mut |s| spoken.push(s),
            );
        }
        assert_eq!(*asked.borrow(), BTreeSet::from([1, 2]));
        listener.flush(&mut |s| spoken.push(s));
        let users: BTreeSet<u64> = spoken.iter().map(|s| s.user).collect();
        assert_eq!(spoken.len(), 2, "one utterance each");
        assert_eq!(users, BTreeSet::from([JP, OTHER]));

        // The driver reconnects with another SSRC, which Discord then names
        // as the bot's: it is not heard either.
        ears.connected(5);
        ears.speaking(5, Some(BOT));
        asked.borrow_mut().clear();
        assert_eq!(ears.tick(&sent).len(), 2);
        assert_eq!(*asked.borrow(), BTreeSet::from([1, 2]));

        // Discord gives JP's old SSRC to OTHER, who has one SSRC at a time.
        ears.speaking(1, Some(OTHER));
        talking.set(false);
        assert_eq!(ears.tick(&sent), [Heard::Silence { user: OTHER }]);

        // JP comes back on a new SSRC; leaving forgets it.
        ears.speaking(4, Some(JP));
        assert_eq!(ears.tick(&sent).len(), 2);
        assert!(ears.left(JP));
        assert_eq!(ears.tick(&sent), [Heard::Silence { user: OTHER }]);
        assert!(!ears.left(BOT), "the bot was never heard");
    }

    /// Discord hands over what the transcriber hears: mono at [`RATE`],
    /// Voxtral's rate.
    #[test]
    fn songbird_decodes_to_what_voxtral_hears() {
        let songbird::driver::DecodeMode::Decode(decode) = decode_mode() else {
            panic!("hearing decodes");
        };
        assert_eq!(decode.channels, songbird::driver::Channels::Mono);
        assert_eq!(u32::from(decode.sample_rate) as usize, RATE);
    }

    /// A tick stream as songbird hands it over: silence as silent ticks
    /// (Discord clients stop sending), speech as audio. Two bursts are two
    /// utterances, timed from the stream's first tick.
    #[test]
    fn a_tick_stream_segments_into_utterances() {
        let origin = 1_790_000_000_000;
        let spoken = listen(ticks(
            JP,
            origin,
            &[
                (false, 1000),
                (true, 1200),
                (false, 1200),
                (true, 1000),
                (false, 1200),
            ],
        ));
        assert_eq!(spoken.len(), 2, "one utterance per burst");
        // Each begins at its burst, less the segmenter's 240 ms pre-roll.
        for (spoken, burst_ms) in spoken.iter().zip([1000, 3400]) {
            let start = spoken.start_ms - origin;
            assert!(
                (burst_ms - 300..=burst_ms).contains(&start),
                "began at {start} ms, the burst at {burst_ms} ms"
            );
            assert!(spoken.samples.len() > RATE / 2, "{}", spoken.samples.len());
        }
    }

    /// A stream that stops coming without silence (its SSRC dropped) ends
    /// what was being said when it comes back, or when the user leaves.
    #[test]
    fn a_stalled_or_departed_stream_finishes_its_utterance() {
        let origin = 1_790_000_000_000;
        let mut moments = ticks(JP, origin, &[(false, 600), (true, 800)]);
        // Nothing for five seconds, then silence again.
        moments.extend(ticks(JP, origin + 6_400, &[(false, 100)]));
        let spoken = listen(moments);
        assert_eq!(spoken.len(), 1);
        assert!(spoken[0].start_ms < origin + 600);

        let mut listener = Listener::default();
        let mut heard = Vec::new();
        for moment in ticks(JP, origin, &[(false, 600), (true, 800)]) {
            listener.hear(moment, &mut |s| heard.push(s));
        }
        assert!(heard.is_empty(), "still speaking");
        listener.hear(
            Moment {
                at_ms: origin + 1_400,
                heard: vec![Heard::Left { user: JP }],
            },
            &mut |s| heard.push(s),
        );
        assert_eq!(heard.len(), 1, "leaving finishes it");
        assert!(listener.streams.is_empty());
    }

    /// The audio is kept as 16 kHz mono 16-bit PCM, exactly as heard.
    #[test]
    fn the_audio_is_kept_exactly() {
        let heard: Vec<i16> = vec![0, 1, -1, 32767, -32768, 1234, -4321];
        let samples: Vec<f32> = heard.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let wav = wav_pcm16(&samples);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        let back: Vec<i16> = wav[44..]
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(back, heard);
    }

    struct Fixed(std::cell::RefCell<Vec<Result<String>>>);
    struct FixedSession(Option<Result<String>>);
    impl Transcription for FixedSession {
        fn push(&mut self, _: &[f32]) -> Result<()> {
            Ok(())
        }
        fn finish(mut self: Box<Self>) -> Result<String> {
            self.0.take().unwrap()
        }
    }

    impl Transcribe for Fixed {
        fn listen(&self) -> Result<Box<dyn Transcription + '_>> {
            Ok(Box::new(FixedSession(Some(self.0.borrow_mut().remove(0)))))
        }
    }

    /// What is heard goes to intake as utterances; with intake gone, and
    /// when it cannot be transcribed, it is kept; with no words, dropped.
    #[test]
    fn utterances_go_to_intake_or_are_kept() {
        let directory = tempfile::tempdir().unwrap();
        let unstored = directory.path().join(intake::UNSTORED_SPEECH);
        let (sender, queue) = std::sync::mpsc::channel();
        let worker = intake::Worker::from_sender(sender);
        let intake = worker.inbox().unwrap();
        let (heard, moments) = mpsc::channel();
        for moment in ticks(
            JP,
            1_790_000_000_000,
            &[(false, 600), (true, 800), (false, 1200)],
        ) {
            heard.send(moment).unwrap();
        }
        drop(heard);
        let load = || -> Result<Box<dyn Transcribe>> {
            Ok(Box::new(Fixed(std::cell::RefCell::new(vec![Ok(
                " hello there \n".to_owned(),
            )]))))
        };
        hear(moments, load, 7, &intake, &unstored);
        let Ok(Work::Utterance(utterance)) = queue.try_recv() else {
            panic!("the utterance goes to intake");
        };
        assert_eq!((utterance.channel, utterance.user), (7, JP));
        assert_eq!(utterance.transcript, "hello there");
        assert_eq!(&utterance.wav[..4], b"RIFF");
        assert!(!unstored.exists());

        let spoken = || Spoken {
            user: JP,
            start_ms: 5,
            samples: vec![0.25; 800],
        };
        let mut ear = Fixed(std::cell::RefCell::new(vec![
            Ok("  ".to_owned()),
            Err(anyhow::anyhow!("no")),
            Ok("kept".to_owned()),
        ]));
        store(&mut ear, spoken(), 7, &intake, &unstored);
        assert!(queue.try_recv().is_err(), "no words, nothing stored");
        assert!(!unstored.exists());
        store(&mut ear, spoken(), 7, &intake, &unstored);
        let name = format!("7-{JP}-5");
        let wav = std::fs::read(unstored.join(format!("{name}.wav"))).unwrap();
        assert_eq!(wav, wav_pcm16(&vec![0.25; 800]), "its audio is kept");
        drop((worker, queue));
        store(&mut ear, spoken(), 7, &intake, &unstored);
        assert_eq!(
            std::fs::read_to_string(unstored.join(format!("{name}.txt"))).unwrap(),
            "kept",
            "intake gone, the utterance is kept"
        );
    }
}
