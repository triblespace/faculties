//! Discord voice messages: the ogg/opus attachments the client records, heard
//! by the transcriber that hears the voice channel.
//!
//! Intake stores a voice message like any other message, its audio as an
//! attachment ([`super::intake`]). With hearing on, and its author among
//! those `live --hear-voice-messages-from` names, it then takes the
//! attachment's bytes back out of the pile, decodes them here to mono at
//! [`RATE`] and hands them to the hearing thread as a [`VoiceMessage`]; that
//! thread transcribes them with the transcriber already loaded for the voice
//! channel, and the transcript is stored as an utterance of the message's
//! author in the message's channel, at the moment the message was sent,
//! linked to the stored recording ([`super::hearing`]). Only a message
//! Discord flags as a voice message is heard ([`recording`]), and a file that
//! would decode to more than [`MAX_SECONDS`] is refused.
//!
//! The decoder is libopus, through the `opus2` crate that songbird already
//! links to decode the voice channel (`songbird::driver::opus`), asked for
//! mono at [`RATE`] directly, so no resampler runs. The ogg container is
//! read here: pages with their CRC checked, the packets of the first
//! logical stream, its `OpusHead` (channel count, pre-skip, output gain), and
//! the end trimmed to the last page's granule position.
//!
//! Built with the `discord-hearing` feature.

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::Value;
use songbird::driver::opus;

/// The rate the transcriber hears: the rate the decoder is asked for.
pub const RATE: usize = super::hearing::RATE;

/// Discord's message flag for a voice message (`IS_VOICE_MESSAGE`).
const IS_VOICE_MESSAGE: u64 = 1 << 13;

/// The rate Opus granule positions and pre-skip count in.
const GRANULE_RATE: usize = 48_000;

/// The longest recording heard, in seconds: a file that decodes to more is
/// refused before it is decoded past this.
pub const MAX_SECONDS: usize = 20 * 60;

/// The most packets a stream of [`MAX_SECONDS`] can hold (2.5 ms is the
/// shortest an opus packet lasts), with its two headers: an ogg file with
/// more is refused while it is read, before its packets are all held.
const MAX_PACKETS: usize = MAX_SECONDS * 400 + 2;

/// A voice message to transcribe: its channel, its author, when it was sent,
/// the stored attachment it was recorded in, and its audio, mono at
/// [`RATE`]. Debug names the sizes, never the audio.
pub struct VoiceMessage {
    pub channel: u64,
    pub user: u64,
    /// When it was sent, in milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// The attachment entity the pile holds the recording as.
    pub attachment: triblespace::prelude::Id,
    pub samples: Vec<f32>,
}

impl std::fmt::Debug for VoiceMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceMessage")
            .field("channel", &self.channel)
            .field("user", &self.user)
            .field("start_ms", &self.start_ms)
            .field("attachment", &self.attachment)
            .field("seconds", &(self.samples.len() as f64 / RATE as f64))
            .finish()
    }
}

/// An attachment of a message that is a voice recording, as intake finds it
/// again in the pile: Discord's attachment id and the file name.
#[derive(Debug, PartialEq, Eq)]
pub struct Recording {
    pub source_id: String,
    pub filename: String,
}

/// The recording of a voice message: the first audio attachment of a
/// message Discord flags as a voice message (`IS_VOICE_MESSAGE`). Only such a
/// message is the speech of its author, recorded in the client; an audio file
/// posted as an attachment may hold anybody's, so it is not heard. Discord
/// allows a voice message one audio attachment and nothing else; were one to
/// carry more, only the first is heard, so that one message is one utterance
/// at the moment it was sent.
pub fn recording(message: &Value) -> Option<Recording> {
    if message["flags"].as_u64().unwrap_or(0) & IS_VOICE_MESSAGE == 0 {
        return None;
    }
    message["attachments"]
        .as_array()?
        .iter()
        .filter(|attachment| {
            let kind = attachment["content_type"].as_str().unwrap_or("");
            kind.starts_with("audio/")
        })
        .find_map(|attachment| {
            Some(Recording {
                source_id: attachment["id"].as_str()?.to_owned(),
                filename: attachment["filename"].as_str()?.to_owned(),
            })
        })
}

/// An ogg/opus file decoded to mono at [`RATE`]: pre-skip removed, the
/// header's output gain applied, and the end trimmed to the last granule
/// position. An empty packet is refused (libopus would conceal a lost one
/// out of it), and so is a file whose packets last longer than
/// [`MAX_SECONDS`]: each packet's length is read from its header before it is
/// decoded, so no more than that is ever decoded.
pub fn decode_ogg_opus(bytes: &[u8]) -> Result<Vec<f32>> {
    let stream = ogg_stream(bytes)?;
    let mut packets = stream.packets.into_iter();
    let head = packets.next().context("the ogg stream has no packets")?;
    let head = OpusHead::parse(&head)?;
    let tags = packets
        .next()
        .context("the opus stream has no comment header")?;
    ensure!(
        tags.starts_with(b"OpusTags"),
        "the second opus packet is not OpusTags"
    );
    let mut decoder = opus::Decoder::new(RATE as u32, opus::Channels::Mono)
        .map_err(|error| anyhow!("create the opus decoder: {error}"))?;
    // 120 ms, the longest an opus packet decodes to.
    let mut frame = vec![0.0f32; RATE * 120 / 1000];
    let mut samples = Vec::new();
    let per_granule = GRANULE_RATE / RATE;
    let longest = MAX_SECONDS * RATE + head.pre_skip as usize / per_granule;
    for (index, packet) in packets.enumerate() {
        ensure!(!packet.is_empty(), "opus packet {index} is empty");
        let length = decoder
            .get_nb_samples(&packet)
            .map_err(|error| anyhow!("read the length of opus packet {index}: {error}"))?;
        ensure!(
            samples.len() + length <= longest,
            "the recording is longer than {} minutes, the longest heard",
            MAX_SECONDS / 60
        );
        let decoded = decoder
            .decode_float(&packet, &mut frame, false)
            .map_err(|error| anyhow!("decode an opus packet: {error}"))?;
        samples.extend_from_slice(&frame[..decoded]);
    }
    let skip = (head.pre_skip as usize / per_granule).min(samples.len());
    samples.drain(..skip);
    if let Some(end) = stream.last_granule {
        let length = (end.saturating_sub(head.pre_skip as u64) as usize).div_ceil(per_granule);
        samples.truncate(length);
    }
    if head.gain != 0 {
        let gain = 10f32.powf(head.gain as f32 / (20.0 * 256.0));
        samples.iter_mut().for_each(|sample| *sample *= gain);
    }
    Ok(samples)
}

/// What an `OpusHead` packet says that decoding needs.
struct OpusHead {
    pre_skip: u16,
    /// Q7.8 dB.
    gain: i16,
}

impl OpusHead {
    fn parse(packet: &[u8]) -> Result<Self> {
        ensure!(
            packet.len() >= 19 && packet.starts_with(b"OpusHead"),
            "the first ogg packet is not an OpusHead"
        );
        ensure!(
            packet[8] & 0xF0 == 0,
            "unsupported OpusHead version {}",
            packet[8]
        );
        let channels = packet[9];
        let family = packet[18];
        ensure!(
            family == 0 && (1..=2).contains(&channels),
            "only mono and stereo opus are heard (mapping family {family}, {channels} channels)"
        );
        Ok(Self {
            pre_skip: u16::from_le_bytes([packet[10], packet[11]]),
            gain: i16::from_le_bytes([packet[16], packet[17]]),
        })
    }
}

/// The packets of an ogg file's first logical stream, and the granule
/// position of its last page that has one.
struct OggStream {
    packets: Vec<Vec<u8>>,
    last_granule: Option<u64>,
}

fn ogg_stream(bytes: &[u8]) -> Result<OggStream> {
    let mut at = 0;
    let mut serial = None;
    let mut packets = Vec::new();
    let mut partial: Vec<u8> = Vec::new();
    let mut last_granule = None;
    while at < bytes.len() {
        let page = &bytes[at..];
        ensure!(
            page.len() >= 27 && page.starts_with(b"OggS"),
            "no ogg page at byte {at}"
        );
        ensure!(
            page[4] == 0,
            "unsupported ogg version {} at byte {at}",
            page[4]
        );
        let continued = page[5] & 0x01 != 0;
        let granule = u64::from_le_bytes(page[6..14].try_into().unwrap());
        let page_serial = u32::from_le_bytes(page[14..18].try_into().unwrap());
        let crc = u32::from_le_bytes(page[22..26].try_into().unwrap());
        let segments = page[26] as usize;
        ensure!(
            page.len() >= 27 + segments,
            "ogg page at byte {at} is cut short"
        );
        let lacing = &page[27..27 + segments];
        let body_length: usize = lacing.iter().map(|&l| l as usize).sum();
        let length = 27 + segments + body_length;
        ensure!(page.len() >= length, "ogg page at byte {at} is cut short");
        let page = &page[..length];
        ensure!(
            ogg_crc(page) == crc,
            "ogg page at byte {at} fails its checksum"
        );
        at += length;
        if *serial.get_or_insert(page_serial) != page_serial {
            continue;
        }
        if !continued && !partial.is_empty() {
            bail!(
                "an ogg packet was left unfinished before byte {}",
                at - length
            );
        }
        let mut body = &page[27 + segments..];
        for &segment in lacing {
            let (now, rest) = body.split_at(segment as usize);
            partial.extend_from_slice(now);
            body = rest;
            if segment < 255 {
                ensure!(
                    packets.len() < MAX_PACKETS,
                    "the recording is longer than {} minutes, the longest heard: it holds \
                     more opus packets than that can",
                    MAX_SECONDS / 60
                );
                packets.push(std::mem::take(&mut partial));
            }
        }
        // -1 marks a page on which no packet ends.
        if granule != u64::MAX {
            last_granule = Some(granule);
        }
    }
    ensure!(serial.is_some(), "no ogg pages");
    Ok(OggStream {
        packets,
        last_granule,
    })
}

/// The ogg page checksum: CRC-32, polynomial 0x04C11DB7, unreflected, zero
/// initial value, over the page with its checksum field taken as zero.
fn ogg_crc(page: &[u8]) -> u32 {
    let mut crc = 0u32;
    for (index, &byte) in page.iter().enumerate() {
        let byte = if (22..26).contains(&index) { 0 } else { byte };
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// When a Discord message was sent, from its id, in milliseconds since the
/// Unix epoch.
pub fn sent_ms(message_id: u64) -> u64 {
    (message_id >> 22) + crate::discord::DISCORD_EPOCH_MS
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// An ogg/opus file of `seconds` of a `hertz` sine at 48 kHz mono, encoded
    /// by libopus in 20 ms packets, `per_page` packets to a page, with
    /// `pre_skip` and the last page's granule position trimming the end to
    /// the exact length.
    pub(crate) fn ogg_opus_sine(seconds: f32, hertz: f32, per_page: usize) -> Vec<u8> {
        let pre_skip: u16 = 312;
        let total = (seconds * GRANULE_RATE as f32) as usize;
        let signal: Vec<f32> = (0..total)
            .map(|i| (i as f32 * hertz * std::f32::consts::TAU / GRANULE_RATE as f32).sin() * 0.5)
            .collect();
        let mut encoder = opus::Encoder::new(
            GRANULE_RATE as u32,
            opus::Channels::Mono,
            opus::Application::Audio,
        )
        .unwrap();
        // The encoder's delay is what pre-skip removes: feed it silence first
        // so the sine starts where the decoder starts keeping samples.
        let frame = GRANULE_RATE / 50;
        let mut input = vec![0.0f32; pre_skip as usize];
        input.extend_from_slice(&signal);
        input.resize(input.len().div_ceil(frame) * frame, 0.0);
        let mut packets = Vec::new();
        for chunk in input.chunks(frame) {
            let mut out = vec![0u8; 4000];
            let length = encoder.encode_float(chunk, &mut out).unwrap();
            out.truncate(length);
            packets.push(out);
        }
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(1);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&4u32.to_le_bytes());
        tags.extend_from_slice(b"test");
        tags.extend_from_slice(&0u32.to_le_bytes());

        let mut file = Vec::new();
        let mut sequence = 0;
        file.extend(ogg_page(0x02, 0, sequence, &[head]));
        sequence += 1;
        file.extend(ogg_page(0, 0, sequence, &[tags]));
        let pages: Vec<&[Vec<u8>]> = packets.chunks(per_page).collect();
        let mut granule = 0u64;
        for (index, page) in pages.iter().enumerate() {
            sequence += 1;
            granule += (page.len() * frame) as u64;
            let last = index + 1 == pages.len();
            let end = if last {
                (pre_skip as usize + total) as u64
            } else {
                granule
            };
            file.extend(ogg_page(if last { 0x04 } else { 0 }, end, sequence, page));
        }
        file
    }

    /// An ogg/opus file of mono `packets` as they are, up to 255 to a page,
    /// with no pre-skip, each page's granule position counting `granules`
    /// (at 48 kHz) a packet.
    fn ogg_opus_packets(packets: &[Vec<u8>], granules: u64) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 1]);
        head.extend_from_slice(&0u16.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&0u32.to_le_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes());
        let mut file = ogg_page(0x02, 0, 0, &[head]);
        file.extend(ogg_page(0, 0, 1, &[tags]));
        let mut granule = 0;
        for (sequence, page) in (2..).zip(packets.chunks(255)) {
            granule += page.len() as u64 * granules;
            file.extend(ogg_page(0, granule, sequence, page));
        }
        file
    }

    fn ogg_page(kind: u8, granule: u64, sequence: u32, packets: &[Vec<u8>]) -> Vec<u8> {
        let mut lacing = Vec::new();
        for packet in packets {
            lacing.extend(std::iter::repeat_n(255u8, packet.len() / 255));
            lacing.push((packet.len() % 255) as u8);
        }
        let mut page = b"OggS".to_vec();
        page.push(0);
        page.push(kind);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&0x5EED_u32.to_le_bytes());
        page.extend_from_slice(&sequence.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.push(lacing.len() as u8);
        page.extend_from_slice(&lacing);
        for packet in packets {
            page.extend_from_slice(packet);
        }
        let crc = ogg_crc(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        page
    }

    /// The dominant frequency of `samples` at [`RATE`] among `candidates`, by
    /// correlation.
    fn loudest(samples: &[f32], candidates: &[f32]) -> f32 {
        let power = |hertz: f32| {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (i, &s) in samples.iter().enumerate() {
                let phase = i as f64 * hertz as f64 * std::f64::consts::TAU / RATE as f64;
                re += s as f64 * phase.cos();
                im += s as f64 * phase.sin();
            }
            re * re + im * im
        };
        *candidates
            .iter()
            .max_by(|a, b| power(**a).total_cmp(&power(**b)))
            .unwrap()
    }

    /// A 3 s sine comes out at the hearing rate, exactly 3 s long, the same
    /// tone at about the same level, however its packets are paged.
    #[test]
    fn an_ogg_opus_file_decodes_to_mono_at_the_hearing_rate() {
        for per_page in [1, 7, 50] {
            let file = ogg_opus_sine(3.0, 440.0, per_page);
            let samples = decode_ogg_opus(&file).unwrap();
            assert_eq!(samples.len(), 3 * RATE, "{per_page} packets per page");
            assert_eq!(
                loudest(&samples[RATE..2 * RATE], &[220.0, 440.0, 880.0]),
                440.0
            );
            let rms =
                (samples[RATE..2 * RATE].iter().map(|s| s * s).sum::<f32>() / RATE as f32).sqrt();
            assert!(
                (0.3..0.4).contains(&rms),
                "rms {rms}, a 0.5 sine's is 0.354"
            );
        }
    }

    /// A damaged page is refused, never decoded into noise; so is a file
    /// that is not ogg at all.
    #[test]
    fn a_damaged_ogg_file_is_refused() {
        let mut file = ogg_opus_sine(1.0, 440.0, 10);
        let middle = file.len() / 2;
        file[middle] ^= 0x55;
        assert!(decode_ogg_opus(&file).is_err());
        assert!(decode_ogg_opus(b"RIFF....WAVEfmt ").is_err());
        assert!(decode_ogg_opus(&[]).is_err());
    }

    /// An empty packet decodes to 120 ms of packet-loss concealment out of
    /// nothing; it is refused, not decoded.
    #[test]
    fn a_file_of_empty_packets_is_refused() {
        let file = ogg_opus_packets(&vec![Vec::new(); 1000], 5760);
        match decode_ogg_opus(&file) {
            Ok(samples) => panic!(
                "decoded {:.1} s out of {} bytes of empty packets",
                samples.len() as f64 / RATE as f64,
                file.len()
            ),
            Err(error) => assert!(format!("{error:#}").contains("empty"), "{error:#}"),
        }
    }

    /// One-byte packets (TOC 0x18: SILK narrowband, one 60 ms frame of no
    /// bytes) each decode to 60 ms of concealment, so a small file could
    /// decode to hours: one that would run past [`MAX_SECONDS`] is refused,
    /// counted by its packets' durations before they are decoded.
    #[test]
    fn a_file_longer_than_the_longest_heard_is_refused_before_it_is_decoded() {
        let packets = vec![vec![0x18u8]; MAX_SECONDS * 1000 / 60 + 100];
        let file = ogg_opus_packets(&packets, 2880);
        let started = std::time::Instant::now();
        match decode_ogg_opus(&file) {
            Ok(samples) => panic!(
                "decoded {:.1} s out of {} bytes",
                samples.len() as f64 / RATE as f64,
                file.len()
            ),
            Err(error) => assert!(format!("{error:#}").contains("longer than"), "{error:#}"),
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // Just under the limit is heard.
        let packets = vec![vec![0x18u8]; MAX_SECONDS * 1000 / 60 - 10];
        let samples = decode_ogg_opus(&ogg_opus_packets(&packets, 2880)).unwrap();
        assert_eq!(samples.len(), packets.len() * RATE * 60 / 1000);
        assert!(samples.len() <= MAX_SECONDS * RATE);
    }

    /// The real thing, when a recorded voice message is at hand
    /// (`FACULTIES_VOICE_MESSAGE_OGG`, with the 16 kHz WAV another decoder
    /// made of it in `FACULTIES_VOICE_MESSAGE_WAV`): decoded to the same
    /// length within a packet, and the same signal.
    #[test]
    fn a_recorded_voice_message_decodes_like_another_decoder_does() {
        let (Ok(ogg), Ok(wav)) = (
            std::env::var("FACULTIES_VOICE_MESSAGE_OGG"),
            std::env::var("FACULTIES_VOICE_MESSAGE_WAV"),
        ) else {
            eprintln!("no recorded voice message given; skipped");
            return;
        };
        let samples = decode_ogg_opus(&std::fs::read(ogg).unwrap()).unwrap();
        let (other, rate) = mary::models::f5::wav::read_pcm16_mono(std::path::Path::new(&wav));
        assert_eq!(rate as usize, RATE);
        assert!(
            samples.len().abs_diff(other.len()) <= RATE / 50,
            "{} samples against {}",
            samples.len(),
            other.len()
        );
        // Best normalized correlation within a few milliseconds of lag: the
        // two resample differently, so they agree in shape, not bit for bit.
        let n = samples.len().min(other.len()) - 200;
        let correlation = (0..100)
            .map(|lag| {
                let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
                for i in 100..n {
                    let a = samples[i] as f64;
                    let b = other[i + lag - 50] as f64;
                    ab += a * b;
                    aa += a * a;
                    bb += b * b;
                }
                ab / (aa * bb).sqrt()
            })
            .fold(f64::MIN, f64::max);
        eprintln!(
            "{:.2} s decoded; correlation with the other decoder {correlation:.4}",
            samples.len() as f64 / RATE as f64
        );
        assert!(correlation > 0.95, "correlation {correlation}");
    }

    /// Discord's voice message (flagged, `audio/ogg`) is a recording; an ogg
    /// file posted as an attachment without the flag is not (whose speech it
    /// holds nobody can say), nor a picture, nor an mp3.
    #[test]
    fn only_a_flagged_voice_message_has_a_recording() {
        let voice = json!({
            "id": "1", "flags": 8192,
            "attachments": [{
                "id": "11", "filename": "voice-message.ogg",
                "content_type": "audio/ogg", "duration_secs": 66.0
            }]
        });
        assert_eq!(
            recording(&voice),
            Some(Recording {
                source_id: "11".to_owned(),
                filename: "voice-message.ogg".to_owned()
            })
        );
        let posted = json!({
            "id": "2",
            "attachments": [
                {"id": "21", "filename": "a.png", "content_type": "image/png"},
                {"id": "22", "filename": "b.mp3", "content_type": "audio/mpeg"},
                {"id": "23", "filename": "c.ogg", "content_type": "audio/ogg; codecs=opus"},
            ]
        });
        assert!(
            recording(&posted).is_none(),
            "an ogg posted without the flag"
        );
        assert!(recording(&json!({"id": "3", "content": "hi"})).is_none());
    }

    #[test]
    fn a_message_id_says_when_it_was_sent() {
        // Discord's documented example: 175928847299117063 is 2016-04-30
        // 11:18:25.796 UTC.
        assert_eq!(sent_ms(175928847299117063), 1_462_015_105_796);
    }
}
