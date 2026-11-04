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
//! - `turn_aborted` and an erred `task_complete` put `ends` on her last node
//!   of the turn, when she produced one (X5, X6); an abort is also a claim
//!   of its record.
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
//! record's claims. A record that holds no node (a header without effort, a
//! `session_meta` without instructions, an abort) gives its claims to the
//! next node the file places, or after the last one to that last node.
//! Reasoning summaries are glosses of the sealed reasoning; the source does
//! not name the model that wrote them, so they name none. Where her message
//! holds no phase, its `agent_message` event's phase is its register (X1).

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
    annotate_node, fact, gloss, node, occurrence, unexported, witness, FactCore, MediaType,
    NodeAnnotations, WitnessAnnotations,
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

/// X1: the phase of each of her messages that holds none, from the
/// `agent_message` event that copies it. The event names its message by
/// text: the nearest message before it holding that text (else the nearest
/// after it). Where the message holds a phase, the copy must spell the same.
fn supplied_phases(records: &[Json]) -> Result<HashMap<usize, String>, Failure> {
    let path = "{event_msg}.payload{agent_message}.phase";
    // (record, a text of her message, the phase the message holds)
    let mut texts: Vec<(usize, &str, Option<&str>)> = Vec::new();
    for (at, record) in records.iter().enumerate() {
        let item = match record.get("payload") {
            Some(payload) if record.str_at("type") == Some("response_item") => payload,
            Some(_) => continue,
            None => record,
        };
        if item.str_at("type") != Some("message") || item.str_at("role") != Some("assistant") {
            continue;
        }
        for part in item.get("content").map(Json::arr).unwrap_or(&[]) {
            if part.str_at("type") != Some("output_text") {
                continue;
            }
            if let Some(text) = part.str_at("text") {
                texts.push((at, text, item.str_at("phase")));
            }
        }
    }
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
        let named = texts.iter().filter(|(_, text, _)| *text == message);
        let before = named
            .clone()
            .filter(|(holder, ..)| *holder < at)
            .max_by_key(|t| t.0);
        let after = named
            .filter(|(holder, ..)| *holder > at)
            .min_by_key(|t| t.0);
        let Some(&(holder, _, own)) = before.or(after) else {
            continue;
        };
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
    /// The node its record held it in.
    own: Option<Id>,
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
    ends: Vec<(Id, Id)>,
    glosses: Vec<(Id, String)>,
    continues: Vec<(Id, Id)>,
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

    /// Her last item of the turn so far, at the node its record held: where
    /// why the turn ended sits (X5, X6). None when she produced nothing.
    fn last_out(&self) -> Option<Id> {
        let since = self.turns.last().copied().unwrap_or(0).min(self.line.len());
        self.line[since..]
            .iter()
            .rev()
            .find(|(item, _)| self.items[*item].held && self.items[*item].out)
            .and_then(|(item, _)| self.items[*item].own)
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
                            let text = part.str_at("text").unwrap_or_default();
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
                            let text = part.str_at("text").unwrap_or_default();
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
                let glosses: Vec<String> = item
                    .get("summary")
                    .map(Json::arr)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|part| part.str_at("text"))
                    .map(str::to_owned)
                    .collect();
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
                                    let text = part.str_at("text").unwrap_or_default();
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
                            let text = part.str_at("text").unwrap_or_default();
                            if !text.is_empty() {
                                out.push(Placed::held(FactCore {
                                    payload: Some(text.to_owned()),
                                    ..core(TEXT, IN)
                                }));
                            }
                        }
                        Some("encrypted_content") => {
                            out.push(Placed::held(FactCore {
                                blob: Some(sealed_blob(
                                    part.get("encrypted_content"),
                                    &at(".content[]{encrypted_content}"),
                                )?),
                                media_type: Some(sealed_type(OPENAI_ENCRYPTED_CONTENT)),
                                ..core(TEXT, IN)
                            }));
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
                out.push(Placed::held(FactCore {
                    blob: Some(sealed_blob(
                        item.get("encrypted_content"),
                        &at(".encrypted_content"),
                    )?),
                    media_type: Some(sealed_type(sealed::OPENAI_COMPACTION)),
                    ..core(THINKING, IN)
                }));
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
                // abort is a claim of the record either way.
                if let Some(node) = self.last_out() {
                    self.ends.push((node, term));
                }
                self.pending.push(WitnessAnnotations {
                    turn_ends: vec![term],
                    ..WitnessAnnotations::default()
                });
                Ok(())
            }
            "task_complete" => {
                // X6: `ends: error` on her last node of the turn; no fact.
                let erred = payload
                    .get("error")
                    .is_some_and(|error| error.get("message").is_some());
                if let (true, Some(node)) = (erred, self.last_out()) {
                    self.ends.push((node, v3::node::ends::ERROR));
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
        for (node, term) in std::mem::take(&mut self.ends) {
            let node = on_window(node, "{event_msg}.payload")?;
            self.out += annotate_node(
                node,
                &NodeAnnotations {
                    ends: vec![term],
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
        let mut occurrences: Vec<(Id, WitnessAnnotations)> = Vec::new();
        for &(record, node) in &self.held {
            let claims = self.claims.get(&record).cloned().unwrap_or_default();
            occurrences.push((node, claims));
        }
        occurrences.extend(std::mem::take(&mut self.extra));
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
