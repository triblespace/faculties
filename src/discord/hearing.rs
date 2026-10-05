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
}

impl Stream {
    fn new(origin_ms: u64) -> Self {
        Self {
            segmenter: Segmenter::new(RATE, VadConfig::default()),
            origin_ms,
            fed: 0,
        }
    }

    /// Where the stream's clock stands.
    fn now_ms(&self) -> u64 {
        self.origin_ms + self.fed * 1000 / RATE as u64
    }

    fn push(&mut self, user: u64, samples: &[f32], emit: &mut impl FnMut(Spoken)) {
        let origin_ms = self.origin_ms;
        self.segmenter.push(samples, &mut |segment| {
            emit(spoken(user, origin_ms, segment))
        });
        self.fed += samples.len() as u64;
    }

    /// The stream ends: what was being said is finished.
    fn close(mut self, user: u64, emit: &mut impl FnMut(Spoken)) {
        let origin_ms = self.origin_ms;
        self.segmenter
            .flush(&mut |segment| emit(spoken(user, origin_ms, segment)));
    }
}

fn spoken(user: u64, origin_ms: u64, segment: Segment) -> Spoken {
    Spoken {
        user,
        start_ms: origin_ms + (segment.start_s * 1000.0).round() as u64,
        samples: segment.samples,
    }
}

/// The heard users' streams, cut into utterances by the energy segmenter
/// `hear` uses. Only what [`Ears`] let through ever reaches it.
#[derive(Default)]
pub struct Listener {
    streams: HashMap<u64, Stream>,
}

impl Listener {
    pub fn hear(&mut self, moment: Moment, emit: &mut impl FnMut(Spoken)) {
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

    fn feed(&mut self, user: u64, at_ms: u64, samples: &[f32], emit: &mut impl FnMut(Spoken)) {
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
        for (user, stream) in self.streams.drain() {
            stream.close(user, emit);
        }
    }
}

/// What turns one utterance, mono samples at [`RATE`], into text. The model
/// and its backend sit behind it, so the backend can change without touching
/// what calls it.
pub trait Transcribe {
    fn transcribe(&mut self, samples: &[f32]) -> Result<String>;
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
    let mut ear = match load() {
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
    let mut listener = Listener::default();
    let mut finished = Vec::new();
    for moment in moments.iter() {
        listener.hear(moment, &mut |spoken| finished.push(spoken));
        for spoken in finished.drain(..) {
            store(&mut *ear, spoken, channel, intake, unstored);
        }
    }
    listener.flush(&mut |spoken| finished.push(spoken));
    for spoken in finished {
        store(&mut *ear, spoken, channel, intake, unstored);
    }
}

/// Transcribe one utterance and hand it to intake; keep it in `unstored`
/// when intake is gone, and keep its audio there when it cannot be
/// transcribed. One with no words in it is not stored.
fn store(ear: &mut dyn Transcribe, spoken: Spoken, channel: u64, intake: &Inbox, unstored: &Path) {
    let seconds = spoken.samples.len() as f64 / RATE as f64;
    let started = Instant::now();
    let transcribed = ear.transcribe(&spoken.samples);
    let took = started.elapsed().as_secs_f64();
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
    use super::Transcribe;
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

    impl Transcribe for Voxtral {
        /// One stream per utterance; what does not fit in one stream's
        /// positions (about ten minutes) goes on in the next.
        fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
            let mut text = String::new();
            let mut rest = samples;
            loop {
                let mut listening = self.ears.listen(DELAY_MS);
                let (now, later) = rest.split_at(rest.len().min(listening.room()));
                text += &listening.push(now);
                text += &listening.finish();
                if later.is_empty() {
                    return Ok(text);
                }
                rest = later;
            }
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

    struct Fixed(Vec<Result<String>>);

    impl Transcribe for Fixed {
        fn transcribe(&mut self, _: &[f32]) -> Result<String> {
            self.0.remove(0)
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
            &[(false, 600), (true, 800), (false, 1000)],
        ) {
            heard.send(moment).unwrap();
        }
        drop(heard);
        let load = || -> Result<Box<dyn Transcribe>> {
            Ok(Box::new(Fixed(vec![Ok(" hello there \n".to_owned())])))
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
        let mut ear = Fixed(vec![
            Ok("  ".to_owned()),
            Err(anyhow::anyhow!("no")),
            Ok("kept".to_owned()),
        ]);
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
