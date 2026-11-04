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
//! - `session_meta` sets the system section: `base_instructions` (or a
//!   `system` body gap before they were recorded), then a `tool-definition`
//!   span gap (tool definitions are never recorded);
//! - a response item extends the line, one node per fact;
//! - `compacted` starts a new line holding exactly what it installs (its
//!   replacement history, or a span gap and its summary), whose first node
//!   `continues` the old tail;
//! - `task_started` marks a turn, and `thread_rolled_back` cuts the line
//!   back to the start of its last turns: the next node branches there;
//! - `turn_aborted` and an erred `task_complete` annotate the last node with
//!   why the turn ended; the error itself is an `in` event fact.
//!
//! When the header or the system section changes, the line is re-presented
//! under the new one: copies `recalls` their originals, and node ids being
//! content addresses, shared prefixes are shared nodes. A fork or resume
//! replays its ancestor's records, which therefore converge with the
//! ancestor's own rollout by structure.
//!
//! Each node a record holds gets a witness and one occurrence holding that
//! record's claims. A record that holds no node (a header without effort, a
//! `session_meta` without instructions) gives its claims to the next node
//! the file places. Reasoning summaries are glosses of the sealed reasoning.

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
use crate::archive_v3::fields::codex::{OPENAI_ENCRYPTED_CONTENT, REASONING_SUMMARIZER, TABLE};
use crate::archive_v3::{
    annotate_gloss, annotate_node, fact, gloss, node, occurrence, unexported, witness, FactCore,
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
    let mut builder = Builder::new()?;
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

/// One fact a record places on the line, and whether the record holds it
/// (a gap the rendering rule adds is held by no record).
struct Placed {
    core: FactCore,
    held: bool,
    /// Glosses of the node, by a side model.
    glosses: Vec<String>,
    /// A call this item answers, by its id; a call it opens.
    answers: Option<String>,
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

struct Builder {
    out: Fragment,
    facts: HashMap<Id, Fragment>,
    /// node -> (previous, fact, the node itself)
    nodes: HashMap<Id, (Option<Id>, Id, Fragment)>,
    header: Id,
    header_gap: Id,
    system: Id,
    system_gap: Id,
    tools: Id,
    /// The header and system section's last node.
    root: Option<Id>,
    /// The line after it: (fact, node).
    line: Vec<(Id, Id)>,
    /// Where each turn starts on the line.
    turns: Vec<usize>,
    /// fact -> the first node a record held it in.
    original: HashMap<Id, Id>,
    /// (record, node) for every node a record holds.
    held: Vec<(usize, Id)>,
    held_nodes: HashSet<Id>,
    claims: HashMap<usize, WitnessAnnotations>,
    /// Claims of records that hold no node, for the next node placed.
    pending: Vec<WitnessAnnotations>,
    extra: Vec<(Id, WitnessAnnotations)>,
    /// call id -> (line position, call fact, call node)
    calls: HashMap<String, (usize, Id, Id)>,
    responds: Vec<(Id, Id)>,
    ends: Vec<(Id, Id)>,
    glosses: Vec<(Id, String)>,
    continues: Vec<(Id, Id)>,
}

impl Builder {
    fn new() -> Result<Self, Failure> {
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
        let mut builder = Builder {
            out: Fragment::empty(),
            facts,
            nodes: HashMap::new(),
            header: header_gap,
            header_gap,
            system: system_gap,
            system_gap,
            tools,
            root: None,
            line: Vec::new(),
            turns: Vec::new(),
            original: HashMap::new(),
            held: Vec::new(),
            held_nodes: HashSet::new(),
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

    fn extend(&mut self, tail: Option<Id>, fact: Id) -> Id {
        let holds = Fragment::rooted(fact, TribleSet::new());
        let built = node(tail, holds).expect("a node over one fact builds");
        let id = built.root().expect("a node is rooted");
        self.nodes
            .entry(id)
            .or_insert_with(|| (tail, fact, unexported(built)));
        id
    }

    /// The header and the system section, from no node.
    fn root_line(&mut self) -> Option<Id> {
        let header = self.extend(None, self.header);
        let system = self.extend(Some(header), self.system);
        Some(self.extend(Some(system), self.tools))
    }

    fn tip(&self) -> Option<Id> {
        self.line.last().map(|(_, node)| *node).or(self.root)
    }

    /// The line re-presented under a new header or system section.
    fn rebuild(&mut self) {
        self.root = self.root_line();
        let mut tail = self.root;
        for at in 0..self.line.len() {
            let node = self.extend(tail, self.line[at].0);
            self.line[at].1 = node;
            tail = Some(node);
        }
    }

    fn witness(&mut self, record: usize, node: Id) {
        self.held.push((record, node));
        self.held_nodes.insert(node);
        for claims in std::mem::take(&mut self.pending) {
            self.extra.push((node, claims));
        }
    }

    /// Place a record's items on the line.
    fn place(&mut self, record: usize, items: Vec<Placed>, path: &str) -> Result<Vec<Id>, Failure> {
        let mut nodes = Vec::new();
        for item in items {
            let fact = self.put(item.core, path)?;
            let node = self.extend(self.tip(), fact);
            if let Some(call) = &item.answers {
                let found = self.calls.get(call).copied();
                if let Some((at, call_fact, call_node)) = found {
                    let current = self
                        .line
                        .get(at)
                        .filter(|(fact, _)| *fact == call_fact)
                        .map(|(_, node)| *node)
                        .unwrap_or(call_node);
                    self.responds.push((node, current));
                }
            }
            self.line.push((fact, node));
            if let Some(call) = item.opens {
                self.calls.insert(call, (self.line.len() - 1, fact, node));
            }
            if item.held {
                self.original.entry(fact).or_insert(node);
                self.witness(record, node);
            }
            for text in item.glosses.into_iter().filter(|text| !text.is_empty()) {
                self.glosses.push((node, text));
            }
            nodes.push(node);
        }
        Ok(nodes)
    }

    /// The last node of the line a record held: the node itself, or for a
    /// copy under a new header its original (a copy is on a window only when
    /// a later record extends it). For why a turn ended, and for the tail a
    /// compaction continues.
    fn last_held(&self) -> Option<Id> {
        self.line.iter().rev().find_map(|(fact, node)| {
            if self.held_nodes.contains(node) {
                Some(*node)
            } else {
                self.original.get(fact).copied()
            }
        })
    }

    fn record(&mut self, at: usize, record: &Json) -> Result<(), Failure> {
        let kind = record.str_at("type");
        if record.get("payload").is_some() {
            let payload = record.get("payload").expect("present");
            let mut claims = WitnessAnnotations::default();
            if let Some(stamp) = record.str_at("timestamp") {
                claims.timestamp = time(stamp).into_iter().collect();
            }
            return match kind {
                Some("session_meta") => self.session(at, payload),
                Some("turn_context") => self.turn_context(at, payload, claims),
                Some("response_item") => {
                    let items = self.items(payload, "{response_item}.payload", &mut claims)?;
                    let ghost = payload.str_at("type") == Some("ghost_snapshot");
                    if !ghost {
                        self.claims.insert(at, claims);
                    }
                    self.place(at, items, "{response_item}.payload")?;
                    Ok(())
                }
                Some("compacted") => self.compacted(at, payload, claims),
                Some("event_msg") => self.event(at, payload, claims),
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
                let items = self.items(record, "", &mut claims)?;
                self.claims.insert(at, claims);
                self.place(at, items, "<item>")?;
                Ok(())
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
        let system = match payload.str_at("base_instructions.text") {
            Some(text) if !text.is_empty() => {
                use v3::fact::{direction::IN, modality::SYSTEM};
                Some(self.put(
                    FactCore {
                        payload: Some(text.to_owned()),
                        ..core(SYSTEM, IN)
                    },
                    "{session_meta}.payload.base_instructions.text",
                )?)
            }
            _ => None,
        };
        let next = system.unwrap_or(self.system_gap);
        let changed = next != self.system;
        self.system = next;
        if changed {
            self.rebuild();
        }
        match system {
            Some(fact) => {
                // The system node of the root line: header, then it.
                let header = self.extend(None, self.header);
                let node = self.extend(Some(header), fact);
                self.original.entry(fact).or_insert(node);
                self.claims.insert(at, claims);
                self.witness(at, node);
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
        let next = header.unwrap_or(self.header_gap);
        if next != self.header {
            self.header = next;
            self.rebuild();
        }
        match header {
            Some(fact) => {
                let node = self.extend(None, fact);
                self.original.entry(fact).or_insert(node);
                self.claims.insert(at, claims);
                self.witness(at, node);
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
                                let register = match item.str_at("phase") {
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
                let sealed = match item.get("encrypted_content") {
                    Some(Json::Str(_)) => Some(sealed_blob(
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
                    answers: item.str_at("call_id").map(str::to_owned),
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
        let old = self.last_held().or_else(|| self.tip());
        let mut items = Vec::new();
        match (history.is_empty(), summary.is_empty()) {
            (false, true) => {
                for element in history {
                    items.extend(self.items(
                        element,
                        "{compacted}.payload.replacement_history[]",
                        &mut claims,
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
        let nodes = self.place(at, items, path)?;
        if let (Some(first), Some(old)) = (nodes.first(), old) {
            self.continues.push((*first, old));
        }
        Ok(())
    }

    fn event(
        &mut self,
        at: usize,
        payload: &Json,
        claims: WitnessAnnotations,
    ) -> Result<(), Failure> {
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
                let node = self.last_held().ok_or_else(|| {
                    Failure::new(
                        "{event_msg}.payload{turn_aborted}",
                        "an aborted turn with nothing before it",
                    )
                })?;
                self.ends.push((node, term));
                Ok(())
            }
            "task_complete" => {
                let Some(error) = payload.get("error").filter(|e| e.get("message").is_some())
                else {
                    return Ok(());
                };
                let node = self.last_held().ok_or_else(|| {
                    Failure::new(
                        "{event_msg}.payload{task_complete}.error",
                        "a failed turn with nothing before it",
                    )
                })?;
                self.ends.push((node, v3::node::ends::ERROR));
                use v3::fact::{direction::IN, modality::EVENT};
                let path = "{event_msg}.payload{task_complete}.error";
                let core = FactCore {
                    structure: Some(structure(error, path)?),
                    ..core(EVENT, IN)
                };
                self.claims.insert(at, claims);
                self.place(at, vec![Placed::held(core)], path)?;
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
            if let Some(original) = self
                .original
                .get(fact)
                .filter(|original| **original != node && emitted.contains(*original))
            {
                self.out += annotate_node(
                    node,
                    &NodeAnnotations {
                        recalls: vec![*original],
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
        for (node, text) in std::mem::take(&mut self.glosses) {
            let node = on_window(node, "<gloss>")?;
            let built = gloss(node, v3::gloss::category::REASONING_SUMMARY, &text)
                .map_err(|_| Failure::new("<gloss>", "a gloss does not build"))?;
            let id = built.root().expect("a gloss is rooted");
            self.out += unexported(built);
            self.out += annotate_gloss(id, REASONING_SUMMARIZER)
                .map_err(|_| Failure::new("<gloss>", "a gloss annotation does not build"))?;
        }
        Ok(self.out)
    }
}
