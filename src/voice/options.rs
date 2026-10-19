//! Request-local Breeze delivery. This type is available without model features
//! so transport validation never needs to acquire a model or open a pile.
use anyhow::{ensure, Result};
use clap::Args;
use serde::{Deserialize, Serialize};

#[derive(Args, Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SpeechOptions {
    /// Breeze delivery instruction, separate from spoken text. Omit to inherit
    /// the resident startup direction; blank is an error.
    #[arg(long, conflicts_with = "clear_direction")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// Clear the startup direction for this request; CFG defaults to 1.
    #[arg(long)]
    pub clear_direction: bool,
    /// Breeze guidance strength (finite and positive). Omit to inherit startup
    /// strength, except when clearing direction. Non-unit CFG needs direction.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cfg_scale: Option<f32>,
}

impl SpeechOptions {
    pub fn is_default(&self) -> bool {
        self.direction.is_none() && !self.clear_direction && self.cfg_scale.is_none()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.clear_direction || self.direction.is_none(),
            "direction and clear_direction cannot be combined"
        );
        if let Some(direction) = &self.direction {
            ensure!(
                !direction.trim().is_empty() && direction.len() <= 65536,
                "direction must contain 1..=65536 UTF-8 bytes and not be blank"
            );
        }
        if let Some(scale) = self.cfg_scale {
            ensure!(
                scale.is_finite() && scale > 0.0,
                "CFG scale must be finite and positive"
            );
            ensure!(
                !self.clear_direction || scale == 1.0,
                "non-unit CFG requires a direction; cannot clear it"
            );
        }
        Ok(())
    }

    #[cfg(feature = "voice")]
    pub(crate) fn native(&self) -> mary::speech_options::SpeechOptions {
        mary::speech_options::SpeechOptions {
            direction: self.direction.clone(),
            clear_direction: self.clear_direction,
            cfg_scale: self.cfg_scale,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "voice")]
    #[test]
    fn transport_and_mary_validation_agree() {
        for direction in [
            None,
            Some(""),
            Some(" \n"),
            Some("Warm (laugh)"),
            Some("Speak professionally."),
        ] {
            for clear_direction in [false, true] {
                for cfg_scale in [
                    None,
                    Some(0.0),
                    Some(-1.0),
                    Some(1.0),
                    Some(2.5),
                    Some(f32::NAN),
                    Some(f32::INFINITY),
                ] {
                    let options = SpeechOptions {
                        direction: direction.map(str::to_owned),
                        clear_direction,
                        cfg_scale,
                    };
                    assert_eq!(
                        options.validate().is_ok(),
                        options.native().validate().is_ok()
                    );
                }
            }
        }
    }

    #[test]
    fn options_are_validated_without_models() {
        SpeechOptions::default().validate().unwrap();
        for value in ["", " \n", &"x".repeat(65537)] {
            assert!(SpeechOptions {
                direction: Some(value.into()),
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for value in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(SpeechOptions {
                cfg_scale: Some(value),
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        assert!(SpeechOptions {
            clear_direction: true,
            cfg_scale: Some(2.0),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(SpeechOptions {
            clear_direction: true,
            direction: Some("Soft".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
    }
}
