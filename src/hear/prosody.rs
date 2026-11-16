//! Prosody: how a recorded voice sounds, apart from what it says.
//!
//! Speech recognition hears words; this reads tone. It measures one mono clip
//! with plain, well-known signal processing and no model, so it works in every
//! build, and it runs on the CPU: a ten-second 24 kHz clip is about 1,000
//! analysis frames, a fraction of a second of arithmetic, and moving it to the
//! GPU would cost more in transfers than it saves.
//!
//! Every measure is computed on 10 ms frames. What each one means:
//!
//! - *Activity.* A frame is active when its energy (mean square over a 25 ms
//!   window) is within 35 dB of the clip's 99th-percentile frame and above
//!   -60 dBFS. The speech span runs from the first to the last active frame.
//! - *Pauses.* A run of inactive frames inside the span lasting at least
//!   150 ms. Shorter gaps are stop closures, not pauses. Pause share is pause
//!   time over the span; articulation time is the span minus the pauses.
//! - *Pitch (F0).* YIN (de Cheveigné and Kawahara 2002): the cumulative-mean
//!   normalized difference function over a 25 ms integration window, the first
//!   dip below 0.2 between the lags of `f0_max` and `f0_min`, refined by a
//!   parabola. An active frame with such a dip is voiced. Reported over voiced
//!   frames: median in Hz, range in semitones between the 5th and 95th
//!   percentiles (robust to a stray octave error), and the standard deviation
//!   in semitones around the median (how much the melody moves).
//! - *Loudness.* Active-speech RMS in dBFS (power mean over active frames),
//!   its range as the 10th-to-95th percentile spread of active frame levels in
//!   dB, and the sample peak in dBFS. Absolute levels depend on how the clip
//!   was rendered or recorded; within one renderer they compare directly.
//! - *Harmonics-to-noise ratio (HNR).* Breathiness is noise riding on the
//!   voice, so it is measured where the voice is periodic: in each voiced
//!   frame, the normalized cross-correlation `r` between the window and the
//!   window one pitch period later, and `HNR = 10 log10(r / (1 - r))`, as in
//!   Boersma (1993) without his window correction. Reported as the median over
//!   voiced frames. A breathy or whispery voice has a low HNR.
//! - *Alpha ratio.* Vocal effort shows in spectral tilt: a soft, hushed voice
//!   has little energy above 1 kHz relative to below it. Over voiced frames,
//!   the energy in 1-5 kHz over the energy in 50-1000 Hz, in dB, from
//!   fourth-order Linkwitz-Riley band splits. Higher (less negative) means
//!   brighter, more effortful phonation. Needs a rate of at least 11 kHz.
//!   HNR and the alpha ratio are both reported because breath noise raises
//!   high-band energy while low effort lowers it: the two separate "breathy"
//!   from "quiet".
//! - *Rate.* Syllable nuclei after de Jong and Wempe (2009): peaks of the
//!   intensity contour (50 ms power average, in dB) in voiced frames, no more
//!   than 6 dB under the median active level, each separated from the previous
//!   kept peak by a dip of at least 2 dB and by 80 ms. Reported as nuclei per
//!   second of articulation time and of the whole span. The detector misses
//!   or merges syllables in fast connected speech, so compare rates between
//!   clips of the same text rather than reading them as exact syllable counts.

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;

const HOP_S: f64 = 0.010;
const WINDOW_S: f64 = 0.025;
const ACTIVE_BELOW_REFERENCE_DB: f64 = 35.0;
const ACTIVE_FLOOR_DBFS: f64 = -60.0;
const MIN_PAUSE_S: f64 = 0.150;
const YIN_THRESHOLD: f64 = 0.2;
const NUCLEUS_BELOW_MEDIAN_DB: f64 = 6.0;
const NUCLEUS_DIP_DB: f64 = 2.0;
const NUCLEUS_MIN_GAP_S: f64 = 0.080;
const SPLIT_HZ: f64 = 1000.0;
const LOW_EDGE_HZ: f64 = 50.0;
const HIGH_EDGE_HZ: f64 = 5000.0;

/// One decoded mono clip.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub rate: u32,
    pub samples: Vec<f32>,
}

/// The pitch search range. Defaults cover speaking voices from low male to
/// high female; narrow it for one known speaker to avoid octave errors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub f0_min_hz: f64,
    pub f0_max_hz: f64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            f0_min_hz: 70.0,
            f0_max_hz: 600.0,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.f0_min_hz.is_finite()
                && self.f0_max_hz.is_finite()
                && self.f0_min_hz >= 20.0
                && self.f0_max_hz > self.f0_min_hz,
            "the pitch range needs 20 Hz <= f0_min < f0_max"
        );
        Ok(())
    }
}

/// The measures of one clip; see the module documentation for each.
/// A measure is `None` when the clip has nothing to measure it on (no voiced
/// frames, say), never a made-up zero.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Prosody {
    pub rate_hz: u32,
    pub duration_s: f64,
    pub span_s: f64,
    pub articulation_s: f64,
    pub pauses: usize,
    pub pause_s: f64,
    pub pause_share: f64,
    pub active_frames: usize,
    pub voiced_frames: usize,
    pub voiced_share: f64,
    pub f0_median_hz: Option<f64>,
    pub f0_p5_hz: Option<f64>,
    pub f0_p95_hz: Option<f64>,
    pub f0_range_st: Option<f64>,
    pub f0_sd_st: Option<f64>,
    pub rms_dbfs: f64,
    pub loudness_range_db: f64,
    pub peak_dbfs: f64,
    pub hnr_median_db: Option<f64>,
    pub alpha_ratio_db: Option<f64>,
    pub nuclei: usize,
    pub nuclei_per_s: Option<f64>,
    pub nuclei_per_span_s: Option<f64>,
}

impl Prosody {
    /// Four indented lines for people, after a caller-supplied heading.
    pub fn text(&self) -> String {
        let opt = |value: Option<f64>, digits: usize| match value {
            Some(value) => format!("{value:.digits$}"),
            None => "-".to_owned(),
        };
        format!(
            "  span      {:.2} s of {:.2} s; {} pause(s), {:.2} s, {:.0}% of the span\n  \
             pitch     median {} Hz, range {} st (p5 {} .. p95 {} Hz), sd {} st\n  \
             loudness  {:.1} dBFS active RMS, range {:.1} dB (p10..p95), peak {:.1} dBFS\n  \
             voice     HNR {} dB median, alpha ratio {} dB, voiced {:.0}% of active\n  \
             rate      {} nuclei, {} /s articulation, {} /s over the span",
            self.span_s,
            self.duration_s,
            self.pauses,
            self.pause_s,
            100.0 * self.pause_share,
            opt(self.f0_median_hz, 0),
            opt(self.f0_range_st, 1),
            opt(self.f0_p5_hz, 0),
            opt(self.f0_p95_hz, 0),
            opt(self.f0_sd_st, 2),
            self.rms_dbfs,
            self.loudness_range_db,
            self.peak_dbfs,
            opt(self.hnr_median_db, 1),
            opt(self.alpha_ratio_db, 1),
            100.0 * self.voiced_share,
            self.nuclei,
            opt(self.nuclei_per_s, 2),
            opt(self.nuclei_per_span_s, 2),
        )
    }
}

/// Decode a RIFF/WAVE clip: integer PCM (8, 16, 24 or 32 bit) or IEEE float
/// (32 or 64 bit), plain or extensible, mixed down to mono by averaging.
pub fn decode_wav(bytes: &[u8]) -> Result<Clip> {
    ensure!(
        bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "prosody reads RIFF/WAVE clips only; this input is not one"
    );
    let mut offset = 12_usize;
    let mut format = None;
    let mut data = None;
    while offset + 8 <= bytes.len() {
        let header = &bytes[offset..offset + 8];
        let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let start = offset + 8;
        let end = start
            .checked_add(length)
            .context("WAV chunk size overflow")?;
        let body = bytes
            .get(start..end)
            .with_context(|| format!("truncated WAV chunk {:?}", &header[..4]))?;
        match &header[..4] {
            b"fmt " => {
                ensure!(format.is_none(), "WAV has more than one format chunk");
                ensure!(body.len() >= 16, "truncated WAV format chunk");
                let mut tag = u16::from_le_bytes(body[..2].try_into().unwrap());
                let channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
                let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let align = u16::from_le_bytes(body[12..14].try_into().unwrap());
                let bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
                if tag == 0xFFFE {
                    ensure!(body.len() >= 26, "truncated WAVE_FORMAT_EXTENSIBLE chunk");
                    tag = u16::from_le_bytes(body[24..26].try_into().unwrap());
                }
                ensure!(channels > 0 && rate > 0, "WAV needs channels and a rate");
                ensure!(
                    bits % 8 == 0 && u32::from(align) == u32::from(channels) * u32::from(bits / 8),
                    "WAV block alignment does not match its channels and sample width"
                );
                ensure!(
                    matches!((tag, bits), (1, 8 | 16 | 24 | 32) | (3, 32 | 64)),
                    "unsupported WAV encoding (format tag {tag}, {bits} bit)"
                );
                format = Some((tag, channels, rate, bits));
            }
            b"data" => {
                ensure!(data.is_none(), "WAV has more than one data chunk");
                data = Some(body);
            }
            _ => {}
        }
        offset = end + (length & 1);
    }
    let (tag, channels, rate, bits) = format.context("WAV has no format chunk")?;
    let data = data.context("WAV has no data chunk")?;
    let width = usize::from(bits / 8);
    let frame = width * usize::from(channels);
    ensure!(
        !data.is_empty() && data.len() % frame == 0,
        "WAV samples are empty or end mid-frame"
    );
    let sample = |raw: &[u8]| -> f64 {
        match (tag, bits) {
            (1, 8) => (f64::from(raw[0]) - 128.0) / 128.0,
            (1, 16) => f64::from(i16::from_le_bytes([raw[0], raw[1]])) / 32768.0,
            (1, 24) => {
                f64::from(i32::from_le_bytes([0, raw[0], raw[1], raw[2]]) >> 8) / 8_388_608.0
            }
            (1, 32) => f64::from(i32::from_le_bytes(raw.try_into().unwrap())) / 2_147_483_648.0,
            (3, 32) => f64::from(f32::from_le_bytes(raw.try_into().unwrap())),
            _ => f64::from_le_bytes(raw.try_into().unwrap()),
        }
    };
    let samples: Vec<f32> = data
        .chunks_exact(frame)
        .map(|frame| {
            let sum: f64 = frame.chunks_exact(width).map(sample).sum();
            (sum / f64::from(channels)) as f32
        })
        .collect();
    ensure!(
        samples.iter().all(|s| s.is_finite()),
        "WAV contains nonfinite samples"
    );
    Ok(Clip { rate, samples })
}

/// Measure one clip.
pub fn analyze(clip: &Clip, settings: &Settings) -> Result<Prosody> {
    settings.validate()?;
    let rate = f64::from(clip.rate);
    ensure!(
        settings.f0_max_hz * 2.0 < rate,
        "a {} Hz clip cannot carry a {} Hz pitch",
        clip.rate,
        settings.f0_max_hz
    );
    let x: Vec<f64> = clip.samples.iter().map(|&s| f64::from(s)).collect();
    let hop = (HOP_S * rate).round() as usize;
    let window = (WINDOW_S * rate).round() as usize;
    let tau_min = (rate / settings.f0_max_hz).floor().max(2.0) as usize;
    let tau_max = (rate / settings.f0_min_hz).ceil() as usize;
    let need = window + tau_max + 1;
    if x.len() < need {
        bail!(
            "the clip is {:.3} s long; prosody needs at least {:.3} s",
            x.len() as f64 / rate,
            need as f64 / rate
        );
    }
    let frames = (x.len() - need) / hop + 1;

    // Frame energy over the integration window that starts at the frame.
    let level: Vec<f64> = (0..frames)
        .map(|f| {
            let w = &x[f * hop..f * hop + window];
            db(w.iter().map(|v| v * v).sum::<f64>() / window as f64)
        })
        .collect();
    let reference = percentile(&level, 0.99);
    let gate = (reference - ACTIVE_BELOW_REFERENCE_DB).max(ACTIVE_FLOOR_DBFS);
    let active: Vec<bool> = level.iter().map(|&l| l > gate).collect();

    // Pitch and periodicity.
    let mut f0 = vec![None; frames];
    let mut hnr = Vec::new();
    let mut diff = vec![0.0; tau_max + 1];
    for f in 0..frames {
        if !active[f] {
            continue;
        }
        let s = f * hop;
        for (tau, d) in diff.iter_mut().enumerate().skip(1) {
            *d = (0..window)
                .map(|j| {
                    let e = x[s + j] - x[s + j + tau];
                    e * e
                })
                .sum();
        }
        let mut cmnd = vec![1.0; tau_max + 1];
        let mut running = 0.0;
        for tau in 1..=tau_max {
            running += diff[tau];
            cmnd[tau] = if running > 0.0 {
                diff[tau] * tau as f64 / running
            } else {
                1.0
            };
        }
        let Some(mut tau) = (tau_min..=tau_max).find(|&t| cmnd[t] < YIN_THRESHOLD) else {
            continue;
        };
        while tau < tau_max && cmnd[tau + 1] < cmnd[tau] {
            tau += 1;
        }
        let shift = if tau > tau_min && tau < tau_max {
            let (a, b, c) = (cmnd[tau - 1], cmnd[tau], cmnd[tau + 1]);
            let curve = a - 2.0 * b + c;
            if curve > 0.0 {
                0.5 * (a - c) / curve
            } else {
                0.0
            }
        } else {
            0.0
        };
        f0[f] = Some(rate / (tau as f64 + shift));
        let (mut cross, mut here, mut there) = (0.0, 0.0, 0.0);
        for j in 0..window {
            let (a, b) = (x[s + j], x[s + j + tau]);
            cross += a * b;
            here += a * a;
            there += b * b;
        }
        if here > 0.0 && there > 0.0 {
            let r = (cross / (here * there).sqrt()).clamp(1e-6, 1.0 - 1e-6);
            hnr.push(10.0 * (r / (1.0 - r)).log10());
        }
    }
    let voiced: Vec<bool> = f0.iter().map(Option::is_some).collect();
    let pitches: Vec<f64> = f0.iter().flatten().copied().collect();

    // Span and pauses.
    let first = active.iter().position(|&a| a);
    let last = active.iter().rposition(|&a| a);
    let hop_s = hop as f64 / rate;
    let (span_s, pauses, pause_s) = match (first, last) {
        (Some(first), Some(last)) => {
            let min_run = (MIN_PAUSE_S / hop_s).round() as usize;
            let mut pauses = 0;
            let mut paused = 0;
            let mut run = 0;
            for &a in &active[first..=last] {
                if a {
                    if run >= min_run {
                        pauses += 1;
                        paused += run;
                    }
                    run = 0;
                } else {
                    run += 1;
                }
            }
            (
                (last + 1 - first) as f64 * hop_s,
                pauses,
                paused as f64 * hop_s,
            )
        }
        _ => (0.0, 0, 0.0),
    };
    let articulation_s = span_s - pause_s;

    // Loudness over active frames.
    let active_levels: Vec<f64> = (0..frames)
        .filter(|&f| active[f])
        .map(|f| level[f])
        .collect();
    let rms_dbfs = if active_levels.is_empty() {
        f64::NEG_INFINITY
    } else {
        db(active_levels.iter().map(|l| undb(*l)).sum::<f64>() / active_levels.len() as f64)
    };
    let loudness_range_db = if active_levels.is_empty() {
        0.0
    } else {
        percentile(&active_levels, 0.95) - percentile(&active_levels, 0.10)
    };
    let peak = x.iter().fold(0.0_f64, |m, v| m.max(v.abs()));

    // Pitch statistics in semitones.
    let (f0_median_hz, f0_p5_hz, f0_p95_hz, f0_range_st, f0_sd_st) = if pitches.is_empty() {
        (None, None, None, None, None)
    } else {
        let median = percentile(&pitches, 0.5);
        let (p5, p95) = (percentile(&pitches, 0.05), percentile(&pitches, 0.95));
        let sd = (pitches
            .iter()
            .map(|p| semitones(*p, median).powi(2))
            .sum::<f64>()
            / pitches.len() as f64)
            .sqrt();
        (
            Some(median),
            Some(p5),
            Some(p95),
            Some(semitones(p95, p5)),
            Some(sd),
        )
    };

    // Spectral tilt over voiced frames.
    let alpha_ratio_db = if rate >= 2.2 * HIGH_EDGE_HZ && voiced.iter().any(|&v| v) {
        let low = highpass2(&lowpass4(&x, SPLIT_HZ, rate), LOW_EDGE_HZ, rate);
        let high = lowpass2(&highpass4(&x, SPLIT_HZ, rate), HIGH_EDGE_HZ, rate);
        let (mut e_low, mut e_high) = (0.0, 0.0);
        for f in (0..frames).filter(|&f| voiced[f]) {
            for j in f * hop..f * hop + window {
                e_low += low[j] * low[j];
                e_high += high[j] * high[j];
            }
        }
        (e_low > 0.0 && e_high > 0.0).then(|| 10.0 * (e_high / e_low).log10())
    } else {
        None
    };

    // Syllable nuclei on a 50 ms power-averaged contour.
    let half = ((0.025 / hop_s).round() as usize).max(1);
    let contour: Vec<f64> = (0..frames)
        .map(|f| {
            let (a, b) = (f.saturating_sub(half), (f + half).min(frames - 1));
            db((a..=b).map(|g| undb(level[g])).sum::<f64>() / (b + 1 - a) as f64)
        })
        .collect();
    let floor = if active_levels.is_empty() {
        f64::INFINITY
    } else {
        percentile(&active_levels, 0.5) - NUCLEUS_BELOW_MEDIAN_DB
    };
    let min_gap = (NUCLEUS_MIN_GAP_S / hop_s).round() as usize;
    let mut kept: Vec<usize> = Vec::new();
    for f in 1..frames.saturating_sub(1) {
        let peak = contour[f] >= contour[f - 1] && contour[f] > contour[f + 1];
        if !(peak && voiced[f] && contour[f] >= floor) {
            continue;
        }
        let Some(&previous) = kept.last() else {
            kept.push(f);
            continue;
        };
        let dip = contour[previous..=f]
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        let separate = f - previous >= min_gap
            && contour[f] - dip >= NUCLEUS_DIP_DB
            && contour[previous] - dip >= NUCLEUS_DIP_DB;
        if separate {
            kept.push(f);
        } else if contour[f] > contour[previous] {
            *kept.last_mut().unwrap() = f;
        }
    }
    let nuclei = kept.len();
    let per = |seconds: f64| (seconds > 0.0).then(|| nuclei as f64 / seconds);

    let active_frames = active_levels.len();
    let voiced_frames = pitches.len();
    Ok(Prosody {
        rate_hz: clip.rate,
        duration_s: x.len() as f64 / rate,
        span_s,
        articulation_s,
        pauses,
        pause_s,
        pause_share: if span_s > 0.0 { pause_s / span_s } else { 0.0 },
        active_frames,
        voiced_frames,
        voiced_share: if active_frames > 0 {
            voiced_frames as f64 / active_frames as f64
        } else {
            0.0
        },
        f0_median_hz,
        f0_p5_hz,
        f0_p95_hz,
        f0_range_st,
        f0_sd_st,
        rms_dbfs,
        loudness_range_db,
        peak_dbfs: db(peak * peak),
        hnr_median_db: (!hnr.is_empty()).then(|| percentile(&hnr, 0.5)),
        alpha_ratio_db,
        nuclei,
        nuclei_per_s: per(articulation_s),
        nuclei_per_span_s: per(span_s),
    })
}

fn db(power: f64) -> f64 {
    10.0 * power.max(1e-20).log10()
}
fn undb(level: f64) -> f64 {
    10f64.powf(level / 10.0)
}
fn semitones(hz: f64, reference: f64) -> f64 {
    12.0 * (hz / reference).log2()
}
/// Linear-interpolated percentile of a nonempty slice, `q` in [0, 1].
fn percentile(values: &[f64], q: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let position = q * (sorted.len() - 1) as f64;
    let (lower, upper) = (position.floor() as usize, position.ceil() as usize);
    sorted[lower] + (sorted[upper] - sorted[lower]) * (position - lower as f64)
}

/// RBJ biquad, Butterworth Q, direct form I.
fn biquad(x: &[f64], hz: f64, rate: f64, high: bool) -> Vec<f64> {
    let w = 2.0 * std::f64::consts::PI * hz / rate;
    let alpha = w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
    let cos = w.cos();
    let (b0, b1, b2) = if high {
        ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0)
    } else {
        ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0)
    };
    let (a0, a1, a2) = (1.0 + alpha, -2.0 * cos, 1.0 - alpha);
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    x.iter()
        .map(|&v| {
            let y = (b0 * v + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2) / a0;
            (x2, x1, y2, y1) = (x1, v, y1, y);
            y
        })
        .collect()
}
fn lowpass2(x: &[f64], hz: f64, rate: f64) -> Vec<f64> {
    biquad(x, hz, rate, false)
}
fn highpass2(x: &[f64], hz: f64, rate: f64) -> Vec<f64> {
    biquad(x, hz, rate, true)
}
/// Fourth-order Linkwitz-Riley: two Butterworth sections in cascade, whose
/// low and high halves sum to an allpass, so the split loses no energy.
fn lowpass4(x: &[f64], hz: f64, rate: f64) -> Vec<f64> {
    lowpass2(&lowpass2(x, hz, rate), hz, rate)
}
fn highpass4(x: &[f64], hz: f64, rate: f64) -> Vec<f64> {
    highpass2(&highpass2(x, hz, rate), hz, rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const RATE: u32 = 16_000;

    fn harmonic(hz: f64, seconds: f64, amp: f64) -> Vec<f32> {
        let n = (seconds * f64::from(RATE)) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(RATE);
                let v: f64 = (1..=5)
                    .map(|k| (2.0 * PI * hz * k as f64 * t).sin() / k as f64)
                    .sum();
                (amp * v / 2.0) as f32
            })
            .collect()
    }
    fn silence(seconds: f64) -> Vec<f32> {
        vec![0.0; (seconds * f64::from(RATE)) as usize]
    }
    /// Deterministic white-ish noise (an LCG), so tests need no RNG crate.
    fn noise(n: usize, amp: f64, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (amp * ((state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0)) as f32
            })
            .collect()
    }
    fn clip(samples: Vec<f32>) -> Clip {
        Clip {
            rate: RATE,
            samples,
        }
    }
    fn wav(tag: u16, bits: u16, channels: u16, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&RATE.to_le_bytes());
        let align = channels * bits / 8;
        bytes.extend_from_slice(&(RATE * u32::from(align)).to_le_bytes());
        bytes.extend_from_slice(&align.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    #[test]
    fn decodes_pcm16_float_and_stereo_to_the_same_mono() {
        let values = [0.5f32, -0.25, 0.0];
        let pcm: Vec<u8> = values
            .iter()
            .flat_map(|v| ((v * 32768.0) as i16).to_le_bytes())
            .collect();
        let float: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let stereo: Vec<u8> = values
            .iter()
            .flat_map(|v| {
                let [a, b] = ((v * 32768.0) as i16).to_le_bytes();
                [a, b, 0, 0]
            })
            .collect();
        assert_eq!(decode_wav(&wav(1, 16, 1, &pcm)).unwrap().samples, values);
        assert_eq!(decode_wav(&wav(3, 32, 1, &float)).unwrap().samples, values);
        let halved: Vec<f32> = values.iter().map(|v| v / 2.0).collect();
        assert_eq!(decode_wav(&wav(1, 16, 2, &stereo)).unwrap().samples, halved);
        assert!(decode_wav(b"OggS not a wave").is_err());
        assert!(decode_wav(&wav(1, 16, 1, &pcm[..5])).is_err());
        assert!(decode_wav(&wav(2, 16, 1, &pcm)).is_err());
    }

    #[test]
    fn a_steady_tone_reads_its_pitch_with_no_range() {
        let p = analyze(&clip(harmonic(200.0, 1.0, 0.3)), &Settings::default()).unwrap();
        let median = p.f0_median_hz.unwrap();
        assert!((median - 200.0).abs() < 1.0, "{median}");
        assert!(p.f0_range_st.unwrap() < 0.2, "{p:?}");
        assert!(p.voiced_share > 0.95, "{p:?}");
        assert_eq!(p.pauses, 0);
    }

    #[test]
    fn a_glide_over_an_octave_reads_about_twelve_semitones() {
        let n = f64::from(RATE) as usize;
        let mut phase = 0.0;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let hz = 150.0 * 2f64.powf(i as f64 / n as f64);
                phase += 2.0 * PI * hz / f64::from(RATE);
                (0.3 * phase.sin()) as f32
            })
            .collect();
        let p = analyze(&clip(samples), &Settings::default()).unwrap();
        let range = p.f0_range_st.unwrap();
        // p5..p95 of a uniform glide covers 90% of its twelve semitones.
        assert!((range - 10.8).abs() < 0.6, "{range}");
    }

    #[test]
    fn noise_lowers_the_harmonics_to_noise_ratio() {
        let clean = harmonic(220.0, 1.0, 0.3);
        let breath = noise(clean.len(), 0.08, 7);
        let breathy: Vec<f32> = clean.iter().zip(&breath).map(|(a, b)| a + b).collect();
        let clean = analyze(&clip(clean), &Settings::default()).unwrap();
        let breathy = analyze(&clip(breathy), &Settings::default()).unwrap();
        assert!(
            clean.hnr_median_db.unwrap() > breathy.hnr_median_db.unwrap() + 10.0,
            "{clean:?} {breathy:?}"
        );
        // Breath noise is broadband, so it also brightens the spectrum.
        assert!(breathy.alpha_ratio_db.unwrap() > clean.alpha_ratio_db.unwrap());
    }

    #[test]
    fn pauses_and_nuclei_count_separated_bursts() {
        let mut samples = silence(0.2);
        for _ in 0..4 {
            samples.extend(harmonic(180.0, 0.18, 0.3));
            samples.extend(silence(0.3));
        }
        let p = analyze(&clip(samples), &Settings::default()).unwrap();
        assert_eq!(p.nuclei, 4, "{p:?}");
        assert_eq!(p.pauses, 3, "{p:?}");
        // Each 300 ms gap reads a little shorter: the energy window spans its edge.
        assert!((p.pause_s - 0.85).abs() < 0.08, "{p:?}");
        assert!(p.nuclei_per_s.unwrap() > p.nuclei_per_span_s.unwrap());
    }

    #[test]
    fn loudness_follows_gain_and_silence_measures_nothing() {
        let quiet = analyze(&clip(harmonic(200.0, 0.5, 0.05)), &Settings::default()).unwrap();
        let loud = analyze(&clip(harmonic(200.0, 0.5, 0.5)), &Settings::default()).unwrap();
        assert!((loud.rms_dbfs - quiet.rms_dbfs - 20.0).abs() < 0.1);
        let silent = analyze(&clip(silence(0.5)), &Settings::default()).unwrap();
        assert_eq!(silent.voiced_frames, 0);
        assert_eq!(silent.f0_median_hz, None);
        assert_eq!(silent.hnr_median_db, None);
        assert!(analyze(&clip(silence(0.01)), &Settings::default()).is_err());
        let bad = Settings {
            f0_min_hz: 300.0,
            f0_max_hz: 100.0,
        };
        assert!(analyze(&clip(silence(0.5)), &bad).is_err());
    }
}
