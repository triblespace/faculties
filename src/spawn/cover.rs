//! Best-effort cover sizing: `--cover PERCENT` of the worker's context window.
//!
//! The percent is a wish for the whole cover (memories, beliefs and goals),
//! never an allocation contract. Goals always render whole and
//! the brief is never cut, so a cover may come out larger than its target.
//! The one real limit is the window `W`, and every figure set against it here
//! is an estimate: no local tokenizer exists for the hosted providers, so
//! characters are converted at a calibrated characters-per-token ratio.
//!
//! This module is the pure part: where `W` comes from, the ratio, the
//! arithmetic, and reading what a provider reported after a run. The launch
//! that renders and records it is in [`super::operations`].

use std::path::Path;

/// Shared Memory and Wiki characters of a launch without `--cover`, and of one with
/// `--cover` that found no window: the former 200k wake plus 40k tail,
/// spent on one tinted render.
pub const PLAIN_MEMORY_CHARS: usize = 240_000;

/// The smallest memory budget the target split gives, however little of
/// the target the fixed sections leave: 40k characters, the size of the
/// former task tail, a coarse recollection of the whole life, about 15k
/// tokens at the claude ratio. Going over the target for it is normal, not
/// a failure. It is not a floor on the cover: when the whole prompt is
/// estimated above the headroom line, the re-render may give memory less,
/// down to nothing. A starting value, not settled policy.
pub const MIN_MEMORY_CHARS: usize = 40_000;

/// The fraction of `W` the launch leaves for the work: when the whole
/// prompt is estimated above `W × (1 − HEADROOM)`, the memory part is
/// rendered once more with a budget aimed at that line. A starting value,
/// not settled policy; each run records the value it used.
pub const HEADROOM: f64 = 0.25;

/// The context window spawn itself configures for a mistral run
/// (`max_context_length` in the per-run vibe configuration).
pub const MISTRAL_CONTEXT_TOKENS: u64 = 1_048_576;

/// A ratio for a model with no calibration: below every measured value, so
/// a cover sized with it errs small.
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 2.0;

/// An estimate of claude's fixed overhead (system prompt, tool schemas):
/// the largest first-call `cache_read_input_tokens` over 138 spawn runs of
/// claude-opus-5-5 on CLI 2.1.312, 16 to 20 Nov 2026 (11,266 to 13,059).
/// Those counts bound what the first call read from cache; they do not say
/// what it was, so taking them for the harness alone is an assumption.
pub const CLAUDE_OVERHEAD_TOKENS: u64 = 13_100;

/// Harness overhead assumed for a provider with no measurement.
pub const DEFAULT_OVERHEAD_TOKENS: u64 = 16_000;

/// A characters-per-token estimate and what it rests on.
#[derive(Clone, Debug, PartialEq)]
pub struct Calibration {
    pub chars_per_token: f64,
    /// Measured runs behind it; `None` for a stated default.
    pub n: Option<u64>,
    pub basis: String,
}

/// The ratio for a provider and model. Measured: claude-opus-5-5, the p5 of
/// prompt characters over first-call uncached input tokens across 138 spawn
/// runs (median 2.709, p95 2.742). The low tail is used so an estimate errs
/// toward more tokens. Anything else gets [`DEFAULT_CHARS_PER_TOKEN`].
pub fn calibration(provider: &str, model: Option<&str>) -> Calibration {
    match (provider, model) {
        ("claude", Some("claude-opus-5-5")) => Calibration {
            chars_per_token: 2.423,
            n: Some(138),
            basis: "estimate: p5 of 138 claude-opus-5-5 spawn runs, 16-20 Nov 2026 (median 2.709)"
                .to_owned(),
        },
        _ => Calibration {
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
            n: None,
            basis: format!(
                "no calibration for {provider} {}; conservative default {DEFAULT_CHARS_PER_TOKEN}",
                model.unwrap_or("(no model named)")
            ),
        },
    }
}

/// The harness overhead in tokens the estimate adds for a provider, and
/// what that figure rests on.
pub fn overhead(provider: &str) -> (u64, &'static str) {
    match provider {
        "claude" => (
            CLAUDE_OVERHEAD_TOKENS,
            "estimated from first-call cache reads",
        ),
        _ => (DEFAULT_OVERHEAD_TOKENS, "assumed, not measured"),
    }
}

/// A context window in tokens and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub tokens: u64,
    pub source: String,
}

/// codex's own model catalog (`models_cache.json`): the model's
/// `context_window` × `effective_context_window_percent`. `Err` says why the
/// catalog gives none.
pub fn codex_catalog_window(path: &Path, model: &str) -> Result<Window, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("codex model catalog {}: {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("codex model catalog {}: {error}", path.display()))?;
    let entry = value["models"]
        .as_array()
        .and_then(|models| models.iter().find(|entry| entry["slug"] == model))
        .ok_or_else(|| format!("codex model catalog {} has no {model}", path.display()))?;
    let window = entry["context_window"]
        .as_u64()
        .filter(|window| *window > 0)
        .ok_or_else(|| format!("codex model catalog names no context_window for {model}"))?;
    let percent = entry["effective_context_window_percent"]
        .as_u64()
        .filter(|percent| (1..=100).contains(percent))
        .unwrap_or(100);
    Ok(Window {
        tokens: window * percent / 100,
        source: format!(
            "codex model catalog {} ({model}: {window} x {percent}%, fetched {})",
            path.display(),
            value["fetched_at"].as_str().unwrap_or("at an unknown time")
        ),
    })
}

/// How one launch sized its cover, for the run record.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverRecord {
    pub percent: f64,
    /// `None` when no source gave a window; `window_source` says why.
    pub window: Option<u64>,
    pub window_source: String,
    /// The ratio converted with; `None` without a window, when nothing was
    /// converted.
    pub calibration: Option<Calibration>,
    pub target_chars: Option<usize>,
    /// Fixed sections and diagnostics measured with no recall budget.
    pub fixed_chars: Option<usize>,
    /// Character allowance shared by narrative and Wiki in this wake.
    pub memory_budget: usize,
    /// The delivered wake's characters.
    pub cover_chars: usize,
    /// The whole prompt's estimated tokens, overhead included.
    pub estimate: Option<u64>,
    pub headroom: Option<f64>,
    pub reduction: Option<String>,
    /// All of it in words, as the launch printed it.
    pub summary: String,
}

/// A cover percent must be a number above 0 and at most 100.
pub fn check_percent(percent: f64) -> anyhow::Result<f64> {
    if percent.is_finite() && percent > 0.0 && percent <= 100.0 {
        Ok(percent)
    } else {
        anyhow::bail!("cover percent {percent} is not above 0 and at most 100")
    }
}

/// The whole cover's target in characters.
pub fn target_chars(percent: f64, window: u64, chars_per_token: f64) -> usize {
    (percent / 100.0 * window as f64 * chars_per_token).round() as usize
}

/// The memory budget a target splits off: what fixed sections leave of
/// it, and at least [`MIN_MEMORY_CHARS`]. The estimate check after
/// rendering may still lower it.
pub fn memory_budget(target_chars: usize, fixed_chars: usize) -> usize {
    target_chars
        .saturating_sub(fixed_chars)
        .max(MIN_MEMORY_CHARS)
}

/// Estimated tokens of a prompt of `chars` characters, overhead included.
pub fn estimate_tokens(chars: usize, chars_per_token: f64, overhead: u64) -> u64 {
    (chars as f64 / chars_per_token).ceil() as u64 + overhead
}

/// The estimate the launch aims the whole prompt at: `W × (1 − HEADROOM)`.
pub fn aim_tokens(window: u64) -> u64 {
    (window as f64 * (1.0 - HEADROOM)).floor() as u64
}

/// The memory budget of the one re-render: the memory characters actually
/// rendered, less the estimated excess over [`aim_tokens`] converted back to
/// characters. Zero when the excess is larger than the memory part.
pub fn reduced_budget(memory_chars: usize, estimate: u64, aim: u64, chars_per_token: f64) -> usize {
    let excess = (estimate.saturating_sub(aim) as f64 * chars_per_token).ceil() as usize;
    memory_chars.saturating_sub(excess)
}

/// What claude's `--output-format json` result says. The report is its
/// `result` text, so the recorded report reads as the text format's did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeResult {
    pub report: Option<String>,
    pub context_window: Option<u64>,
    pub session_id: Option<String>,
}

/// claude's result object, or `None` when the bytes are not one (a crash,
/// partial output, or a run started before the switch to JSON): the caller
/// then records the bytes as they are. The window is the entry for `model`
/// in `modelUsage`; with no entry under that exact key it stays unknown.
pub fn claude_result(bytes: &[u8], model: Option<&str>) -> Option<ClaudeResult> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if value["type"] != "result" {
        return None;
    }
    // Only the run's own model's entry: a window filed under another key
    // would later size a launch of a model it does not belong to.
    let entry = model.and_then(|model| value["modelUsage"].get(model));
    Some(ClaudeResult {
        report: value["result"].as_str().map(str::to_owned),
        context_window: entry
            .and_then(|entry| entry["contextWindow"].as_u64())
            .filter(|window| *window > 0),
        session_id: value["session_id"].as_str().map(str::to_owned),
    })
}

/// The input tokens of the first model call in a claude session
/// transcript: uncached, cache-written and cache-read together. Reads lines
/// only up to that call, so a long transcript is never held whole.
pub fn claude_first_call<L: AsRef<str>>(lines: impl IntoIterator<Item = L>) -> Option<u64> {
    lines.into_iter().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line.as_ref()).ok()?;
        if value["type"] != "assistant" {
            return None;
        }
        let usage = value["message"]["usage"].as_object()?;
        let count = |field: &str| usage.get(field).and_then(|value| value.as_u64());
        let total = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .iter()
        .filter_map(|field| count(field))
        .sum::<u64>();
        (total > 0).then_some(total)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_split_gives_memory_what_beliefs_and_goals_leave_and_at_least_the_minimum() {
        // 20% of a 1M window at the claude ratio.
        let target = target_chars(20.0, 1_000_000, 2.423);
        assert_eq!(target, 484_600);
        assert_eq!(memory_budget(target, 270_000), 214_600);
        // Beliefs larger than the whole target still leave the minimum.
        assert_eq!(memory_budget(target, 600_000), MIN_MEMORY_CHARS);
        assert_eq!(memory_budget(0, 0), MIN_MEMORY_CHARS);
    }

    #[test]
    fn a_percent_outside_0_to_100_is_refused() {
        for bad in [0.0, -1.0, 100.5, f64::NAN, f64::INFINITY] {
            assert!(check_percent(bad).is_err(), "{bad}");
        }
        assert_eq!(check_percent(100.0).unwrap(), 100.0);
        assert_eq!(check_percent(12.5).unwrap(), 12.5);
    }

    #[test]
    fn the_reduction_aims_the_estimate_at_the_headroom() {
        let ratio = 2.0;
        let window = 100_000;
        let aim = aim_tokens(window);
        assert_eq!(aim, 75_000);
        // 10k tokens over the aim are 20k characters of memory.
        assert_eq!(reduced_budget(50_000, 85_000, aim, ratio), 30_000);
        // An excess larger than the memory part leaves an empty one.
        assert_eq!(reduced_budget(10_000, 200_000, aim, ratio), 0);
        assert_eq!(estimate_tokens(10, 2.0, 100), 105);
    }

    #[test]
    fn only_claude_opus_5_5_is_calibrated() {
        let claude = calibration("claude", Some("claude-opus-5-5"));
        assert_eq!((claude.chars_per_token, claude.n), (2.423, Some(138)));
        let codex = calibration("codex", Some("gpt-5.5"));
        assert_eq!(
            (codex.chars_per_token, codex.n),
            (DEFAULT_CHARS_PER_TOKEN, None)
        );
        assert!(codex.basis.contains("no calibration"));
    }

    #[test]
    fn the_codex_catalog_gives_the_effective_window_of_the_named_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models_cache.json");
        std::fs::write(
            &path,
            r#"{"fetched_at":"2026-10-02T16:49:47Z","models":[
                {"slug":"gpt-5.5","context_window":272000,"effective_context_window_percent":95}]}"#,
        )
        .unwrap();
        let window = codex_catalog_window(&path, "gpt-5.5").unwrap();
        assert_eq!(window.tokens, 258_400);
        assert!(window.source.contains("fetched 2026-10-02"));
        assert!(codex_catalog_window(&path, "gpt-9").is_err());
        assert!(codex_catalog_window(&dir.path().join("missing"), "gpt-5.5").is_err());
    }

    #[test]
    fn claudes_result_gives_the_report_text_window_and_session() {
        let json = br#"{"type":"result","subtype":"success","is_error":false,
            "result":"the answer\n","session_id":"e0bf4238-7a93-4604-be3f-a4ddb456d86b",
            "modelUsage":{"claude-opus-5-5":{"inputTokens":2,"contextWindow":1000000}}}"#;
        let result = claude_result(json, Some("claude-opus-5-5")).unwrap();
        assert_eq!(result.report.as_deref(), Some("the answer\n"));
        assert_eq!(result.context_window, Some(1_000_000));
        assert_eq!(
            result.session_id.as_deref(),
            Some("e0bf4238-7a93-4604-be3f-a4ddb456d86b")
        );
        // A window under another model's key, even the only one, is not
        // this run's: it stays unknown, the report still reads.
        let other = claude_result(json, Some("claude-sonnet-5-5")).unwrap();
        assert_eq!(other.context_window, None);
        assert_eq!(other.report.as_deref(), Some("the answer\n"));
        assert_eq!(claude_result(json, None).unwrap().context_window, None);
        // Text, partial output, and other JSON are not a result.
        assert!(claude_result(b"plain words", None).is_none());
        assert!(claude_result(br#"{"type":"result","resu"#, None).is_none());
        assert!(claude_result(br#"{"type":"system"}"#, None).is_none());
    }

    #[test]
    fn the_first_call_counts_every_input_kind_of_the_first_assistant_line() {
        let transcript = concat!(
            r#"{"type":"user","message":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":192066,"cache_read_input_tokens":11266,"output_tokens":250}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":203334}}}"#,
            "\n"
        );
        assert_eq!(claude_first_call(transcript.lines()), Some(203_334));
        assert_eq!(claude_first_call("".lines()), None);
    }
}
