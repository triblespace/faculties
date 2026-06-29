//! Token-count estimation for memory/context budgeting.
//!
//! A token budget only means something relative to a tokenizer, and different
//! models tokenize differently. [`TokenEstimator`] lets a budget be interpreted
//! through whichever estimator matches the model that will consume the result:
//! a cheap chars-per-token heuristic for API models (which expose no local
//! tokenizer), or — opt-in, behind the `hf-tokenizer` feature — an exact
//! tokenizer for local models like gemma.

/// Estimates how many tokens a string costs, for budget math.
#[derive(Clone, Debug)]
pub enum TokenEstimator {
    /// `chars / ratio` — the model-agnostic heuristic. `ratio` is the per-model
    /// knob (≈4 for most models; the playground's `DEFAULT_PROMPT_CHARS_PER_TOKEN`).
    /// Dependency-free; as granular as we can honestly be for an API model that
    /// ships no public tokenizer.
    CharsPerToken(u32),
    // The exact variant for local models (a HuggingFace `tokenizer.json`, e.g.
    // gemma) belongs here, behind an `hf-tokenizer` feature so the `tokenizers`
    // dependency stays opt-in rather than burdening every faculty:
    //
    //     #[cfg(feature = "hf-tokenizer")]
    //     HuggingFace(Box<tokenizers::Tokenizer>),
}

impl TokenEstimator {
    /// Estimate the token cost of `text`.
    pub fn estimate(&self, text: &str) -> usize {
        match self {
            TokenEstimator::CharsPerToken(ratio) => text.chars().count() / (*ratio).max(1) as usize,
        }
    }

    /// Read the configured estimator from the environment, falling back to the
    /// default. `MEMORY_CHARS_PER_TOKEN=<n>` overrides the heuristic ratio.
    pub fn from_env() -> Self {
        if let Ok(raw) = std::env::var("MEMORY_CHARS_PER_TOKEN") {
            if let Ok(ratio) = raw.parse::<u32>() {
                return TokenEstimator::CharsPerToken(ratio.max(1));
            }
        }
        TokenEstimator::default()
    }
}

impl Default for TokenEstimator {
    /// Matches the playground's `DEFAULT_PROMPT_CHARS_PER_TOKEN` (4).
    fn default() -> Self {
        TokenEstimator::CharsPerToken(4)
    }
}
