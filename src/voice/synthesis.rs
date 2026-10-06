//! Explicit resident speech backend, drained into existing audio/host sinks.
use anybytes::Bytes;
use anyhow::{Context, Result, ensure};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(feature = "voice")]
use std::sync::{Arc, Mutex};
use triblespace::prelude::Id;

#[derive(Clone, Debug)]
pub enum ModelSources {
    Qwen3Tts {
        pile: PathBuf,
        reference_wav: PathBuf,
        reference_text: PathBuf,
        reference_codes: PathBuf,
        #[cfg(feature = "voice")]
        variant: mary::speak::Qwen3TtsVariant,
    },
    Breeze(BreezeSources),
    /// Retain a launcher error until a caller explicitly primes/synthesizes.
    /// Discovery/construction remains inert; this never selects another model.
    Invalid(String),
}

#[derive(Clone, Debug)]
pub struct BreezeSources {
    /// Launcher custody promise through CUDA storage teardown, including errors.
    pub immutable_pile: bool,
    pub pile: PathBuf,
    pub model_root: Id,
    pub config_root: Id,
    pub tokenizer_asset: Id,
    pub external_codec_root: Id,
    pub external_codec_config_root: Id,
    pub reference_wav: PathBuf,
    pub reference_text: PathBuf,
    pub direction: Option<String>,
    pub cfg_scale: f32,
}
impl ModelSources {
    /// Capture launcher configuration without opening any file or device.
    pub fn from_environment() -> Self {
        Self::from_lookup(&|name| std::env::var_os(name), &crate::model_dir())
            .unwrap_or_else(|error| Self::Invalid(error.to_string()))
    }

    fn from_lookup(get: &impl Fn(&str) -> Option<OsString>, directory: &Path) -> Result<Self> {
        let backend = get("FACULTIES_VOICE_BACKEND").unwrap_or_else(|| "qwen3tts".into());
        match backend.to_str() {
            Some("qwen3tts") => Ok(Self::Qwen3Tts {
                pile: get("QWEN3TTS_PILE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| directory.join("qwen3tts.pile")),
                reference_wav: directory.join("ref_voice_v2_24k.wav"),
                reference_text: directory.join("ref_voice_v2.txt"),
                reference_codes: directory.join("ref_voice_v2_code.npy"),
                #[cfg(feature = "voice")]
                variant: if get("MARY_SPEAK_MODEL").as_deref() == Some(std::ffi::OsStr::new("0.6b"))
                {
                    mary::speak::Qwen3TtsVariant::Base0_6B
                } else {
                    mary::speak::Qwen3TtsVariant::Base1_7B
                },
            }),
            Some("breeze") => {
                let text = |name: &str| -> Result<String> {
                    let value = get(name).with_context(|| format!("Breeze requires {name}"))?;
                    let value = value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("{name} must be UTF-8"))?;
                    ensure!(!value.trim().is_empty(), "{name} must not be blank");
                    Ok(value)
                };
                let path = |name: &str| -> Result<PathBuf> {
                    let value = get(name).with_context(|| format!("Breeze requires {name}"))?;
                    ensure!(!value.is_empty(), "{name} must not be empty");
                    Ok(PathBuf::from(value))
                };
                let id = |name: &str| -> Result<Id> {
                    let value = text(name)?;
                    ensure!(
                        value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
                        "{name} must be a full 32-hex opaque ID"
                    );
                    Id::from_hex(&value).with_context(|| format!("invalid {name}"))
                };
                // This records the launcher's custody promise, not a claim that
                // an O_RDONLY descriptor or mode bits make memory aliases safe.
                ensure!(
                    text("FACULTIES_BREEZE_IMMUTABLE_PILE")? == "1",
                    "FACULTIES_BREEZE_IMMUTABLE_PILE=1 must affirm immutable mapped bytes through CUDA teardown"
                );
                let direction = get("FACULTIES_BREEZE_DIRECTION")
                    .map(|_| text("FACULTIES_BREEZE_DIRECTION"))
                    .transpose()?;
                let cfg_scale = get("FACULTIES_BREEZE_CFG_SCALE")
                    .map(|_| {
                        text("FACULTIES_BREEZE_CFG_SCALE")?
                            .parse::<f32>()
                            .context("FACULTIES_BREEZE_CFG_SCALE must be numeric")
                    })
                    .transpose()?
                    .unwrap_or(1.0);
                ensure!(
                    cfg_scale.is_finite() && cfg_scale > 0.0,
                    "Breeze CFG scale must be finite and positive"
                );
                ensure!(
                    cfg_scale == 1.0 || direction.is_some(),
                    "non-unit Breeze CFG requires an explicit direction"
                );
                Ok(Self::Breeze(BreezeSources {
                    immutable_pile: true,
                    pile: path("FACULTIES_BREEZE_PILE")?,
                    model_root: id("FACULTIES_BREEZE_MODEL_ROOT")?,
                    config_root: id("FACULTIES_BREEZE_CONFIG_ROOT")?,
                    tokenizer_asset: id("FACULTIES_BREEZE_TOKENIZER_ASSET")?,
                    external_codec_root: id("FACULTIES_BREEZE_EXTERNAL_CODEC_ROOT")?,
                    external_codec_config_root: id("FACULTIES_BREEZE_EXTERNAL_CODEC_CONFIG_ROOT")?,
                    reference_wav: path("FACULTIES_BREEZE_REFERENCE_WAV")?,
                    reference_text: path("FACULTIES_BREEZE_REFERENCE_TEXT")?,
                    direction,
                    cfg_scale,
                }))
            }
            _ => anyhow::bail!("FACULTIES_VOICE_BACKEND must be exactly qwen3tts or breeze"),
        }
    }

    fn check_backend(&self) -> Result<()> {
        match self {
            Self::Invalid(error) => anyhow::bail!("invalid speech configuration: {error}"),
            Self::Breeze(source) if !source.immutable_pile => anyhow::bail!(
                "Breeze requires explicit immutable pile custody through CUDA teardown"
            ),
            Self::Breeze(_) if !cfg!(feature = "voice-breeze") => {
                anyhow::bail!("Breeze speech requires a build with the voice-breeze feature")
            }
            _ => Ok(()),
        }
    }
}
#[derive(Clone, Debug)]
pub struct AudioClip {
    pub wav: Bytes,
    pub sample_rate: u32,
    pub sample_count: usize,
}
impl AudioClip {
    /// PCM16 conversion matches Mary's existing WAV writer, but carries bytes
    /// instead of writing an implicit temporary file. Invalid frames fail first.
    pub fn from_samples(samples: &[f32], sample_rate: u32) -> Result<Self> {
        ensure!(!samples.is_empty(), "synthesis returned no audio samples");
        ensure!(
            sample_rate > 0 && sample_rate <= u32::MAX / 2,
            "invalid PCM sample rate"
        );
        ensure!(
            samples.iter().all(|sample| sample.is_finite()),
            "synthesis returned a non-finite sample"
        );
        let data_len = samples
            .len()
            .checked_mul(2)
            .and_then(|size| u32::try_from(size).ok())
            .filter(|size| *size <= u32::MAX - 36)
            .context("PCM clip exceeds RIFF/WAV size limit")?;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&((sample.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        Ok(Self {
            wav: wav.into(),
            sample_rate,
            sample_count: samples.len(),
        })
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.wav.as_ref())
            .with_context(|| format!("write WAV to {}", path.display()))
    }
    pub fn duration_seconds(&self) -> f64 {
        self.sample_count as f64 / f64::from(self.sample_rate)
    }
}

#[derive(Clone)]
pub struct Synthesizer {
    pub sources: ModelSources,
    #[cfg(feature = "voice")]
    resident: Arc<Mutex<Option<Resident>>>,
}
#[cfg(feature = "voice")]
struct Resident {
    session: mary::speak::Synthesizer,
    reference_seconds: f32,
    reference_chars: usize,
}
impl std::fmt::Debug for Synthesizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("Synthesizer");
        debug.field("sources", &self.sources);
        #[cfg(feature = "voice")]
        debug.field(
            "resident",
            &self
                .resident
                .lock()
                .map(|resident| resident.is_some())
                .unwrap_or(true),
        );
        debug.finish()
    }
}
impl Synthesizer {
    pub fn new(sources: ModelSources) -> Self {
        Self {
            sources,
            #[cfg(feature = "voice")]
            resident: Arc::new(Mutex::new(None)),
        }
    }
    /// Finite audio synthesis only. Never enumerates, selects, or plays devices,
    /// creates a pause file, writes a journal, or saves audio on the host.
    pub fn synthesize(&self, text: &str) -> Result<AudioClip> {
        super::operations::validate_text(text)?;
        self.sources.check_backend()?;
        #[cfg(not(feature = "voice"))]
        anyhow::bail!("speech synthesis requires a build with the `voice` feature");
        #[cfg(feature = "voice")]
        {
            let PreparedSpeech {
                mut stream,
                sample_rate,
                ..
            } = self.start(text)?;
            let mut samples = Vec::new();
            for chunk in stream.by_ref() {
                samples.extend_from_slice(&chunk);
            }
            stream.finish()?;
            AudioClip::from_samples(&samples, sample_rate)
        }
    }
    /// Start the resident model loader once, without queuing an utterance.
    /// Construction and MCP discovery remain inert; a streaming caller uses
    /// this immediately before it begins accepting text so model loading can
    /// overlap the arrival of the first sentence.
    pub fn prime(&self) -> Result<()> {
        self.sources.check_backend()?;
        #[cfg(not(feature = "voice"))]
        anyhow::bail!("speech synthesis requires a build with the `voice` feature");
        #[cfg(feature = "voice")]
        {
            let mut resident = self
                .resident
                .lock()
                .map_err(|_| anyhow::anyhow!("resident speech session is poisoned"))?;
            if resident.is_none() {
                *resident = Some(self.load_resident()?);
            }
            Ok(())
        }
    }
    #[cfg(feature = "voice")]
    pub(super) fn start(&self, text: &str) -> Result<PreparedSpeech> {
        super::operations::validate_text(text)?;
        self.prime()?;
        let resident = self
            .resident
            .lock()
            .map_err(|_| anyhow::anyhow!("resident speech session is poisoned"))?;
        let resident = resident.as_ref().expect("initialized above");
        let estimate = estimate_audio_secs(
            text.chars().count(),
            resident.reference_seconds,
            resident.reference_chars,
        );
        let started = std::time::Instant::now();
        let stream = resident.session.speak(text)?;
        Ok(PreparedSpeech {
            stream,
            sample_rate: mary::speak::SpeakStream::SAMPLE_RATE,
            estimated_seconds: estimate,
            started,
        })
    }

    #[cfg(feature = "voice")]
    fn load_resident(&self) -> Result<Resident> {
        self.sources.check_backend()?;
        let (pile, reference_wav, reference_text, reference_codes, variant) = match &self.sources {
            ModelSources::Qwen3Tts {
                pile,
                reference_wav,
                reference_text,
                reference_codes,
                variant,
            } => (
                pile,
                reference_wav,
                reference_text,
                reference_codes,
                variant,
            ),
            ModelSources::Breeze(source) => {
                #[cfg(feature = "voice-breeze")]
                return Self::load_breeze(source);
                #[cfg(not(feature = "voice-breeze"))]
                {
                    let _ = source;
                    anyhow::bail!("Breeze speech requires a build with the voice-breeze feature");
                }
            }
            ModelSources::Invalid(error) => anyhow::bail!("invalid speech configuration: {error}"),
        };
        let ref_text = std::fs::read_to_string(reference_text)
            .context("read configured reference transcript")?;
        ensure!(!ref_text.trim().is_empty(), "reference transcript is empty");
        // Preflight the reference before Mary's infallible reader sees it.
        let reference = std::fs::read(reference_wav).context("read configured reference WAV")?;
        let (sample_count, sample_rate) = reference_metadata(&reference)?;
        preflight_reference_codes(reference_codes)?;
        let weights = crate::model_storage::with_snapshot(pile, "Qwen3-TTS", |snapshot| {
            mary::speak::Qwen3TtsWeights::from_snapshot(snapshot.clone(), *variant)
                .context("select configured native Qwen3-TTS cohort")
        })?;
        let session = mary::speak::Synthesizer::spawn(
            weights,
            reference_wav,
            ref_text.trim(),
            reference_codes,
        )?;
        Ok(Resident {
            session,
            reference_seconds: sample_count as f32 / sample_rate as f32,
            reference_chars: ref_text.trim().chars().count(),
        })
    }

    #[cfg(feature = "voice-breeze")]
    fn load_breeze(source: &BreezeSources) -> Result<Resident> {
        use mary::models::breeze::{
            generator::GenerationOptions, load::Artifacts, resident::BreezeVoiceConfig,
        };
        let ref_text = std::fs::read_to_string(&source.reference_text)
            .context("read configured Breeze reference transcript")?;
        // Native FLOAT/PCM16 preflight; never Qwen's PCM16/NPY reference kit.
        let samples = mary::models::breeze::reference::read_wav(&source.reference_wav)?;
        let reference_seconds = samples.len() as f32 / 24_000.0;
        let reference_chars = ref_text.trim().chars().count();
        drop(samples);
        let config = BreezeVoiceConfig {
            pile: source.pile.clone(),
            artifacts: Artifacts {
                model_root: source.model_root,
                config_root: source.config_root,
                tokenizer_asset: source.tokenizer_asset,
                external_codec_root: source.external_codec_root,
                external_codec_config_root: source.external_codec_config_root,
            },
            reference_wav: source.reference_wav.clone(),
            reference_text: ref_text,
            direction: source.direction.clone(),
            options: GenerationOptions {
                cfg_scale: source.cfg_scale,
                ..Default::default()
            },
        };
        config.validate()?;
        // SAFETY: the explicit immutable-generation launcher contract was
        // required during configuration. It covers the genuine pile's mapped
        // bytes/page prefixes through CUDA storage teardown, also on errors;
        // neither this reader closing nor a worker dropping lifts that duty.
        let session = unsafe { mary::speak::Synthesizer::spawn_breeze(config) }?;
        Ok(Resident {
            session,
            reference_seconds,
            reference_chars,
        })
    }
}
#[cfg(feature = "voice")]
pub(super) struct PreparedSpeech {
    pub stream: mary::speak::SpeakStream,
    pub sample_rate: u32,
    pub estimated_seconds: f32,
    pub started: std::time::Instant,
}

/// Inspect the 24 kHz mono PCM16 reference shape required by Mary's speech
/// pipeline. Complete chunk lengths are checked before offsets are dereferenced.
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
fn reference_metadata(bytes: &[u8]) -> Result<(usize, u32)> {
    ensure!(
        bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "reference must be a RIFF/WAVE file"
    );
    let declared = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    ensure!(
        declared
            .checked_add(8)
            .is_some_and(|end| end == bytes.len()),
        "reference RIFF length does not match its bytes"
    );
    let mut offset = 12_usize;
    let mut rate = None;
    let mut samples = None;
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 8)
            .context("truncated WAV chunk header")?;
        let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let body_start = offset + 8;
        let end = body_start
            .checked_add(length)
            .context("WAV chunk length overflow")?;
        let body = bytes
            .get(body_start..end)
            .context("truncated WAV chunk body")?;
        match &header[..4] {
            b"fmt " => {
                ensure!(body.len() >= 16, "truncated WAV format");
                ensure!(
                    u16::from_le_bytes(body[..2].try_into().unwrap()) == 1
                        && u16::from_le_bytes(body[2..4].try_into().unwrap()) == 1
                        && u16::from_le_bytes(body[14..16].try_into().unwrap()) == 16,
                    "reference must be mono PCM16"
                );
                let value = u32::from_le_bytes(body[4..8].try_into().unwrap());
                ensure!(
                    value == 24_000,
                    "reference clip must be 24 kHz mono PCM16 (got {value} Hz)"
                );
                rate = Some(value);
            }
            b"data" => {
                ensure!(rate.is_some(), "WAV format must precede reference samples");
                ensure!(
                    length > 0 && length % 2 == 0,
                    "reference samples are empty or incomplete"
                );
                samples = Some(length / 2);
            }
            _ => {}
        }
        offset = end
            .checked_add(length & 1)
            .context("WAV padding overflow")?;
        ensure!(offset <= bytes.len(), "truncated WAV chunk padding");
    }
    Ok((
        samples.context("reference has no data chunk")?,
        rate.context("reference has no format chunk")?,
    ))
}

/// Reuse Mary's CPU decoder before model acquisition. Its ordinary assertions
/// become a fixed error here; normal I/O errors keep their original cause.
/// This does not recover abort/OOM or validate new numeric code semantics.
/// Mary later reopens this path, so the launcher must keep the reference kit
/// stable throughout synthesis; preflight does not freeze those host files.
#[cfg(feature = "voice")]
fn preflight_reference_codes(path: &Path) -> Result<()> {
    use mary::models::qwen3tts::config::NUM_CODE_GROUPS;
    let (data, shape) = std::panic::catch_unwind(|| mary::nn::npy::load_npy(path))
        .map_err(|_| {
            anyhow::anyhow!("configured reference codec cache decoder rejected its input")
        })?
        .with_context(|| format!("read configured reference codec cache {}", path.display()))?;
    ensure!(
        shape.len() == 2 && shape[0] > 0 && shape[1] == NUM_CODE_GROUPS,
        "reference codes must have nonempty shape (T, {NUM_CODE_GROUPS}), got {shape:?}"
    );
    let expected = shape[0]
        .checked_mul(NUM_CODE_GROUPS)
        .context("reference code dimensions overflow")?;
    ensure!(
        data.len() == expected,
        "reference code shape needs {expected} values, decoded {}",
        data.len()
    );
    Ok(())
}

/// Estimate an utterance's total audio duration from its character count,
/// calibrated by the reference kit: assume the generated speech runs at the
/// reference clip's chars-per-second. This is the same linear chars→duration
/// model mary's batch F5 path uses to size its mel window
/// (`say::synth_chunk`: `duration = ref_len / ref_chars * gen_chars`). It is
/// an ESTIMATE — the underrun guard in `stream_to_device` catches the cases
/// where reality runs longer.
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub(super) fn estimate_audio_secs(gen_chars: usize, ref_secs: f32, ref_chars: usize) -> f32 {
    if ref_chars == 0 {
        return 0.0;
    }
    ref_secs * gen_chars as f32 / ref_chars as f32
}

/// How much audio (seconds) must be buffered before starting playback so
/// synthesis at `production_rate` (audio-seconds produced per wall-second;
/// < 1 is slower than realtime) stays ahead of the playhead for the REST of
/// an utterance totalling `total_est_secs`.
///
/// Derivation: playback starting with `B` seconds buffered has consumed `t`
/// seconds of audio by wall-time `t`, while production has `B + rate·t`
/// ready. Their gap `B − t·(1 − rate)` shrinks linearly (for rate < 1) until
/// production finishes at `t = (T − B)/rate`, so the binding constraint is
/// at the END: `B ≥ T·(1 − rate)` keeps the buffer nonnegative throughout —
/// the EXACT no-underrun bound, not a heuristic. The margin absorbs rate
/// jitter and estimate error (it holds a `margin/rate` cushion at the worst
/// point); the floor keeps the queue from starting starved even when there
/// is no deficit at all.
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub(super) fn prebuffer_target_secs(total_est_secs: f32, production_rate: f32) -> f32 {
    const MARGIN_SECS: f32 = 0.5;
    const FLOOR_SECS: f32 = 0.15;
    let deficit = total_est_secs * (1.0 - production_rate).max(0.0);
    (deficit + MARGIN_SECS).max(FLOOR_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breeze_settings() -> Vec<(&'static str, OsString)> {
        let id = format!("{:x}", triblespace::prelude::fucid().id);
        vec![
            ("FACULTIES_VOICE_BACKEND", "breeze".into()),
            ("FACULTIES_BREEZE_IMMUTABLE_PILE", "1".into()),
            ("FACULTIES_BREEZE_PILE", "/explicit/native.pile".into()),
            ("FACULTIES_BREEZE_MODEL_ROOT", id.clone().into()),
            ("FACULTIES_BREEZE_CONFIG_ROOT", id.clone().into()),
            ("FACULTIES_BREEZE_TOKENIZER_ASSET", id.clone().into()),
            ("FACULTIES_BREEZE_EXTERNAL_CODEC_ROOT", id.clone().into()),
            ("FACULTIES_BREEZE_EXTERNAL_CODEC_CONFIG_ROOT", id.into()),
            (
                "FACULTIES_BREEZE_REFERENCE_WAV",
                "/explicit/preferred-float.wav".into(),
            ),
            (
                "FACULTIES_BREEZE_REFERENCE_TEXT",
                "/explicit/known.txt".into(),
            ),
        ]
    }

    fn parse_settings(settings: &[(&str, OsString)]) -> Result<ModelSources> {
        ModelSources::from_lookup(
            &|key| {
                settings
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.clone())
            },
            Path::new("/qwen-only-model-directory"),
        )
    }

    #[test]
    fn absent_backend_keeps_qwen_and_its_original_reference_kit() {
        match parse_settings(&[]).unwrap() {
            ModelSources::Qwen3Tts {
                pile,
                reference_wav,
                reference_text,
                reference_codes,
                ..
            } => {
                assert_eq!(pile, Path::new("/qwen-only-model-directory/qwen3tts.pile"));
                assert_eq!(
                    reference_wav,
                    Path::new("/qwen-only-model-directory/ref_voice_v2_24k.wav")
                );
                assert_eq!(
                    reference_text,
                    Path::new("/qwen-only-model-directory/ref_voice_v2.txt")
                );
                assert_eq!(
                    reference_codes,
                    Path::new("/qwen-only-model-directory/ref_voice_v2_code.npy")
                );
            }
            _ => panic!("default backend changed"),
        }
    }

    #[test]
    fn backend_names_are_strict_and_errors_do_not_fallback() {
        for name in ["", "Breeze", " breeze", "qwen", "unknown"] {
            assert!(parse_settings(&[("FACULTIES_VOICE_BACKEND", name.into())]).is_err());
        }
        assert!(
            ModelSources::Invalid("test configuration error".into())
                .check_backend()
                .is_err()
        );
    }

    #[test]
    #[cfg(feature = "voice")]
    fn qwen_variant_switch_preserves_exact_previous_fallback_semantics() {
        for value in [
            None,
            Some("0.6b"),
            Some("1.7b"),
            Some("unknown"),
            Some("0.6B"),
            Some(""),
        ] {
            let settings = value
                .map(|value| vec![("MARY_SPEAK_MODEL", value.into())])
                .unwrap_or_default();
            let ModelSources::Qwen3Tts { variant, .. } = parse_settings(&settings).unwrap() else {
                unreachable!()
            };
            let expected = if value == Some("0.6b") {
                mary::speak::Qwen3TtsVariant::Base0_6B
            } else {
                mary::speak::Qwen3TtsVariant::Base1_7B
            };
            assert_eq!(variant, expected);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let settings = [("MARY_SPEAK_MODEL", OsString::from_vec(vec![0xff]))];
            let ModelSources::Qwen3Tts { variant, .. } = parse_settings(&settings).unwrap() else {
                unreachable!()
            };
            assert_eq!(variant, mary::speak::Qwen3TtsVariant::Base1_7B);
        }
    }

    #[test]
    fn breeze_configuration_uses_only_its_explicit_assets_and_opaque_ids() {
        let mut settings = breeze_settings();
        settings.push(("QWEN3TTS_PILE", "/must-not-be-used.pile".into()));
        let selected = parse_settings(&settings).unwrap();
        let ModelSources::Breeze(source) = &selected else {
            panic!("wrong backend")
        };
        assert_eq!(source.pile, Path::new("/explicit/native.pile"));
        assert_eq!(
            source.reference_wav,
            Path::new("/explicit/preferred-float.wav")
        );
        assert_eq!(source.reference_text, Path::new("/explicit/known.txt"));
        assert_eq!(source.cfg_scale, 1.0);
        assert!(source.direction.is_none());
        assert!(source.immutable_pile);
        // Random IDs need not encode a source/model/version or be distinct.
        assert_eq!(source.model_root, source.external_codec_root);
        if cfg!(feature = "voice-breeze") {
            selected.check_backend().unwrap();
        } else {
            assert!(
                selected
                    .check_backend()
                    .unwrap_err()
                    .to_string()
                    .contains("voice-breeze")
            );
        }
    }

    #[test]
    fn breeze_rejects_missing_paths_partial_ids_and_unaffirmed_custody_without_io() {
        for key in [
            "FACULTIES_BREEZE_PILE",
            "FACULTIES_BREEZE_REFERENCE_WAV",
            "FACULTIES_BREEZE_REFERENCE_TEXT",
            "FACULTIES_BREEZE_IMMUTABLE_PILE",
        ] {
            let mut settings = breeze_settings();
            settings.retain(|(name, _)| *name != key);
            assert!(parse_settings(&settings).is_err(), "{key}");
        }
        for bad in ["abc", "not-a-root", "", "00000000000000000000000000000000"] {
            let mut settings = breeze_settings();
            settings
                .iter_mut()
                .find(|(name, _)| *name == "FACULTIES_BREEZE_MODEL_ROOT")
                .unwrap()
                .1 = bad.into();
            assert!(parse_settings(&settings).is_err(), "{bad}");
        }
        let ModelSources::Breeze(mut source) = parse_settings(&breeze_settings()).unwrap() else {
            unreachable!()
        };
        source.immutable_pile = false;
        assert!(ModelSources::Breeze(source).check_backend().is_err());
    }

    #[test]
    fn breeze_guidance_is_explicit_finite_and_requires_a_direction() {
        for scale in ["NaN", "inf", "0", "-1", "three"] {
            let mut settings = breeze_settings();
            settings.push(("FACULTIES_BREEZE_CFG_SCALE", scale.into()));
            assert!(parse_settings(&settings).is_err(), "{scale}");
        }
        let mut settings = breeze_settings();
        settings.push(("FACULTIES_BREEZE_CFG_SCALE", "3".into()));
        assert!(parse_settings(&settings).is_err());
        settings.push(("FACULTIES_BREEZE_DIRECTION", "Speak softly.".into()));
        let ModelSources::Breeze(source) = parse_settings(&settings).unwrap() else {
            unreachable!()
        };
        assert_eq!(source.cfg_scale, 3.0);
        assert_eq!(source.direction.as_deref(), Some("Speak softly."));
    }

    #[test]
    fn resident_pcm_matches_existing_header_and_clamps_finite_samples() {
        let audio = AudioClip::from_samples(&[-2.0, 0.0, 2.0], 24_000).unwrap();
        assert_eq!(reference_metadata(audio.wav.as_ref()).unwrap(), (3, 24_000));
        assert_eq!(&audio.wav.as_ref()[44..], &[1, 128, 0, 0, 255, 127]);
        assert!(AudioClip::from_samples(&[f32::NAN], 24_000).is_err());
        assert!(AudioClip::from_samples(&[], 24_000).is_err());
        assert!(AudioClip::from_samples(&[0.0], 0).is_err());
    }
    #[test]
    fn malformed_reference_is_rejected_without_panicking() {
        let audio = AudioClip::from_samples(&[0.0; 8], 24_000).unwrap();
        for length in 0..audio.wav.len() {
            assert!(reference_metadata(&audio.wav.as_ref()[..length]).is_err());
        }
        let mut stereo = audio.wav.as_ref().to_vec();
        stereo[22] = 2;
        assert!(reference_metadata(&stereo).is_err());
    }

    #[test]
    fn reference_requires_24_khz_before_model_acquisition() {
        for rate in [16_000, 22_050, 44_100, 48_000] {
            let audio = AudioClip::from_samples(&[0.0; 8], rate).unwrap();
            let error = reference_metadata(audio.wav.as_ref()).unwrap_err();
            assert!(error.to_string().contains("24 kHz"));
        }
        let audio = AudioClip::from_samples(&[0.0; 8], 24_000).unwrap();
        assert_eq!(reference_metadata(audio.wav.as_ref()).unwrap(), (8, 24_000));
    }

    #[test]
    #[cfg(feature = "voice")]
    fn reference_code_preflight_reuses_mary_decoder_without_loading_a_model() {
        use mary::models::qwen3tts::config::NUM_CODE_GROUPS;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reference.npy");
        let missing = preflight_reference_codes(&path).unwrap_err();
        assert_eq!(
            missing.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound,
        );
        mary::nn::npy::save_npy(
            &path,
            &vec![0.0; 2 * NUM_CODE_GROUPS],
            &[2, NUM_CODE_GROUPS],
        )
        .unwrap();
        preflight_reference_codes(&path).unwrap();
        for (data, shape) in [
            (Vec::new(), vec![0, NUM_CODE_GROUPS]),
            (vec![0.0; NUM_CODE_GROUPS], vec![NUM_CODE_GROUPS]),
            (vec![0.0; NUM_CODE_GROUPS - 1], vec![1, NUM_CODE_GROUPS]),
            (vec![0.0; NUM_CODE_GROUPS], vec![1, NUM_CODE_GROUPS - 1]),
        ] {
            mary::nn::npy::save_npy(&path, &data, &shape).unwrap();
            assert!(preflight_reference_codes(&path).is_err());
        }
        // Exercise the existing decoder's magic assertion, not a copied parser.
        std::fs::write(&path, b"not-an-npy-file").unwrap();
        let error = preflight_reference_codes(&path).unwrap_err();
        assert!(error.to_string().contains("decoder rejected"));
    }
}
