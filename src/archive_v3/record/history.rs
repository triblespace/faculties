//! The history a Codex client keeps of tool outputs (X18, X18'): the
//! budget it truncates each output to, as codex-rs renders the truncation,
//! and the budgets its clients could have cut an output under where the
//! rollout records none. The truncation is codex-rs behaviour, not a field,
//! so it is a rule here that the table's [`HistoryBudget`] refers to: the
//! table says where the policy, a record's own limit, the outputs and the
//! client versions are, and this says what her window held. The Codex
//! importer and the relations (`grammar::relations`) both decide by it;
//! neither owns it.

use super::json::Json;
use super::Failure;
use crate::archive_v3::fields::HistoryBudget;

/// A tool output's history budget: in tokens, or in bytes (codex-rs
/// protocol/src/protocol.rs, `TruncationPolicy`).
#[derive(Clone, Copy)]
pub(crate) enum Budget {
    Tokens(usize),
    Bytes(usize),
}

/// Codex's four bytes per token, rounded up (codex-rs
/// utils/string/src/truncate.rs, `approx_token_count` and
/// `approx_tokens_from_byte_count`).
fn approx_tokens(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

impl Budget {
    /// A header's `truncation_policy`, which must be exactly `{mode, limit}`
    /// with `mode` `bytes` or `tokens` and `limit` a count (serde's
    /// adjacently tagged `TruncationPolicy`), with codex's 20%
    /// serialization allowance (`with_serialization_allowance`:
    /// `(limit as f64 * 1.2).ceil()`). Anything else is no budget the
    /// importer can apply, and fails (X18).
    fn of_policy(policy: &Json) -> Option<Budget> {
        let Json::Obj(members) = policy else {
            return None;
        };
        if members.len() != 2 {
            return None;
        }
        let limit = match policy.get("limit") {
            Some(Json::Num(token)) if token.bytes().all(|b| b.is_ascii_digit()) => {
                token.parse::<usize>().ok()?
            }
            _ => return None,
        };
        let allowed = (limit as f64 * 1.2).ceil() as usize;
        match policy.get("mode") {
            Some(Json::Str(mode)) if mode == "tokens" => Some(Budget::Tokens(allowed)),
            Some(Json::Str(mode)) if mode == "bytes" => Some(Budget::Bytes(allowed)),
            _ => None,
        }
    }

    /// A per-record override, in tokens, allowance included (codex-rs
    /// history/src/lib.rs, `history_truncation_token_limit`, serialized as
    /// `fallback_token_limit_override`).
    fn of_override(limit: &Json) -> Option<Budget> {
        match limit {
            Json::Num(token) if token.bytes().all(|b| b.is_ascii_digit()) => {
                token.parse::<usize>().ok().map(Budget::Tokens)
            }
            _ => None,
        }
    }

    /// The budget a header records at the table's `policy` field: none
    /// where it records none (X18).
    pub(crate) fn of_header(
        table: &HistoryBudget,
        header: &Json,
    ) -> Result<Option<Budget>, Failure> {
        let (field, path) = table.policy;
        match header.at(field) {
            None => Ok(None),
            Some(policy) => Ok(Some(Budget::of_policy(policy).ok_or_else(|| {
                Failure::new(
                    path,
                    "a history budget that does not parse: the importer cannot apply it",
                )
            })?)),
        }
    }

    /// The budget of a record: its own limit at the table's `limit` field,
    /// else its header's.
    pub(crate) fn of_record(
        table: &HistoryBudget,
        record: &Json,
        header: Option<Budget>,
    ) -> Result<Option<Budget>, Failure> {
        let (field, path) = table.limit;
        match record.at(field) {
            None => Ok(header),
            Some(limit) => Ok(Some(Budget::of_override(limit).ok_or_else(|| {
                Failure::new(path, "a history budget override that is no count")
            })?)),
        }
    }

    fn limit(self) -> usize {
        match self {
            Budget::Tokens(limit) | Budget::Bytes(limit) => limit,
        }
    }

    /// What codex's history holds of a text output: `truncate_text`
    /// (codex-rs utils/output-truncation/src/lib.rs), which is
    /// `truncate_middle_chars` under a byte budget and
    /// `truncate_middle_with_token_budget` (0.78 to 0.132; its successor
    /// `plan_token_truncation` renders the same) under a token budget
    /// (utils/string/src/truncate.rs).
    fn truncate(self, text: &str) -> String {
        match self {
            Budget::Bytes(bytes) => middle(text, bytes, false),
            Budget::Tokens(tokens) => {
                if text.is_empty() {
                    return String::new();
                }
                let bytes = tokens.saturating_mul(4);
                if tokens > 0 && text.len() <= bytes {
                    return text.to_owned();
                }
                middle(text, bytes, true)
            }
        }
    }

    /// Whether codex's history shortens a list output: whether its
    /// `truncate_function_output_items_with_policy` (codex-rs
    /// core/src/truncate.rs at rust-v0.78.0 to 0.114.0, then
    /// utils/output-truncation/src/lib.rs at 0.117.0 to 0.132.0: the same
    /// rule at each release tag checked of the clients that record a budget)
    /// changes any part her window holds. A non-empty text part that no longer fits is cut; once
    /// the budget is spent, every text part, an empty one too, is omitted
    /// and counted in a notice; images cost nothing.
    fn shortens(self, parts: &[Json]) -> bool {
        let mut remaining = self.limit();
        for part in parts {
            if part.str_at("type") != Some("input_text") {
                continue;
            }
            let text = part.str_at("text").unwrap_or_default();
            if remaining == 0 {
                return true;
            }
            let cost = match self {
                Budget::Tokens(_) => approx_tokens(text.len()),
                Budget::Bytes(_) => text.len(),
            };
            if cost > remaining {
                return true;
            }
            remaining -= cost;
        }
        false
    }
}

/// X18': the budgets a client may have cut a tool output under where its
/// rollout records none. Clients 0.51 to 0.77 and 0.133 on truncate her
/// history without recording the budget, so where an output exceeds the
/// smallest budget its client's generation could have run under, the
/// window she saw is unknown and the unit fails (Open). Within it, every
/// such budget leaves the output whole.
///
/// - Before 0.51 the history keeps what it is given (codex-rs 0.50.0
///   core/src/conversation_history.rs, `record_items`): the recorded
///   output is what she saw.
/// - 0.51 to 0.58 cut a text at 10 KiB or 256 lines
///   (`format_output_for_model_body`, `MODEL_FORMAT_MAX_BYTES`,
///   `MODEL_FORMAT_MAX_LINES`, core/src/conversation_history.rs at 0.51.0
///   to 0.54.0, then context_manager); a list's text parts each so at 0.51
///   to 0.53, and together at 10 KiB at 0.54 on
///   (`globally_truncate_function_output_items`).
/// - 0.59 to 0.77 and 0.133 on cut under the model's policy times 1.2
///   (`process_item`, `policy.mul(1.2)`, core/src/context_manager/
///   history.rs at 0.59.0 to 0.77.0; `record_items`, `policy * 1.2`, at
///   0.133.0 to 0.160.0). The smallest documented policy is Bytes(10_000)
///   (core/src/model_family.rs at 0.77.0; models-manager models.json at
///   0.133.0, 0.140.0 and 0.150.0, where every other model has
///   Tokens(10_000)), so 12,000 bytes; the token policy is checked too. A
///   config override (`tool_output_token_limit`) could set any smaller
///   budget and is not recorded either; it is not modelled.
/// - 0.78 to 0.132 record the policy on every header; one that records
///   none is treated as the model's policy.
/// - A version that does not parse, `0.0.0` (a source build), or no
///   `session_meta` at all: any generation.
#[derive(Clone, Copy, Default)]
pub(crate) struct Unrecorded {
    /// 0.51 to 0.58: 10 KiB or 256 lines.
    fixed: bool,
    /// 0.59 on: the model's policy, at least Bytes(12_000).
    model: bool,
}

impl Unrecorded {
    const FIXED_BYTES: usize = 10 * 1024;
    const FIXED_LINES: usize = 256;
    /// The smallest documented model policy, Bytes(10_000), with codex's
    /// 1.2 allowance; Tokens(10_000) likewise.
    const MODEL: [Budget; 2] = [Budget::Bytes(12_000), Budget::Tokens(12_000)];

    /// The unit's clients, by the versions of the records the table's
    /// `client` names.
    pub(crate) fn of_clients(table: &HistoryBudget, records: &[Json]) -> Self {
        let ((field, client), version) = table.client;
        let versions = records
            .iter()
            .filter(|record| record.str_at(field) == Some(client))
            .map(|record| record.str_at(version));
        let mut out = Unrecorded::default();
        let mut any = false;
        for version in versions {
            any = true;
            let minor = version.and_then(|version| {
                let mut parts = version.split('.');
                let major = parts.next()?.parse::<u64>().ok()?;
                let minor = parts.next()?;
                let digits: String = minor.chars().take_while(char::is_ascii_digit).collect();
                let minor = digits.parse::<u64>().ok()?;
                Some((major, minor))
            });
            match minor {
                Some((0, 0)) | None => {
                    out.fixed = true;
                    out.model = true;
                }
                Some((0, minor)) if minor < 51 => {}
                Some((0, minor)) if minor <= 58 => out.fixed = true,
                Some(_) => out.model = true,
            }
        }
        if !any {
            out.fixed = true;
            out.model = true;
        }
        out
    }

    fn fixed_cuts(text: &str) -> bool {
        text.len() > Self::FIXED_BYTES || text.lines().count() > Self::FIXED_LINES
    }

    /// Whether some budget the unit's clients could have run under cuts
    /// this text output.
    fn cuts_text(self, text: &str) -> bool {
        (self.fixed && Self::fixed_cuts(text))
            || (self.model
                && Self::MODEL
                    .iter()
                    .any(|budget| budget.truncate(text) != text))
    }

    /// Whether some such budget changes this list output.
    fn cuts_parts(self, parts: &[Json]) -> bool {
        let texts = || {
            parts
                .iter()
                .filter(|part| part.str_at("type") == Some("input_text"))
                .map(|part| part.str_at("text").unwrap_or_default())
        };
        (self.fixed
            && (texts().any(Self::fixed_cuts) || Budget::Bytes(Self::FIXED_BYTES).shortens(parts)))
            || (self.model && Self::MODEL.iter().any(|budget| budget.shortens(parts)))
    }
}

/// X18, X18': the history one tool output entered her window through: its
/// record's budget, the budgets its clients could have cut it under
/// unrecorded, and whether its client recorded it as it entered her
/// history.
#[derive(Clone, Copy)]
pub(crate) struct History {
    pub(crate) budget: Option<Budget>,
    pub(crate) unrecorded: Unrecorded,
    pub(crate) recorded_here: bool,
}

impl History {
    /// X18': where an output was recorded as it entered her history and no
    /// budget is recorded, one her client could have cut fails.
    fn unknown(self, cut: bool, path: &str) -> Result<(), Failure> {
        if self.recorded_here && self.budget.is_none() && cut {
            return Err(Failure::new(
                path,
                "an output a budget her client could have run under cuts, and the \
                 rollout records none: the truncation is unrecorded, so what her \
                 window held is unknown (X18')",
            ));
        }
        Ok(())
    }

    /// What her window held of a text output: the text as the client's
    /// history truncated it under the budget.
    pub(crate) fn text(self, text: &str, path: &str) -> Result<String, Failure> {
        self.unknown(self.unrecorded.cuts_text(text), path)?;
        Ok(match self.budget {
            Some(budget) => budget.truncate(text),
            None => text.to_owned(),
        })
    }

    /// A list output, before its parts are read (X18').
    pub(crate) fn list(self, parts: &[Json], path: &str) -> Result<(), Failure> {
        self.unknown(self.unrecorded.cuts_parts(parts), path)
    }

    /// A list output, after its parts are read: one the budget shortens
    /// fails, since how to keep its full parts beside her window is not
    /// modeled.
    pub(crate) fn list_whole(self, parts: &[Json], path: &str) -> Result<(), Failure> {
        if self.budget.is_some_and(|budget| budget.shortens(parts)) {
            return Err(Failure::new(
                path,
                "a list output its history budget shortens: how to keep its \
                 full parts beside her window is not modeled",
            ));
        }
        Ok(())
    }
}

/// `truncate_with_byte_estimate` (codex-rs utils/string/src/truncate.rs at
/// rust-v0.120.0 and 0.132.0, core/src/truncate.rs at 0.78.0 to 0.114.0,
/// the same rule), which `truncate_middle_chars` at main (71a4c070)
/// still renders: at most `max_bytes` of the
/// text's two ends, split evenly on character boundaries, around a marker
/// that counts what was cut, in tokens or in characters.
fn middle(text: &str, max_bytes: usize, tokens: bool) -> String {
    if text.is_empty() {
        return String::new();
    }
    let marker = |count: usize| {
        if tokens {
            format!("\u{2026}{count} tokens truncated\u{2026}")
        } else {
            format!("\u{2026}{count} chars truncated\u{2026}")
        }
    };
    if max_bytes == 0 {
        return marker(if tokens {
            approx_tokens(text.len())
        } else {
            text.chars().count()
        });
    }
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let left = max_bytes / 2;
    let right = max_bytes - left;
    // `split_string`: the prefix is the characters that end within `left`,
    // the suffix starts at the first character at or after the tail.
    let tail = text.len().saturating_sub(right);
    let (mut prefix_end, mut suffix_start, mut removed, mut started) =
        (0usize, text.len(), 0usize, false);
    for (at, c) in text.char_indices() {
        let end = at + c.len_utf8();
        if end <= left {
            prefix_end = end;
            continue;
        }
        if at >= tail {
            if !started {
                suffix_start = at;
                started = true;
            }
            continue;
        }
        removed += 1;
    }
    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }
    let count = if tokens {
        approx_tokens(text.len() - max_bytes)
    } else {
        removed
    };
    format!(
        "{}{}{}",
        &text[..prefix_end],
        marker(count),
        &text[suffix_start..]
    )
}
