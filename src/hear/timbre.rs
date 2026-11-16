//! Timbre: whose voice a clip sounds like, as the distance between speaker
//! embeddings.
//!
//! The embedding is the x-vector of Qwen3-TTS-Base's ECAPA-TDNN speaker
//! encoder, the vector its talker is conditioned on when it clones a voice.
//! The encoder was trained to tell speakers apart, so it is built to carry
//! identity and to discard words; prosody (`hear prosody`) and the Gemma
//! hearing tower carry what is said and how, not whose voice it is. Breeze,
//! the live voice, has no speaker vector of its own: it clones from the
//! reference's codec codes in context.
//!
//! The weights are the voice's own: the Qwen3-TTS model pile the `voice`
//! faculty reads (`QWEN3TTS_PILE`, else `qwen3tts.pile` in the model
//! directory), selected the same way. Only the speaker encoder is built.
//! Clips must be 24 kHz, the encoder's rate; nothing is resampled.

use anyhow::{ensure, Result};

/// The rate the speaker encoder was trained at.
pub const RATE: u32 = 24_000;

/// Cosine distance, `1 - cos(a, b)`: 0 for the same direction, 1 for
/// orthogonal. Errors on a length mismatch or a zero vector rather than
/// inventing a distance.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> Result<f64> {
    ensure!(
        a.len() == b.len(),
        "embedding lengths differ: {} and {}",
        a.len(),
        b.len()
    );
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    ensure!(na > 0.0 && nb > 0.0, "an embedding is the zero vector");
    Ok(1.0 - dot / (na.sqrt() * nb.sqrt()))
}

/// The two halves of a clip, split at the sample midpoint: two takes of the
/// same voice with different words, so their distance is the noise floor of
/// the measure on that clip.
pub fn halves(samples: &[f32]) -> (&[f32], &[f32]) {
    samples.split_at(samples.len() / 2)
}

/// The shortest clip (or half) embedded: two seconds. The encoder pools over
/// time, and below this a distance says more about the pooling than the voice.
pub const MIN_SAMPLES: usize = 2 * RATE as usize;

#[cfg(feature = "voice")]
pub use encoder::Encoder;

#[cfg(feature = "voice")]
mod encoder {
    use super::{MIN_SAMPLES, RATE};
    use anyhow::{ensure, Context, Result};
    use mary::models::qwen3tts::speaker::{SpeakerEncoder, SpeakerMel};
    use mary::nn::backend::speak::{Device, Raw};
    use std::path::Path;

    /// The speaker encoder on the voice's backend (CUDA on Linux).
    pub struct Encoder {
        encoder: SpeakerEncoder<Raw>,
        mel: SpeakerMel<Raw>,
        device: Device,
    }

    impl Encoder {
        /// Select the configured Qwen3-TTS cohort in `pile` and build its
        /// speaker encoder alone.
        pub fn open(pile: &Path) -> Result<Self> {
            let device = Device::default();
            let encoder = crate::model_storage::with_snapshot(pile, "Qwen3-TTS", |snapshot| {
                let weights = mary::speak::Qwen3TtsWeights::from_snapshot(
                    snapshot.clone(),
                    mary::speak::Qwen3TtsVariant::from_env(),
                )
                .context("select the native Qwen3-TTS cohort")?;
                Ok(weights.speaker_encoder::<Raw>(&device))
            })?;
            let mel = SpeakerMel::new(&device);
            Ok(Self {
                encoder,
                mel,
                device,
            })
        }

        /// The x-vector of 24 kHz mono samples.
        pub fn embed(&self, samples: &[f32]) -> Result<Vec<f32>> {
            ensure!(
                samples.len() >= MIN_SAMPLES,
                "{} samples is under the {MIN_SAMPLES} ({} s) the encoder is given",
                samples.len(),
                MIN_SAMPLES / RATE as usize
            );
            let mel = self.mel.forward(samples, &self.device);
            let vector = self.encoder.forward(mel).into_data();
            let vector = vector
                .to_vec::<f32>()
                .map_err(|error| anyhow::anyhow!("read the x-vector: {error:?}"))?;
            ensure!(
                vector.iter().all(|v| v.is_finite()),
                "the x-vector is not finite"
            );
            Ok(vector)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_is_zero_for_scaled_copies_and_one_for_orthogonal() {
        let a = [1.0, 2.0, 3.0];
        assert!(cosine_distance(&a, &[2.0, 4.0, 6.0]).unwrap().abs() < 1e-12);
        assert!((cosine_distance(&[1.0, 0.0], &[0.0, 5.0]).unwrap() - 1.0).abs() < 1e-12);
        assert!((cosine_distance(&[1.0, 0.0], &[-1.0, 0.0]).unwrap() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn distance_refuses_mismatch_and_zero() {
        assert!(cosine_distance(&[1.0], &[1.0, 2.0]).is_err());
        assert!(cosine_distance(&[0.0, 0.0], &[1.0, 2.0]).is_err());
    }

    #[test]
    fn halves_split_at_the_midpoint() {
        let s = [1.0, 2.0, 3.0, 4.0, 5.0];
        let (a, b) = halves(&s);
        assert_eq!(a, &[1.0, 2.0]);
        assert_eq!(b, &[3.0, 4.0, 5.0]);
    }
}
