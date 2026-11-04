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
//! - `task_started` marks a turn, and `thread_rolled_back` cuts the line
//!   back to the start of its last turns: the next node branches there;
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
//! next node the file places, or after the last one to that last node.
//! Reasoning summaries are glosses of the sealed reasoning; the source does
//! not name the model that wrote them, so they name none. Where her message
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

use crate::archive_v3::claude_code::check::{self, base64_url, data_uri};
use crate::archive_v3::claude_code::json::{self, Json};
use crate::archive_v3::claude_code::Failure;
use crate::archive_v3::fields::codex::{OPENAI_ENCRYPTED_CONTENT, TABLE};
use crate::archive_v3::{
    annotate_node, ends_disagree, fact, gloss, node, occurrence, unexported, witness, FactCore,
    MediaType, NodeAnnotations, WitnessAnnotations,
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
    let mut builder = Builder::new(phases)?;
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
/// same.
fn supplied_phases(records: &[Json]) -> Result<HashMap<usize, String>, Failure> {
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

/// A tool output's history budget: in tokens, or in bytes.
#[derive(Clone, Copy)]
enum Budget {
    Tokens(usize),
    Bytes(usize),
}

impl Budget {
    /// The turn's `truncation_policy` with codex's 20% serialization
    /// allowance (`with_serialization_allowance`, rounded up).
    fn of_policy(policy: &Json) -> Option<Budget> {
        let limit = match policy.get("limit") {
            Some(Json::Num(token)) => token.parse::<usize>().ok()?,
            _ => return None,
        };
        let allowed = (limit as f64 * 1.2).ceil() as usize;
        match policy.str_at("mode") {
            Some("tokens") => Some(Budget::Tokens(allowed)),
            Some("bytes") => Some(Budget::Bytes(allowed)),
            _ => None,
        }
    }

    /// The cost of a text under the budget's unit (codex's four bytes per
    /// token, rounded up).
    fn cost(self, text: &str) -> usize {
        match self {
            Budget::Tokens(_) => text.len().div_ceil(4),
            Budget::Bytes(_) => text.len(),
        }
    }

    fn limit(self) -> usize {
        match self {
            Budget::Tokens(limit) | Budget::Bytes(limit) => limit,
        }
    }

    /// Whether `truncate_function_output_payload` shortens this output:
    /// a text over the budget, or a list in which a non-empty text part no
    /// longer fits (codex-rs utils/output-truncation/src/lib.rs,
    /// `truncate_text` and `truncate_output_iter`). Empty text parts, which
    /// it drops under any budget, hold no fact here either.
    fn shortens(self, output: &Json) -> bool {
        match output {
            Json::Str(text) => self.cost(text) > self.limit(),
            Json::Arr(parts) => {
                let mut remaining = self.limit();
                for part in parts {
                    if part.str_at("type") != Some("input_text") {
                        continue;
                    }
                    let Some(text) = part.str_at("text").filter(|text| !text.is_empty()) else {
                        continue;
                    };
                    let cost = self.cost(text);
                    if remaining == 0 || cost > remaining {
                        return true;
                    }
                    remaining -= cost;
                }
                false
            }
            _ => false,
        }
    }
}

/// One fact a record places on the line, and whether the record holds it
/// (a gap the rendering rule adds is held by no record).
struct Placed {
    core: FactCore,
    held: bool,
    /// Glosses of the node, by a side model.
    glosses: Vec<String>,
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
    /// Claims of records that hold no node, for the next node placed.
    pending: Vec<WitnessAnnotations>,
    extra: Vec<(Id, WitnessAnnotations)>,
    /// call id -> (line position, call item)
    calls: HashMap<String, (usize, usize)>,
    responds: Vec<(Id, Id)>,
    /// (record, node, term): why the sampling of the item that record holds
    /// at that node stopped, a claim of that occurrence (M1).
    ends: Vec<(usize, Id, Id)>,
    glosses: Vec<(Id, String)>,
    continues: Vec<(Id, Id)>,
    /// The history budget of the latest `turn_context` that records one.
    budget: Option<Budget>,
}

impl Builder {
    fn new(phases: HashMap<usize, String>) -> Result<Self, Failure> {
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
        for claims in std::mem::take(&mut self.pending) {
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
            for text in placed.glosses.into_iter().filter(|text| !text.is_empty()) {
                self.glosses.push((node, text));
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
    fn end_turn(&mut self, claims: WitnessAnnotations) {
        let since = self.turns.last().copied().unwrap_or(0).min(self.line.len());
        let tail = self.line[since..]
            .iter()
            .rev()
            .find(|(item, _)| self.items[*item].held)
            .and_then(|(item, _)| self.items[*item].own);
        match tail {
            Some(node) => self.extra.push((node, claims)),
            None => self.pending.push(claims),
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
                    self.within_budget(record, payload)?;
                    let items =
                        self.items(payload, "{response_item}.payload", &mut claims, Some(at))?;
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
                    self.pending.push(WitnessAnnotations {
                        source_locator: Some(id.to_owned()),
                        ..WitnessAnnotations::default()
                    });
                }
                Ok(())
            }
            // G0/G1: a bare response item.
            Some(_) => {
                let mut claims = WitnessAnnotations::default();
                let items = self.items(record, "", &mut claims, Some(at))?;
                self.claims.insert(at, claims);
                self.place(at, items, "<item>")
            }
        }
    }

    /// The window she was sampled from holds a tool output as her live
    /// history recorded it: `ContextManager::record_item_with_metadata`
    /// (codex-rs core/src/context_manager/history.rs) passes it through
    /// `truncate_function_output_payload` under the record's
    /// `fallback_token_limit_override`, else the turn's `truncation_policy`
    /// with its allowance, "preserving full rollout payloads". Where that
    /// shortens an output, what she was shown is the truncated form, which
    /// this importer does not build, so the unit fails rather than import
    /// the full payload as her window. Where no budget is recorded it is
    /// unknown, and nothing is claimed.
    fn within_budget(&self, record: &Json, payload: &Json) -> Result<(), Failure> {
        let kind = payload.str_at("type").unwrap_or_default();
        if !matches!(kind, "function_call_output" | "custom_tool_call_output") {
            return Ok(());
        }
        let budget = match record.at("metadata.fallback_token_limit_override") {
            Some(Json::Num(token)) => token.parse::<usize>().ok().map(Budget::Tokens),
            _ => self.budget,
        };
        let Some(budget) = budget else {
            return Ok(());
        };
        if payload
            .get("output")
            .is_some_and(|output| budget.shortens(output))
        {
            return Err(Failure::new(
                &format!("{{response_item}}.payload{{{kind}}}.output"),
                "an output over its recorded history budget: her window held a truncated form \
                 the importer does not build",
            ));
        }
        Ok(())
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
            None => self.pending.push(claims),
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
        if let Some(policy) = payload.get("truncation_policy") {
            self.budget = Budget::of_policy(policy);
        }
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
                self.pending.push(claims);
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
                                let register = match phase {
                                    None => None,
                                    Some("commentary") => Some(v3::fact::register::COMMENTARY),
                                    Some("final_answer") => Some(v3::fact::register::ANSWER),
                                    Some(_) => {
                                        return Err(Failure::new(
                                            &at(".phase"),
                                            "a phase the table does not name",
                                        ))
                                    }
                                };
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
                let mut glosses: Vec<String> = Vec::new();
                match item.get("summary") {
                    None | Some(Json::Null) => {}
                    Some(Json::Arr(parts)) => {
                        for part in parts {
                            if part.str_at("type") != Some("summary_text") {
                                return Err(Failure::new(
                                    &at(".summary[]"),
                                    "a summary part the importer does not name",
                                ));
                            }
                            let text = text_at(part, &at(".summary[]{summary_text}.text"))?;
                            glosses.push(text.to_owned());
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
                match item.get("output") {
                    Some(Json::Str(text)) => {
                        if !text.is_empty() {
                            out.push(Placed::held(FactCore {
                                payload: Some(text.clone()),
                                ..core(TEXT, IN)
                            }));
                        }
                    }
                    Some(Json::Arr(parts)) => {
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
                self.end_turn(WitnessAnnotations {
                    turn_ends: vec![term],
                    ..WitnessAnnotations::default()
                });
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
                        None => self.end_turn(WitnessAnnotations {
                            turn_ends: vec![v3::node::ends::ERROR],
                            ..WitnessAnnotations::default()
                        }),
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
        // Claims after the file's last node go to that node.
        if let Some(node) = self.last_node {
            for claims in std::mem::take(&mut self.pending) {
                self.extra.push((node, claims));
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
        // Reasoning summaries: the source names no model that wrote them.
        for (node, text) in std::mem::take(&mut self.glosses) {
            let node = on_window(node, "<gloss>")?;
            let built = gloss(node, v3::gloss::category::REASONING_SUMMARY, &text)
                .map_err(|_| Failure::new("<gloss>", "a gloss does not build"))?;
            self.out += unexported(built);
        }
        Ok(self.out)
    }
}
