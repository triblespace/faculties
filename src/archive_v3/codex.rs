//! The v3 Codex importer: one rollout file in, one [`Fragment`] out, or a
//! [`Failure`] that names a path and a reason (never a value).
//!
//! A unit is one `rollout-*.jsonl` file. Units are independent (model v3):
//! nothing here looks at another file or at a pile. A subagent's spawning
//! call lives in its parent's rollout, so the link is never computed here;
//! the provider's reference to the parent thread is kept as a pointer for
//! the join that will.
//!
//! The unit is decoded completely in memory and every record is checked
//! against the field table ([`crate::archive_v3::fields::codex`]) first: an
//! unknown path, kind or vocabulary value fails the unit, as does an open
//! field or a client copy (an event) whose original the unit does not hold.
//! Only then is anything built.
//!
//! # What is built
//!
//! Records are read in file order. A window is the settings header, the
//! system section, then the line:
//!
//! - `turn_context` sets the header: the effort she was sampled under, a
//!   `system` body gap that carries it (S1);
//! - `session_meta` sets the system section from its position on:
//!   `base_instructions` (or a `system` body gap before they were recorded),
//!   then a `tool-definition` span gap (tool definitions are never
//!   recorded). A fork replays its ancestors' `session_meta` after its own;
//!   where a replayed one's instructions differ from the unit's own, which
//!   records the fork wrote itself is unmarked, and the unit fails (X3);
//! - a response item extends the line, one node per fact;
//! - `compacted` starts a new line holding exactly what it installs (its
//!   replacement history, or a span gap and its summary); the first node of
//!   the new line that is not on the old line `continues` the old line's
//!   last node a record holds (X2);
//! - `task_started` marks a turn (where the unit marks none, his prompt
//!   after her answer starts one, X17), and `thread_rolled_back` cuts the
//!   line back to the start of its last turns: the next node branches
//!   there;
//! - `turn_aborted` and an erred `task_complete` put `ends` on the
//!   occurrence of the record that holds her last item of the turn, when
//!   she produced one, never on the node (X5, X6, M1). An abort, and an
//!   erred turn in which she produced nothing, are also a `turn_ends` claim
//!   of their record, on the turn's last node, or on the next node placed
//!   when the turn holds none (X15). An error whose `message` is missing or
//!   no string fails (X11, X15).
//!
//! When the header or the system section changes, the line is re-presented
//! under the new one. Items are known by their place in the source, never
//! by their content: a record's item re-presented at another position
//! `recalls` the node its record held, and two records that hold identical
//! content are two items (C5). Node ids being content addresses, shared
//! prefixes are shared nodes. A fork or resume replays its ancestor's
//! records, which therefore converge with the ancestor's own rollout by
//! structure.
//!
//! A result answers the call its correlator names on its own line; a call
//! the line no longer holds (one in a rolled-back turn, or before a
//! compaction) fails the unit (X4).
//!
//! Each node a record holds gets a witness and one occurrence holding that
//! record's claims. Why a sampling or turn ended annotates the occurrence
//! and is no part of its id (M2), so two records whose own claims coincide
//! on one node converge; where they ended differently the unit fails. A
//! record that holds no node (a header without effort, a
//! `session_meta` without instructions, an abort) gives its claims to the
//! next node the file places; where the file ends before that node, the
//! unit fails, since a snapshot cut there would give them to another node
//! than the whole file does (X19).
//! Reasoning summaries are glosses of the sealed reasoning, one per part
//! with its place (X16); the source does not name the model that wrote
//! them, so they name none. A tool output is what her live history held:
//! truncated under the recorded budget, with the full text a claim of its
//! record (X18). Where her message
//! holds no phase, its `agent_message` event's phase is its register (X1);
//! the event names its message by structure, as the first item after it
//! past her reasoning, never by text alone (X13). An empty sealed token is a
//! body gap (X7, X12), and a text part without its text fails (X14).

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use hifitime::Epoch;
use triblespace::core::blob::MemoryBlobStore;
use triblespace::core::import::json_tree::JsonTreeImporter;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

use crate::archive_v3::fields::codex::{OPENAI_ENCRYPTED_CONTENT, TABLE};
use crate::archive_v3::record::check::{self, base64_url, data_uri};
use crate::archive_v3::record::json::{self, Json};
use crate::archive_v3::record::Failure;
use crate::archive_v3::{
    annotate_node, ends_disagree, fact, gloss_part, node, occurrence, unexported, witness,
    FactCore, MediaType, NodeAnnotations, WitnessAnnotations,
};
use crate::schemas::archive_v3 as v3;
use crate::schemas::blockdag::content_fact::sealed;
use crate::schemas::blockdag::source_projection::SOURCE_CODEX;

#[cfg(test)]
mod tests;

/// Import one Codex rollout file.
pub fn import(source: &[u8]) -> Result<Fragment, Failure> {
    let mut records = Vec::new();
    for bytes in source.split(|byte| *byte == b'\n') {
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let record =
            json::parse(bytes).ok_or_else(|| Failure::new("<document>", "a line is not JSON"))?;
        records.push(record);
    }
    let mut unit = check::UnitMirrors::new(&TABLE);
    for record in &records {
        check::record_in(&TABLE, record, &mut unit)?;
    }
    unit.verify()?;
    let phases = supplied_phases(&records)?;
    let marked_turns = records.iter().any(|record| {
        record.str_at("type") == Some("event_msg")
            && record.str_at("payload.type") == Some("task_started")
    });
    // X18': every client that wrote or resumed the unit, by its version.
    let unrecorded = Unrecorded::of_clients(
        records
            .iter()
            .filter(|record| record.str_at("type") == Some("session_meta"))
            .map(|record| record.str_at("payload.cli_version")),
    );
    let mut builder = Builder::new(phases, marked_turns, unrecorded)?;
    for (at, record) in records.iter().enumerate() {
        builder.record(at, record)?;
    }
    builder.finish()
}

fn put_fact(facts: &mut HashMap<Id, Fragment>, core: FactCore, path: &str) -> Result<Id, Failure> {
    let built = fact(core).map_err(|_| Failure::new(path, "a fact does not build"))?;
    let id = built.root().expect("a fact is rooted");
    facts.entry(id).or_insert_with(|| unexported(built));
    Ok(id)
}

fn time(text: &str) -> Option<Inline<NsTAIInterval>> {
    let epoch = Epoch::from_str(text.trim()).ok()?;
    (epoch, epoch).try_to_inline().ok()
}

fn core(modality: Id, direction: Id) -> FactCore {
    FactCore {
        modality: Some(modality),
        direction: Some(direction),
        ..FactCore::default()
    }
}

fn structure(value: &Json, path: &str) -> Result<Fragment, Failure> {
    let mut text = String::new();
    value.write(&mut text);
    let mut store = MemoryBlobStore::new();
    let mut tree = JsonTreeImporter::new(&mut store, None)
        .import_str(&text)
        .map_err(|_| Failure::new(path, "tool JSON does not import as a json_tree"))?;
    tree.blobs_mut().union(store);
    Ok(tree)
}

/// A part's `text`, which must be there and be a string (X14: nothing is
/// skipped silently).
fn text_at<'a>(part: &'a Json, path: &str) -> Result<&'a str, Failure> {
    part.str_at("text")
        .ok_or_else(|| Failure::new(path, "a text part without its text"))
}

fn sealed_type(spelling: &str) -> MediaType {
    MediaType::parse(spelling).expect("the sealed media types are well formed")
}

fn sealed_blob(value: Option<&Json>, path: &str) -> Result<anybytes::Bytes, Failure> {
    let text = value
        .and_then(Json::str)
        .ok_or_else(|| Failure::new(path, "a sealed token is not a string"))?;
    base64_url(text)
        .map(anybytes::Bytes::from)
        .ok_or_else(|| Failure::new(path, "a sealed token does not decode"))
}

/// A sealed token as a blob of `media_type`, or, where the token is the
/// empty string, a body gap of the same fact (X7, X12).
fn sealed_or_gap(
    value: Option<&Json>,
    path: &str,
    media_type: &str,
    base: FactCore,
) -> Result<FactCore, Failure> {
    Ok(match value {
        Some(Json::Str(token)) if token.is_empty() => FactCore {
            absent: Some(v3::fact::absent::BODY),
            ..base
        },
        _ => FactCore {
            blob: Some(sealed_blob(value, path)?),
            media_type: Some(sealed_type(media_type)),
            ..base
        },
    })
}

/// An image she was shown: a `data:` URI, rendered under its detail.
fn image(part: &Json, path: &str) -> Result<FactCore, Failure> {
    use v3::fact::{direction::IN, modality::IMAGE};
    let url = part
        .str_at("image_url")
        .ok_or_else(|| Failure::new(path, "an image has no url"))?;
    let (media_type, bytes) =
        data_uri(url).ok_or_else(|| Failure::new(path, "an image url is no base64 data URI"))?;
    let detail = match part.str_at("detail") {
        None => None,
        Some("auto") => Some(v3::fact::detail::AUTO),
        Some("low") => Some(v3::fact::detail::LOW),
        Some("high") => Some(v3::fact::detail::HIGH),
        Some("original") => Some(v3::fact::detail::ORIGINAL),
        Some(_) => {
            return Err(Failure::new(
                path,
                "an image detail the table does not name",
            ))
        }
    };
    Ok(FactCore {
        blob: Some(anybytes::Bytes::from(bytes)),
        media_type: Some(media_type),
        detail,
        ..core(IMAGE, IN)
    })
}

fn effort(term: &str) -> Option<Id> {
    use v3::fact::effort::*;
    Some(match term {
        "low" => LOW,
        "medium" => MEDIUM,
        "high" => HIGH,
        "xhigh" => XHIGH,
        "max" => MAX,
        "ultra" => ULTRA,
        "minimal" => MINIMAL,
        "none" => NONE,
        _ => return None,
    })
}

/// The response item a record holds, if any: an envelope's payload, or a
/// bare (G0/G1) item.
fn item_of(record: &Json) -> Option<&Json> {
    match (record.str_at("type"), record.get("payload")) {
        (Some("response_item"), Some(payload)) => Some(payload),
        (Some(_), Some(_)) => None,
        (Some(_), None) => Some(record),
        (None, _) => None,
    }
}

/// Whether a record places an item on her line other than her reasoning:
/// what lies between a client copy and the message it copies (X13).
fn places_beyond_reasoning(record: &Json) -> bool {
    if let Some(item) = item_of(record) {
        return !matches!(item.str_at("type"), Some("reasoning" | "ghost_snapshot"));
    }
    match record.str_at("type") {
        Some("compacted") => true,
        Some("turn_context") => record.str_at("payload.effort").is_some(),
        Some("session_meta") => record
            .str_at("payload.base_instructions.text")
            .is_some_and(|text| !text.is_empty()),
        _ => false,
    }
}

/// X1, X13: the phase of each of her messages that holds none, from the
/// `agent_message` event that copies it. The copy names its message by
/// structure: codex writes an item's events before the item
/// (`handle_output_item_done`, codex-rs core/src/stream_events_utils.rs,
/// emits the item-completed events, then `record_completed_response_item`
/// persists the item), so its message is the first item after it, past only
/// her reasoning (all 13,694 copies whose text the real sample holds). Where
/// that item does not hold the copy's text but another does, the copy is
/// matched by text alone and fails; where none does, it holds nothing (its
/// mirror decides). Where the message holds a phase, the copy must spell the
/// same. The importer's rule, and the relations' (`grammar::relations`).
pub(crate) fn supplied_phases(records: &[Json]) -> Result<HashMap<usize, String>, Failure> {
    let path = "{event_msg}.payload{agent_message}.phase";
    let said = |item: &Json| -> Vec<String> {
        if item.str_at("type") != Some("message") || item.str_at("role") != Some("assistant") {
            return Vec::new();
        }
        item.get("content")
            .map(Json::arr)
            .unwrap_or(&[])
            .iter()
            .filter(|part| part.str_at("type") == Some("output_text"))
            .filter_map(|part| part.str_at("text").map(str::to_owned))
            .collect()
    };
    let held: HashSet<String> = records.iter().filter_map(item_of).flat_map(said).collect();
    // X13': her messages a compaction installs. A copy can name only an
    // item of the file by position; one whose message only a replacement
    // history holds has no structural match.
    let installed: HashSet<String> = records
        .iter()
        .filter(|record| record.str_at("type") == Some("compacted"))
        .filter_map(|record| record.at("payload.replacement_history"))
        .flat_map(|history| history.arr().iter().flat_map(said))
        .collect();
    let mut phases: HashMap<usize, String> = HashMap::new();
    for (at, record) in records.iter().enumerate() {
        if record.str_at("type") != Some("event_msg") {
            continue;
        }
        let Some(payload) = record.get("payload") else {
            continue;
        };
        if payload.str_at("type") != Some("agent_message") {
            continue;
        }
        let (Some(phase), Some(message)) = (payload.str_at("phase"), payload.str_at("message"))
        else {
            continue;
        };
        if phase.is_empty() {
            continue;
        }
        let next = records[at + 1..]
            .iter()
            .position(places_beyond_reasoning)
            .map(|offset| at + 1 + offset);
        let holder = next.filter(|&next| {
            item_of(&records[next]).is_some_and(|item| said(item).iter().any(|t| t == message))
        });
        let Some(holder) = holder else {
            if held.contains(message) {
                return Err(Failure::new(
                    path,
                    "a phase copy whose message is not the item after it: matched by text alone",
                ));
            }
            if installed.contains(message) {
                return Err(Failure::new(
                    path,
                    "a phase copy whose message only a replacement history holds: no structural \
                     match",
                ));
            }
            continue;
        };
        let own = item_of(&records[holder]).and_then(|item| item.str_at("phase"));
        let disagrees = match own {
            Some(own) => own != phase,
            None => phases
                .insert(holder, phase.to_owned())
                .is_some_and(|known| known != phase),
        };
        if disagrees {
            return Err(Failure::new(
                path,
                "a phase copy that disagrees with her message's phase",
            ));
        }
    }
    Ok(phases)
}

/// X1: the register of her text under her message's phase, or its event
/// copy's: none where neither holds one. The importer's rule, and the
/// relations' (`grammar::relations`).
pub(crate) fn phase_register(phase: Option<&str>, path: &str) -> Result<Option<Id>, Failure> {
    Ok(match phase {
        None => None,
        Some("commentary") => Some(v3::fact::register::COMMENTARY),
        Some("final_answer") => Some(v3::fact::register::ANSWER),
        Some(_) => return Err(Failure::new(path, "a phase the table does not name")),
    })
}

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

    /// The budget a header records in its `truncation_policy`: none where
    /// it records none (X18).
    pub(crate) fn of_header(policy: Option<&Json>) -> Result<Option<Budget>, Failure> {
        match policy {
            None => Ok(None),
            Some(policy) => Ok(Some(Budget::of_policy(policy).ok_or_else(|| {
                Failure::new(
                    "{turn_context}.payload.truncation_policy",
                    "a history budget that does not parse: the importer cannot apply it",
                )
            })?)),
        }
    }

    /// The budget of a response item: its own override, else its header's.
    pub(crate) fn of_record(
        limit: Option<&Json>,
        header: Option<Budget>,
    ) -> Result<Option<Budget>, Failure> {
        match limit {
            None => Ok(header),
            Some(limit) => Ok(Some(Budget::of_override(limit).ok_or_else(|| {
                Failure::new(
                    "{response_item}.metadata.fallback_token_limit_override",
                    "a history budget override that is no count",
                )
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

    pub(crate) fn of_clients<'a>(versions: impl Iterator<Item = Option<&'a str>>) -> Self {
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
/// history. The importer's rule, and the relations' (`grammar::relations`).
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

/// One fact a record places on the line, and whether the record holds it
/// (a gap the rendering rule adds is held by no record).
struct Placed {
    core: FactCore,
    held: bool,
    /// Glosses of the node, by a side model: each part of a summary with
    /// its place among the parts (X16).
    glosses: Vec<(u64, String)>,
    /// A call this item answers: its id, and the correlator's path.
    answers: Option<(String, String)>,
    /// A call it opens, by its id.
    opens: Option<String>,
}

impl Placed {
    fn held(core: FactCore) -> Self {
        Placed {
            core,
            held: true,
            glosses: Vec::new(),
            answers: None,
            opens: None,
        }
    }

    fn gap(core: FactCore) -> Self {
        Placed {
            held: false,
            ..Placed::held(core)
        }
    }
}

/// One item: a fact a record places (or a gap no record holds), known by
/// its place in the source, never by its content.
struct Item {
    fact: Id,
    held: bool,
    /// She produced it.
    out: bool,
    /// The node its record held it in, and that record.
    own: Option<Id>,
    record: Option<usize>,
}

struct Builder {
    out: Fragment,
    facts: HashMap<Id, Fragment>,
    /// node -> (previous, fact, the node itself)
    nodes: HashMap<Id, (Option<Id>, Id, Fragment)>,
    items: Vec<Item>,
    /// X1: phases her messages take from their event copies, by record.
    phases: HashMap<usize, String>,
    header: usize,
    header_gap: usize,
    system: usize,
    system_gap: usize,
    tools: usize,
    /// The unit's own instructions: its first `session_meta`'s (X3).
    own_system: Option<Option<Id>>,
    /// The header and system section's last node.
    root: Option<Id>,
    /// The line after it: (item, node).
    line: Vec<(usize, Id)>,
    /// Where each turn starts on the line.
    turns: Vec<usize>,
    /// Every (node, item) placement, copies included.
    placements: Vec<(Id, usize)>,
    /// (record, node) for every node a record holds.
    held: Vec<(usize, Id)>,
    /// The last node a record held.
    last_node: Option<Id>,
    claims: HashMap<usize, WitnessAnnotations>,
    /// Claims of records that hold no node, for the next node placed, with
    /// the path a unit that ends before that node fails at (X19).
    pending: Vec<(WitnessAnnotations, &'static str)>,
    extra: Vec<(Id, WitnessAnnotations)>,
    /// call id -> (line position, call item)
    calls: HashMap<String, (usize, usize)>,
    responds: Vec<(Id, Id)>,
    /// (record, node, term): why the sampling of the item that record holds
    /// at that node stopped, a claim of that occurrence (M1).
    ends: Vec<(usize, Id, Id)>,
    glosses: Vec<(Id, u64, String)>,
    continues: Vec<(Id, Id)>,
    /// The history budget the latest `turn_context` records: none when it
    /// records none (X18). `TurnContextItem.truncation_policy` exists from
    /// codex rust-v0.78.0 to rust-v0.132.0 (codex-rs protocol/src/
    /// protocol.rs), and those clients write it on every header
    /// (`truncation_policy: Some(..)`: core/src/codex.rs at 0.78.0 and
    /// 0.100.0, core/src/session/turn_context.rs at 0.132.0). A header
    /// without it was written by a client that records none, so a budget is
    /// never carried into it.
    budget: Option<Budget>,
    /// The budget of the record being read: its own override, else the
    /// header's.
    record_budget: Option<Budget>,
    /// X17: whether the unit marks its turns with `task_started`. Where it
    /// marks none, a turn starts at his prompt after an item of hers.
    marked_turns: bool,
    /// X18': the budgets an output may have been cut under where none is
    /// recorded.
    unrecorded: Unrecorded,
}

impl Builder {
    fn new(
        phases: HashMap<usize, String>,
        marked_turns: bool,
        unrecorded: Unrecorded,
    ) -> Result<Self, Failure> {
        use v3::fact::{absent, direction::IN, modality::*};
        let gap = |modality, kind| FactCore {
            absent: Some(kind),
            ..core(modality, IN)
        };
        let mut facts = HashMap::new();
        // A request that records no settings header still had one.
        let header_gap = put_fact(&mut facts, gap(SYSTEM, absent::BODY), "<header>")?;
        // Before a system prompt is recorded, it is unknown; tool
        // definitions are never recorded.
        let system_gap = put_fact(&mut facts, gap(SYSTEM, absent::BODY), "<system>")?;
        let tools = put_fact(&mut facts, gap(TOOL_DEFINITION, absent::SPAN), "<system>")?;
        let unheld = |fact| Item {
            fact,
            held: false,
            out: false,
            own: None,
            record: None,
        };
        let mut builder = Builder {
            out: Fragment::empty(),
            facts,
            nodes: HashMap::new(),
            items: vec![unheld(header_gap), unheld(system_gap), unheld(tools)],
            phases,
            header: 0,
            header_gap: 0,
            system: 1,
            system_gap: 1,
            tools: 2,
            own_system: None,
            root: None,
            line: Vec::new(),
            turns: Vec::new(),
            placements: Vec::new(),
            held: Vec::new(),
            last_node: None,
            claims: HashMap::new(),
            pending: Vec::new(),
            extra: Vec::new(),
            calls: HashMap::new(),
            responds: Vec::new(),
            ends: Vec::new(),
            glosses: Vec::new(),
            continues: Vec::new(),
            budget: None,
            record_budget: None,
            marked_turns,
            unrecorded,
        };
        builder.root = builder.root_line();
        Ok(builder)
    }

    fn put(&mut self, core: FactCore, path: &str) -> Result<Id, Failure> {
        put_fact(&mut self.facts, core, path)
    }

    fn item(&mut self, fact: Id, held: bool, out: bool) -> usize {
        self.items.push(Item {
            fact,
            held,
            out,
            own: None,
            record: None,
        });
        self.items.len() - 1
    }

    fn extend(&mut self, tail: Option<Id>, fact: Id) -> Id {
        let holds = Fragment::rooted(fact, TribleSet::new());
        let built = node(tail, holds).expect("a node over one fact builds");
        let id = built.root().expect("a node is rooted");
        self.nodes
            .entry(id)
            .or_insert_with(|| (tail, fact, unexported(built)));
        id
    }

    /// Place `item` after `tail`.
    fn at(&mut self, tail: Option<Id>, item: usize) -> Id {
        let node = self.extend(tail, self.items[item].fact);
        self.placements.push((node, item));
        node
    }

    /// The header and the system section, from no node.
    fn root_line(&mut self) -> Option<Id> {
        let header = self.at(None, self.header);
        let system = self.at(Some(header), self.system);
        Some(self.at(Some(system), self.tools))
    }

    fn tip(&self) -> Option<Id> {
        self.line.last().map(|(_, node)| *node).or(self.root)
    }

    /// The line re-presented under a new header or system section.
    fn rebuild(&mut self) {
        self.root = self.root_line();
        let mut tail = self.root;
        for at in 0..self.line.len() {
            let node = self.at(tail, self.line[at].0);
            self.line[at].1 = node;
            tail = Some(node);
        }
    }

    fn witness(&mut self, record: usize, item: usize, node: Id) {
        self.items[item].own = Some(node);
        self.items[item].record = Some(record);
        self.held.push((record, node));
        self.last_node = Some(node);
        for (claims, _) in std::mem::take(&mut self.pending) {
            self.extra.push((node, claims));
        }
    }

    /// Place a record's items on the line.
    fn place(&mut self, record: usize, items: Vec<Placed>, path: &str) -> Result<(), Failure> {
        for placed in items {
            let out = placed.core.direction == Some(v3::fact::direction::OUT);
            let fact = self.put(placed.core, path)?;
            let item = self.item(fact, placed.held, out);
            let node = self.at(self.tip(), item);
            if let Some((call, at)) = &placed.answers {
                // X4: the call on her own line, else what she was shown is
                // unknown.
                let found = self
                    .calls
                    .get(call)
                    .copied()
                    .filter(|(position, call_item)| {
                        self.line
                            .get(*position)
                            .is_some_and(|(there, _)| there == call_item)
                    });
                let Some((position, _)) = found else {
                    return Err(Failure::new(
                        at,
                        "an output answers a call outside its window: what she was shown is open",
                    ));
                };
                self.responds.push((node, self.line[position].1));
            }
            self.line.push((item, node));
            if let Some(call) = placed.opens {
                self.calls.insert(call, (self.line.len() - 1, item));
            }
            if placed.held {
                self.witness(record, item, node);
            }
            for (part, text) in placed
                .glosses
                .into_iter()
                .filter(|(_, text)| !text.is_empty())
            {
                self.glosses.push((node, part, text));
            }
        }
        Ok(())
    }

    /// The node its record held of the line's last item a record holds:
    /// the tail a compaction continues.
    fn last_held(&self) -> Option<Id> {
        self.line
            .iter()
            .rev()
            .find(|(item, _)| self.items[*item].held)
            .and_then(|(item, _)| self.items[*item].own)
    }

    /// Her last item of the turn so far: the record that holds it and the
    /// node it holds it in, whose occurrence says why the turn ended (X5,
    /// X6, M1). None when she produced nothing.
    fn last_out(&self) -> Option<(usize, Id)> {
        let since = self.turns.last().copied().unwrap_or(0).min(self.line.len());
        self.line[since..]
            .iter()
            .rev()
            .find(|(item, _)| self.items[*item].held && self.items[*item].out)
            .and_then(|(item, _)| Some((self.items[*item].record?, self.items[*item].own?)))
    }

    /// X15: where a record that ends the turn puts its `turn_ends` claim:
    /// on the node its record held of the turn's last item a record holds,
    /// or, when the turn holds none, on the next node placed (the next
    /// header). Both lie in the file before or right after the record, so
    /// a snapshot cut after it places the claim where the whole file does
    /// (M2), as long as the turn holds a node.
    fn end_turn(&mut self, claims: WitnessAnnotations, path: &'static str) {
        let since = self.turns.last().copied().unwrap_or(0).min(self.line.len());
        let tail = self.line[since..]
            .iter()
            .rev()
            .find(|(item, _)| self.items[*item].held)
            .and_then(|(item, _)| self.items[*item].own);
        match tail {
            Some(node) => self.extra.push((node, claims)),
            None => self.pending.push((claims, path)),
        }
    }

    fn record(&mut self, at: usize, record: &Json) -> Result<(), Failure> {
        let kind = record.str_at("type");
        if let Some(payload) = record.get("payload") {
            let mut claims = WitnessAnnotations::default();
            if let Some(stamp) = record.str_at("timestamp") {
                claims.timestamp = time(stamp).into_iter().collect();
            }
            return match kind {
                Some("session_meta") => self.session(at, payload),
                Some("turn_context") => self.turn_context(at, payload, claims),
                Some("response_item") => {
                    self.record_budget = Budget::of_record(
                        record.at("metadata.fallback_token_limit_override"),
                        self.budget,
                    )?;
                    self.prompt_turn(payload);
                    let items =
                        self.items(payload, "{response_item}.payload", &mut claims, Some(at));
                    self.record_budget = None;
                    let items = items?;
                    let ghost = payload.str_at("type") == Some("ghost_snapshot");
                    if !ghost {
                        self.claims.insert(at, claims);
                    }
                    self.place(at, items, "{response_item}.payload")
                }
                Some("compacted") => self.compacted(at, payload, claims),
                Some("event_msg") => self.event(payload),
                Some(
                    "world_state" | "inter_agent_communication_metadata" | "token_usage_record",
                ) => Ok(()),
                _ => Err(Failure::new(
                    "<record>",
                    "a record kind the importer does not name",
                )),
            };
        }
        match kind {
            // G0/G1: the header line, and resume state lines.
            None if record.get("record_type").is_some() => Ok(()),
            None => {
                if let Some(id) = record.str_at("id") {
                    self.pending.push((
                        WitnessAnnotations {
                            source_locator: Some(id.to_owned()),
                            ..WitnessAnnotations::default()
                        },
                        "<record>",
                    ));
                }
                Ok(())
            }
            // G0/G1: a bare response item.
            Some(_) => {
                self.prompt_turn(record);
                let mut claims = WitnessAnnotations::default();
                let items = self.items(record, "", &mut claims, Some(at))?;
                self.claims.insert(at, claims);
                self.place(at, items, "<item>")
            }
        }
    }

    /// X17: where the unit marks no turn with `task_started`, a turn
    /// starts at his prompt (a user message) when the line since the latest
    /// turn started holds an item of hers, so an abort after his new prompt
    /// is no end of her previous answer (X5).
    fn prompt_turn(&mut self, item: &Json) {
        if self.marked_turns
            || item.str_at("type") != Some("message")
            || item.str_at("role") != Some("user")
        {
            return;
        }
        let since = self.turns.last().copied().unwrap_or(0).min(self.line.len());
        let answered = self.line[since..]
            .iter()
            .any(|(item, _)| self.items[*item].held && self.items[*item].out);
        if answered {
            self.turns.push(self.line.len());
        }
    }

    fn session(&mut self, at: usize, payload: &Json) -> Result<(), Failure> {
        let mut claims = WitnessAnnotations {
            source_locator: payload.str_at("id").map(str::to_owned),
            ..WitnessAnnotations::default()
        };
        for field in [
            "agent_nickname",
            "agent_path",
            "agent_role",
            "source.subagent.thread_spawn.agent_nickname",
            "source.subagent.thread_spawn.agent_path",
            "source.subagent.thread_spawn.agent_role",
        ] {
            if let Some(author) = payload.str_at(field) {
                if !claims.raw_author.iter().any(|known| known == author) {
                    claims.raw_author.push(author.to_owned());
                }
            }
        }
        for field in [
            "parent_thread_id",
            "source.subagent.thread_spawn.parent_thread_id",
        ] {
            if let Some(pointer) = payload.str_at(field) {
                if !claims.pointer.iter().any(|known| known == pointer) {
                    claims.pointer.push(pointer.to_owned());
                }
            }
        }
        let path = "{session_meta}.payload.base_instructions.text";
        let system = match payload.str_at("base_instructions.text") {
            Some(text) if !text.is_empty() => {
                use v3::fact::{direction::IN, modality::SYSTEM};
                Some(self.put(
                    FactCore {
                        payload: Some(text.to_owned()),
                        ..core(SYSTEM, IN)
                    },
                    path,
                )?)
            }
            _ => None,
        };
        // X3: the unit's own instructions are its first header's; a replayed
        // ancestor's that differ leave the fork's own records unmarked.
        match self.own_system {
            None => self.own_system = Some(system),
            Some(own) if own != system => {
                return Err(Failure::new(
                    path,
                    "a replayed header's instructions differ from the unit's own: which records \
                     the fork wrote itself is unmarked",
                ))
            }
            Some(_) => {}
        }
        let next = match system {
            Some(fact) => self.item(fact, true, false),
            None => self.system_gap,
        };
        let changed = self.items[next].fact != self.items[self.system].fact;
        self.system = next;
        if changed {
            self.rebuild();
        }
        match system {
            Some(fact) => {
                // The system node of the root line: header, then it.
                let header = self.extend(None, self.items[self.header].fact);
                let node = self.extend(Some(header), fact);
                self.claims.insert(at, claims);
                self.witness(at, next, node);
            }
            None => self.pending.push((claims, "{session_meta}.payload")),
        }
        Ok(())
    }

    fn turn_context(
        &mut self,
        at: usize,
        payload: &Json,
        mut claims: WitnessAnnotations,
    ) -> Result<(), Failure> {
        claims.raw_model = payload.str_at("model").map(str::to_owned);
        self.budget = Budget::of_header(payload.get("truncation_policy"))?;
        let header = match payload.str_at("effort") {
            Some(term) => {
                use v3::fact::{absent, direction::IN, modality::SYSTEM};
                let term = effort(term).ok_or_else(|| {
                    Failure::new(
                        "{turn_context}.payload.effort",
                        "an effort the table does not name",
                    )
                })?;
                Some(self.put(
                    FactCore {
                        absent: Some(absent::BODY),
                        effort: Some(term),
                        ..core(SYSTEM, IN)
                    },
                    "{turn_context}.payload.effort",
                )?)
            }
            None => None,
        };
        let next = match header {
            Some(fact) => self.item(fact, true, false),
            None => self.header_gap,
        };
        let changed = self.items[next].fact != self.items[self.header].fact;
        self.header = next;
        if changed {
            self.rebuild();
        }
        match header {
            Some(fact) => {
                let node = self.extend(None, fact);
                self.claims.insert(at, claims);
                self.witness(at, next, node);
            }
            None => {
                claims.timestamp.clear();
                self.pending.push((claims, "{turn_context}.payload"));
            }
        }
        Ok(())
    }
    /// The facts of one response item at `path` (a bare line, an envelope's
    /// payload, a replacement history element), in source order.
    fn items(
        &mut self,
        item: &Json,
        path: &str,
        claims: &mut WitnessAnnotations,
        record: Option<usize>,
    ) -> Result<Vec<Placed>, Failure> {
        use v3::fact::{direction::*, modality::*};
        let kind = item.str_at("type").unwrap_or_default();
        let at = |what: &str| format!("{path}{{{kind}}}{what}");
        let mut out = Vec::new();
        match kind {
            "message" => {
                let role = item.str_at("role").unwrap_or_default();
                for part in item.get("content").map(Json::arr).unwrap_or(&[]) {
                    let part_kind = part.str_at("type").unwrap_or_default();
                    match (role, part_kind) {
                        ("user" | "developer", "input_text") => {
                            let text = text_at(part, &at(".content[]{input_text}.text"))?;
                            if !text.is_empty() {
                                let modality = if role == "user" { TEXT } else { DEVELOPER };
                                out.push(Placed::held(FactCore {
                                    payload: Some(text.to_owned()),
                                    ..core(modality, IN)
                                }));
                            }
                        }
                        ("user" | "developer", "input_image") => {
                            out.push(Placed::held(image(part, &at(".content[]{input_image}"))?));
                        }
                        ("assistant", "output_text") => {
                            let text = text_at(part, &at(".content[]{output_text}.text"))?;
                            if !text.is_empty() {
                                // Her message's phase, else its event
                                // copy's (X1).
                                let phase = item.str_at("phase").or_else(|| {
                                    record
                                        .and_then(|at| self.phases.get(&at))
                                        .map(String::as_str)
                                });
                                let register = phase_register(phase, &at(".phase"))?;
                                out.push(Placed::held(FactCore {
                                    payload: Some(text.to_owned()),
                                    register,
                                    ..core(TEXT, OUT)
                                }));
                            }
                        }
                        _ => {
                            return Err(Failure::new(
                                &at(".content[]"),
                                "a message part the importer does not name",
                            ))
                        }
                    }
                }
            }
            "reasoning" => {
                // X16: each part keeps its place among the parts, so their
                // order and count are kept.
                let mut glosses: Vec<(u64, String)> = Vec::new();
                match item.get("summary") {
                    None | Some(Json::Null) => {}
                    Some(Json::Arr(parts)) => {
                        for (place, part) in (0u64..).zip(parts) {
                            if part.str_at("type") != Some("summary_text") {
                                return Err(Failure::new(
                                    &at(".summary[]"),
                                    "a summary part the importer does not name",
                                ));
                            }
                            let text = text_at(part, &at(".summary[]{summary_text}.text"))?;
                            glosses.push((place, text.to_owned()));
                        }
                    }
                    Some(_) => {
                        return Err(Failure::new(&at(".summary"), "a summary that is no list"))
                    }
                }
                // An empty token holds nothing (X7): a body gap, as when
                // none is recorded.
                let sealed = match item.get("encrypted_content") {
                    Some(Json::Str(token)) if !token.is_empty() => Some(sealed_blob(
                        item.get("encrypted_content"),
                        &at(".encrypted_content"),
                    )?),
                    _ => None,
                };
                let core = match sealed {
                    Some(bytes) => FactCore {
                        blob: Some(bytes),
                        media_type: Some(sealed_type(sealed::OPENAI_REASONING)),
                        ..core(THINKING, OUT)
                    },
                    // Her reasoning happened; its token is not recorded.
                    None => FactCore {
                        absent: Some(v3::fact::absent::BODY),
                        ..core(THINKING, OUT)
                    },
                };
                out.push(Placed {
                    glosses,
                    ..Placed::held(core)
                });
            }
            "function_call" => {
                let arguments = item.str_at("arguments").ok_or_else(|| {
                    Failure::new(&at(".arguments"), "a call's arguments are not text")
                })?;
                let parsed = json::parse(arguments.as_bytes()).ok_or_else(|| {
                    Failure::new(&at(".arguments"), "a call's arguments are not JSON")
                })?;
                out.push(Placed {
                    opens: item.str_at("call_id").map(str::to_owned),
                    ..Placed::held(FactCore {
                        tool_name: item.str_at("name").map(str::to_owned),
                        tool_namespace: item.str_at("namespace").map(str::to_owned),
                        structure: Some(structure(&parsed, &at(".arguments"))?),
                        ..core(TOOL_CALL, OUT)
                    })
                });
            }
            "custom_tool_call" => {
                out.push(Placed {
                    opens: item.str_at("call_id").map(str::to_owned),
                    ..Placed::held(FactCore {
                        tool_name: item.str_at("name").map(str::to_owned),
                        payload: item
                            .str_at("input")
                            .filter(|input| !input.is_empty())
                            .map(str::to_owned),
                        ..core(TOOL_CALL, OUT)
                    })
                });
            }
            "local_shell_call" | "web_search_call" => {
                let action = item
                    .get("action")
                    .ok_or_else(|| Failure::new(&at(".action"), "a call has no action"))?;
                let name = if kind == "local_shell_call" {
                    "local_shell"
                } else {
                    "web_search"
                };
                out.push(Placed {
                    opens: item.str_at("call_id").map(str::to_owned),
                    ..Placed::held(FactCore {
                        tool_name: Some(name.to_owned()),
                        structure: Some(structure(action, &at(".action"))?),
                        ..core(TOOL_CALL, OUT)
                    })
                });
                if kind == "web_search_call" {
                    // The server read results into her window; what they
                    // were is unrecorded.
                    out.push(Placed::gap(FactCore {
                        absent: Some(v3::fact::absent::SPAN),
                        ..FactCore::default()
                    }));
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                out.push(Placed {
                    answers: item
                        .str_at("call_id")
                        .map(|call| (call.to_owned(), at(".call_id"))),
                    ..Placed::held(core(TOOL_RESULT, IN))
                });
                // X18: her window holds the output as the client's history
                // truncated it under the record's budget
                // (`ContextManager::record_item_with_metadata`, codex-rs
                // core/src/context_manager/history.rs, through
                // `truncate_function_output_payload`, utils/output-truncation/
                // src/lib.rs: "Tool output truncation applies only to live
                // history, preserving full rollout payloads"). The full text
                // she never saw is the record's claim beside it.
                // X18': only a response item the client recorded as it
                // entered her history. A `compacted` replacement history is
                // installed as it stands (`ContextManager::replace`, codex-rs
                // core/src/context_manager/history.rs at 0.77.0, 0.132.0 and
                // 0.140.0, truncates nothing), and a bare item predates the
                // wrapped rollout lines 0.50.0 already writes
                // (`RolloutItem::ResponseItem`, core/src/codex.rs), so its
                // client cut nothing when recording (before 0.51).
                let history = History {
                    budget: self.record_budget,
                    unrecorded: self.unrecorded,
                    recorded_here: path == "{response_item}.payload",
                };
                match item.get("output") {
                    Some(Json::Str(text)) => {
                        let shown = history.text(text, &at(".output"))?;
                        if shown != *text {
                            claims.untruncated = Some(text.clone());
                        }
                        if !shown.is_empty() {
                            out.push(Placed::held(FactCore {
                                payload: Some(shown),
                                ..core(TEXT, IN)
                            }));
                        }
                    }
                    Some(Json::Arr(parts)) => {
                        history.list(parts, &at(".output"))?;
                        for part in parts {
                            match part.str_at("type") {
                                Some("input_text") => {
                                    let text = text_at(part, &at(".output[]{input_text}.text"))?;
                                    if !text.is_empty() {
                                        out.push(Placed::held(FactCore {
                                            payload: Some(text.to_owned()),
                                            ..core(TEXT, IN)
                                        }));
                                    }
                                }
                                Some("input_image") => {
                                    out.push(Placed::held(image(
                                        part,
                                        &at(".output[]{input_image}"),
                                    )?));
                                }
                                _ => {
                                    return Err(Failure::new(
                                        &at(".output[]"),
                                        "an output part the importer does not name",
                                    ))
                                }
                            }
                        }
                        history.list_whole(parts, &at(".output"))?;
                    }
                    Some(Json::Null) | None => {}
                    Some(_) => {
                        return Err(Failure::new(
                            &at(".output"),
                            "an output that is no text or list",
                        ))
                    }
                }
            }
            "ghost_snapshot" => {}
            "agent_message" => {
                if let Some(author) = item.str_at("author") {
                    if !claims.raw_author.iter().any(|known| known == author) {
                        claims.raw_author.push(author.to_owned());
                    }
                }
                for part in item.get("content").map(Json::arr).unwrap_or(&[]) {
                    match part.str_at("type") {
                        Some("input_text") => {
                            let text = text_at(part, &at(".content[]{input_text}.text"))?;
                            if !text.is_empty() {
                                out.push(Placed::held(FactCore {
                                    payload: Some(text.to_owned()),
                                    ..core(TEXT, IN)
                                }));
                            }
                        }
                        // An empty sealed token holds nothing (X12): a body
                        // gap, never a zero-byte blob.
                        Some("encrypted_content") => {
                            out.push(Placed::held(sealed_or_gap(
                                part.get("encrypted_content"),
                                &at(".content[]{encrypted_content}.encrypted_content"),
                                OPENAI_ENCRYPTED_CONTENT,
                                core(TEXT, IN),
                            )?));
                        }
                        _ => {
                            return Err(Failure::new(
                                &at(".content[]"),
                                "a message part the importer does not name",
                            ))
                        }
                    }
                }
            }
            "compaction" | "compaction_summary" => {
                out.push(Placed::held(sealed_or_gap(
                    item.get("encrypted_content"),
                    &at(".encrypted_content"),
                    sealed::OPENAI_COMPACTION,
                    core(THINKING, IN),
                )?));
            }
            _ => {
                return Err(Failure::new(
                    &format!("{path}{{?}}"),
                    "an item kind the importer does not name",
                ))
            }
        }
        Ok(out)
    }

    fn compacted(
        &mut self,
        at: usize,
        payload: &Json,
        mut claims: WitnessAnnotations,
    ) -> Result<(), Failure> {
        let path = "{compacted}.payload";
        let history = payload
            .get("replacement_history")
            .map(Json::arr)
            .unwrap_or(&[]);
        let summary = payload.str_at("message").unwrap_or_default();
        let old_tip = self.tip();
        let tail = self.last_held().or(old_tip);
        let mut items = Vec::new();
        match (history.is_empty(), summary.is_empty()) {
            (false, true) => {
                for element in history {
                    items.extend(self.items(
                        element,
                        "{compacted}.payload.replacement_history[]",
                        &mut claims,
                        None,
                    )?);
                }
            }
            (true, false) => {
                use v3::fact::{direction::IN, modality::TEXT};
                // Before replacement histories: what the client kept is
                // unrecorded, then the summary.
                items.push(Placed::gap(FactCore {
                    absent: Some(v3::fact::absent::SPAN),
                    ..FactCore::default()
                }));
                items.push(Placed::held(FactCore {
                    payload: Some(summary.to_owned()),
                    ..core(TEXT, IN)
                }));
            }
            (false, false) => {
                return Err(Failure::new(
                    path,
                    "a compaction with both a summary and a replacement history",
                ))
            }
            (true, true) => {
                return Err(Failure::new(
                    path,
                    "a compaction whose new context holds nothing",
                ))
            }
        }
        if items.is_empty() {
            return Err(Failure::new(
                path,
                "a compaction whose new context holds nothing",
            ));
        }
        self.line.clear();
        self.turns.clear();
        self.calls.clear();
        self.claims.insert(at, claims);
        self.place(at, items, path)?;
        // X2: the first node of the new line that is not on the old line
        // continues the old tail, so no continues path returns to its start.
        let mut on_old: HashSet<Id> = HashSet::new();
        let mut cursor = old_tip;
        while let Some(node) = cursor {
            on_old.insert(node);
            cursor = self.nodes[&node].0;
        }
        let mut new_line = Vec::new();
        let mut cursor = self.tip();
        while let Some(node) = cursor {
            new_line.push(node);
            cursor = self.nodes[&node].0;
        }
        let first = new_line
            .into_iter()
            .rev()
            .find(|node| !on_old.contains(node))
            .ok_or_else(|| {
                Failure::new(path, "a compaction whose new context lies on the old line")
            })?;
        if let Some(tail) = tail {
            self.continues.push((first, tail));
        }
        Ok(())
    }

    fn event(&mut self, payload: &Json) -> Result<(), Failure> {
        let kind = payload.str_at("type").unwrap_or_default();
        match kind {
            "turn_aborted" => {
                let term = match payload.str_at("reason") {
                    Some("interrupted") => v3::node::ends::INTERRUPTED,
                    _ => {
                        return Err(Failure::new(
                            "{event_msg}.payload{turn_aborted}.reason",
                            "an abort reason the table does not name",
                        ))
                    }
                };
                // X5: on her last node of the turn, if she produced one; the
                // abort is a claim of the record either way (X15).
                if let Some((record, node)) = self.last_out() {
                    self.ends.push((record, node, term));
                }
                self.end_turn(
                    WitnessAnnotations {
                        turn_ends: vec![term],
                        ..WitnessAnnotations::default()
                    },
                    "{event_msg}.payload{turn_aborted}.reason",
                );
                Ok(())
            }
            "task_complete" => {
                // X6: `ends: error` on her last node of the turn; no fact.
                // X11: an error that names no message (only an error code,
                // or a string) is a failed turn whose report is unverified:
                // it fails, never machinery. X15: the message must be a
                // string; `null`, a number or anything else fails.
                let path = "{event_msg}.payload{task_complete}.error";
                let erred =
                    match payload.get("error") {
                        None | Some(Json::Null) => false,
                        Some(error @ Json::Obj(_)) => match error.get("message") {
                            Some(Json::Str(_)) => true,
                            Some(_) => return Err(Failure::new(
                                path,
                                "a failed turn whose error message is no string: what failed is \
                                 unverified",
                            )),
                            None => {
                                return Err(Failure::new(
                                    path,
                                    "a failed turn whose error names no message: what failed is \
                                 unverified",
                                ))
                            }
                        },
                        Some(_) => return Err(Failure::new(
                            path,
                            "a failed turn whose error names no message: what failed is unverified",
                        )),
                    };
                if erred {
                    match self.last_out() {
                        Some((record, node)) => {
                            self.ends.push((record, node, v3::node::ends::ERROR))
                        }
                        // X15: a failed turn in which she produced nothing
                        // is never lost: a claim of the record, as an abort.
                        None => self.end_turn(
                            WitnessAnnotations {
                                turn_ends: vec![v3::node::ends::ERROR],
                                ..WitnessAnnotations::default()
                            },
                            "{event_msg}.payload{task_complete}.error",
                        ),
                    }
                }
                Ok(())
            }
            "task_started" => {
                self.turns.push(self.line.len());
                Ok(())
            }
            "thread_rolled_back" => {
                let path = "{event_msg}.payload{thread_rolled_back}.num_turns";
                let count = match payload.get("num_turns") {
                    Some(Json::Num(token)) => token
                        .parse::<usize>()
                        .map_err(|_| Failure::new(path, "a rollback count is no count"))?,
                    _ => return Err(Failure::new(path, "a rollback count is no count")),
                };
                if count == 0 {
                    return Ok(());
                }
                if count > self.turns.len() {
                    return Err(Failure::new(
                        path,
                        "a rollback of more turns than the line holds",
                    ));
                }
                let keep = self.turns.len() - count;
                self.line.truncate(self.turns[keep]);
                self.turns.truncate(keep);
                Ok(())
            }
            "agent_message"
            | "agent_reasoning"
            | "user_message"
            | "thread_settings_applied"
            | "token_count"
            | "context_compacted"
            | "exec_command_end"
            | "patch_apply_end"
            | "sub_agent_activity"
            | "collab_agent_interaction_end"
            | "collab_agent_spawn_end"
            | "collab_close_end"
            | "collab_waiting_end"
            | "mcp_tool_call_end"
            | "web_search_end"
            | "item_completed" => Ok(()),
            _ => Err(Failure::new(
                "{event_msg}.payload{?}",
                "an event kind the importer does not name",
            )),
        }
    }

    fn finish(mut self) -> Result<Fragment, Failure> {
        // X19: claims that wait for a node the file does not reach. A
        // backup snapshot cut before that node would give them to another
        // node than the whole file does (the previous turn's last), so the
        // unit fails rather than place them where the source does not.
        if self.last_node.is_some() {
            if let Some((_, path)) = self.pending.first() {
                return Err(Failure::new(
                    path,
                    "a record's claims wait for a node beyond the end of the file",
                ));
            }
        }
        // The windows: every node on the line of a node a record holds.
        let mut emitted: HashSet<Id> = HashSet::new();
        for &(_, node) in &self.held {
            let mut cursor = Some(node);
            while let Some(at) = cursor {
                if !emitted.insert(at) {
                    break;
                }
                cursor = self.nodes[&at].0;
            }
        }
        let on_window = |node: Id, path: &str| -> Result<Id, Failure> {
            if emitted.contains(&node) {
                Ok(node)
            } else {
                Err(Failure::new(path, "a link to a node no window holds"))
            }
        };
        let mut held_facts: HashSet<Id> = HashSet::new();
        for &node in &emitted {
            let (_, fact, built) = &self.nodes[&node];
            self.out += built.clone();
            if held_facts.insert(*fact) {
                self.out += self.facts[fact].clone();
            }
        }
        // A record's item placed where its record did not hold it recalls
        // the node its record held (a gap or an unheld header recalls
        // nothing).
        let mut recalled: HashSet<(Id, Id)> = HashSet::new();
        for &(node, item) in &self.placements {
            let item = &self.items[item];
            let Some(own) = item.own.filter(|own| item.held && *own != node) else {
                continue;
            };
            if emitted.contains(&node) && emitted.contains(&own) && recalled.insert((node, own)) {
                self.out += annotate_node(
                    node,
                    &NodeAnnotations {
                        recalls: vec![own],
                        ..NodeAnnotations::default()
                    },
                );
            }
        }
        for (result, call) in std::mem::take(&mut self.responds) {
            let (Ok(result), Ok(call)) = (on_window(result, "<result>"), on_window(call, "<call>"))
            else {
                return Err(Failure::new("<result>", "a result or call no window holds"));
            };
            self.out += annotate_node(
                result,
                &NodeAnnotations {
                    responds_to: vec![call],
                    ..NodeAnnotations::default()
                },
            );
        }
        for (first, old) in std::mem::take(&mut self.continues) {
            let first = on_window(first, "{compacted}.payload")?;
            let old = on_window(old, "{compacted}.payload")?;
            self.out += annotate_node(
                first,
                &NodeAnnotations {
                    continues: vec![old],
                    ..NodeAnnotations::default()
                },
            );
        }
        // Witnesses: one per node a record holds, one occurrence per record.
        // Why her turn ended is a claim of the occurrence of the record that
        // holds her last item, never of the node (M1).
        let mut ends = std::mem::take(&mut self.ends);
        let mut occurrences: Vec<(Id, WitnessAnnotations)> = Vec::new();
        for &(record, node) in &self.held {
            let mut claims = self.claims.get(&record).cloned().unwrap_or_default();
            ends.retain(|&(by, at, term)| {
                if (by, at) != (record, node) {
                    return true;
                }
                if !claims.ends.contains(&term) {
                    claims.ends.push(term);
                }
                false
            });
            occurrences.push((node, claims));
        }
        if !ends.is_empty() {
            return Err(Failure::new(
                "{event_msg}.payload",
                "a turn end on an item no record holds",
            ));
        }
        occurrences.extend(std::mem::take(&mut self.extra));
        // Why a sampling or turn ended annotates its occurrence and is no
        // part of its id (M2). Two records whose own claims are the same
        // converge on one occurrence; if they ended differently, which
        // stopped which way would be lost, so the unit fails instead. Their
        // claim values are compared where they are made (P1), shared with
        // Claude Code.
        if ends_disagree(&occurrences) {
            return Err(Failure::new(
                "{event_msg}.payload",
                "two records with the same claims end differently",
            ));
        }
        let mut witnesses: HashMap<Id, Id> = HashMap::new();
        for (node, claims) in occurrences {
            let id = match witnesses.get(&node) {
                Some(id) => *id,
                None => {
                    let built = witness(SOURCE_CODEX, node)
                        .map_err(|_| Failure::new("<witness>", "a witness does not build"))?;
                    let id = built.root().expect("a witness is rooted");
                    self.out += unexported(built);
                    witnesses.insert(node, id);
                    id
                }
            };
            let built = occurrence(id, claims)
                .map_err(|_| Failure::new("<witness>", "a witness occurrence does not build"))?;
            self.out += unexported(built);
        }
        // Reasoning summaries, part by part: the source names no model that
        // wrote them.
        for (node, part, text) in std::mem::take(&mut self.glosses) {
            let node = on_window(node, "<gloss>")?;
            let built = gloss_part(node, v3::gloss::category::REASONING_SUMMARY, part, &text)
                .map_err(|_| Failure::new("<gloss>", "a gloss does not build"))?;
            self.out += unexported(built);
        }
        Ok(self.out)
    }
}
