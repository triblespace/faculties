//! One speaker's framed PCM -> bounded utterances on a warm hearing backend.
//! No device, network, model loading, output file or Discord identity lives here.
//! Backpressure belongs to the pipe; a producer that drops audio must send GAP.

use std::io::Read;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use framed_stream::{EndStatus, Frame, FramedReader, Gap, UNIT_SAMPLES};

use super::operations::{hear_segment, now_ms};
use super::segmenter::{Segment, Segmenter, VadConfig};
use super::{Backend, Observation, Options};

/// Explicit little-endian format; unlike standard audio/L16, never network endian.
pub const PCM_F32LE_48K: &str = "audio/x-pcm;format=f32le;rate=48000;channels=1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    rate: usize,
    bytes_per_sample: usize,
}

impl PcmFormat {
    pub fn sample_rate(&self) -> usize {
        self.rate
    }

    pub fn parse(content_type: &str) -> Result<Self> {
        let mut parts = content_type.split(';').map(str::trim);
        ensure!(parts.next() == Some("audio/x-pcm"), "expected audio/x-pcm");
        let (mut format, mut rate, mut channels) = (None, None, None);
        for part in parts {
            let (name, value) = part.split_once('=').context("invalid PCM parameter")?;
            let slot = match name.trim() {
                "format" => &mut format,
                "rate" => &mut rate,
                "channels" => &mut channels,
                other => bail!("unsupported PCM parameter {other}"),
            };
            ensure!(
                slot.replace(value.trim()).is_none(),
                "duplicate PCM parameter {name}"
            );
        }
        ensure!(
            channels == Some("1"),
            "hearing stream requires one speaker, mono PCM"
        );
        let rate: usize = rate.context("PCM rate is required")?.parse()?;
        ensure!(
            matches!(rate, 16000 | 24000 | 48000),
            "unsupported PCM rate {rate}"
        );
        let bytes_per_sample = match format {
            Some("s16le") => 2,
            Some("f32le") => 4,
            _ => bail!("PCM format must be s16le or f32le"),
        };
        Ok(Self {
            rate,
            bytes_per_sample,
        })
    }

    fn decode(&self, payload: &[u8], extent: u64) -> Result<Vec<f32>> {
        ensure!(
            extent > 0 && extent <= self.rate as u64,
            "PCM record must contain at most one second, not {extent} samples"
        );
        ensure!(
            payload.len() == extent as usize * self.bytes_per_sample,
            "PCM extent does not match payload length"
        );
        let samples: Vec<f32> = if self.bytes_per_sample == 2 {
            payload
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                .collect()
        } else {
            payload
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect()
        };
        ensure!(
            samples
                .iter()
                .all(|x| x.is_finite() && (-1.0..=1.0).contains(x)),
            "PCM samples must be finite and normalized to [-1, 1]"
        );
        Ok(samples)
    }
}

#[derive(Debug)]
pub enum Event {
    Utterance {
        observation: Observation,
        processing_ms: u128,
    },
    /// An unfinished utterance is discarded, never stitched across missing audio.
    Gap(Gap),
    Complete,
}

/// Validate the transport header independently of loading a model.
pub fn open<R: Read>(input: R) -> Result<(FramedReader<R>, PcmFormat)> {
    let reader = FramedReader::open(input)?;
    ensure!(
        reader.unit() == UNIT_SAMPLES,
        "PCM stream unit must be samples"
    );
    let format = PcmFormat::parse(reader.content_type())?;
    Ok((reader, format))
}

/// Streaming is utterance-level inference, not token-level incremental ASR.
/// END{complete} flushes the tail; abort/truncation never flush partial speech.
pub fn process<R: Read>(
    mut reader: FramedReader<R>,
    backend: &mut impl Backend,
    source: &str,
    options: &Options,
    emit: &mut impl FnMut(Event) -> Result<()>,
) -> Result<()> {
    options.validate()?;
    ensure!(
        reader.unit() == UNIT_SAMPLES,
        "PCM stream unit must be samples"
    );
    let format = PcmFormat::parse(reader.content_type())?;
    let mut segmenter = Segmenter::new(format.rate, VadConfig::default());
    loop {
        let mut segments = Vec::new();
        match reader.next_frame()? {
            Frame::Record(record) => {
                ensure!(
                    PcmFormat::parse(record.content_type())? == format,
                    "PCM format changed mid-stream"
                );
                let samples = format.decode(&record.payload, record.extent)?;
                // Keep the segmenter's pending buffer small even for a large record.
                for chunk in samples.chunks(format.rate / 50) {
                    segmenter.push(chunk, &mut |segment| segments.push(segment));
                }
            }
            Frame::Gap(gap) => {
                // The framing layer checks monotonic clocks; avoid wrapping the
                // segmenter's sample counter on a hostile near-u64::MAX gap.
                ensure!(
                    gap.offset.checked_add(gap.extent).is_some(),
                    "PCM gap clock overflow"
                );
                segmenter.pause_skip(gap.extent);
                emit(Event::Gap(gap))?;
            }
            Frame::End(EndStatus::Complete) => {
                segmenter.flush(&mut |segment| segments.push(segment));
                deliver(segments, backend, source, options, emit)?;
                return emit(Event::Complete);
            }
            Frame::End(EndStatus::Aborted(reason)) => bail!("PCM producer aborted: {reason}"),
        }
        deliver(segments, backend, source, options, emit)?;
    }
}

fn deliver(
    segments: Vec<Segment>,
    backend: &mut impl Backend,
    source: &str,
    options: &Options,
    emit: &mut impl FnMut(Event) -> Result<()>,
) -> Result<()> {
    for segment in segments {
        let started = Instant::now();
        let observation = hear_segment(backend, options, &segment, source, None, now_ms()?)?;
        emit(Event::Utterance {
            observation,
            processing_ms: started.elapsed().as_millis(),
        })?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
