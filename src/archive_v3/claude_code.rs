//! The v3 Claude Code importer: one transcript file in, one [`Fragment`] out,
//! or a [`Failure`] that names a path and a reason (never a value).
//!
//! A unit is one `.jsonl` file: a main session or one subagent's file. Units
//! are independent (model v3): nothing here looks at another file or at a
//! pile, and a link into another unit (a subagent's spawning call, a session
//! this one continues) is never computed.
//!
//! The unit is decoded completely in memory. Every record is first checked
//! against the field table ([`crate::archive_v3::fields::claude_code`]): an
//! unknown path, record kind, block kind or vocabulary value fails the unit,
//! and so does a field the table holds open. Only then is anything built.
//!
//! # What is built
//!
//! Each record's items become facts, each held by one node chained after the
//! record's parent (`parentUuid`); records are built parents first, so a
//! parent written after its child is still its parent. A parent that names
//! no record of the unit opens a new root on a span gap: what that record
//! held is unknown, never absent.
//!
//! A window starts with the settings header of the request it was sampled
//! for (the effort, rendered into her prompt but in an unpublished text and
//! place: a `system` body gap that carries it), then the system section: the
//! recorded system prompt and tool definitions (`api-request-shape`), or,
//! before one is recorded, a `system` body gap and a `tool_definition` span
//! gap. When the header or the shape changes, the next window is rebuilt on
//! it, its earlier items re-presented as copies that `recalls` their
//! originals.
//!
//! The window an `out` fact was sampled from is exactly what the source
//! marks, applied to the full history the client sent with that request and
//! never carried into the next request:
//!
//! - `input_transformations` of type `thinking_dropped` remove the named
//!   thinking blocks (`messages.N.content.M`);
//! - `context_management.applied_edits` of type `clear_tool_uses_20250919`
//!   replace the content of the oldest N results with a body gap (the
//!   server's placeholder text is unpublished);
//! - a compaction boundary starts a new root whose first node `continues`
//!   from the old tail (an old tail outside the unit is a span gap before
//!   the summary), and its preserved segment is re-presented after the
//!   summary.
//!
//! A window that differs branches at the first difference; node ids are
//! content addresses, so shared prefixes are shared nodes. Rules the source
//! does not mark (old thinking stripped per model) are never applied here.
//!
//! Each node a record holds gets a witness, and the record's claims (time,
//! role, model, session) form one occurrence of it, so two records holding
//! one node never mix their claims. Readable thinking, titles and away
//! recaps are glosses. An attachment recorded before the client kept its
//! rendering keeps its data as `in` event facts whose nodes say the
//! rendering is unrecorded.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use base64::Engine;
use hifitime::Epoch;
use triblespace::core::blob::MemoryBlobStore;
use triblespace::core::import::json_tree::JsonTreeImporter;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

use self::json::Json;
use crate::archive_v3::fields::claude_code::{SUMMARIZED_THINKING, TABLE};
use crate::archive_v3::{
    annotate_gloss, annotate_node, fact, gloss, node, occurrence, unexported, witness, FactCore,
    MediaType, NodeAnnotations, WitnessAnnotations,
};
use crate::schemas::archive_v3 as v3;
use crate::schemas::blockdag::content_fact::sealed;
use crate::schemas::blockdag::source_projection::SOURCE_CLAUDE_CODE;

mod check;
pub mod json;
#[cfg(test)]
mod tests;

/// The side models whose text becomes a gloss, by the role they play. The
/// sources do not name them.
pub const THINKING_SUMMARIZER: &str = "anthropic thinking summarizer (unnamed)";
pub const TITLE_MODEL: &str = "claude code ai-title model (unnamed)";
pub const RECAP_MODEL: &str = "claude code away-summary model (unnamed)";

/// Why a unit was refused: a normalized source path and a fixed reason.
/// Neither ever holds a source value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub path: String,
    pub reason: &'static str,
}

impl Failure {
    pub(crate) fn new(path: &str, reason: &'static str) -> Self {
        Failure {
            path: path.to_owned(),
            reason,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

impl std::error::Error for Failure {}

pub(crate) fn time(text: &str) -> Option<Inline<NsTAIInterval>> {
    let epoch = Epoch::from_str(text.trim()).ok()?;
    (epoch, epoch).try_to_inline().ok()
}

/// Import one Claude Code transcript file.
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
    let members = records
        .iter()
        .map(|record| check::record(&TABLE, record))
        .collect::<Result<Vec<_>, _>>()?;
    // The first record with an id is the record; a repeat must hold the
    // same thing.
    let mut first: HashMap<String, usize> = HashMap::new();
    let mut repeat = vec![false; records.len()];
    for (at, record) in records.iter().enumerate() {
        let Some(uuid) = record.str_at("uuid") else {
            continue;
        };
        match first.get(uuid) {
            Some(&earlier) => {
                if !same_kept(&records[earlier], record) {
                    let kind = record.str_at("type").unwrap_or_default();
                    return Err(Failure::new(
                        &format!("{{{kind}}}.uuid"),
                        "a record id repeats with other content",
                    ));
                }
                repeat[at] = true;
            }
            None => {
                first.insert(uuid.to_owned(), at);
            }
        }
    }
    let mut builder = Builder::new(first.keys().cloned().collect())?;
    // The system section in force at each record: the latest recorded
    // shape before it in the file.
    for (at, record) in records.iter().enumerate() {
        builder.version_at.push(builder.prefixes.len() - 1);
        if record.str_at("type") == Some("api-request-shape") {
            builder.shape(at, record)?;
        }
    }
    // Parents before children; otherwise the file's order.
    let mut order = Vec::with_capacity(records.len());
    let mut done: HashSet<&str> = HashSet::new();
    let mut waiting: HashMap<&str, Vec<usize>> = HashMap::new();
    for (at, record) in records.iter().enumerate() {
        if repeat[at] || record.str_at("type") == Some("api-request-shape") {
            continue;
        }
        if let Some(parent) = record.str_at("parentUuid") {
            if first.contains_key(parent) && !done.contains(parent) {
                waiting.entry(parent).or_default().push(at);
                continue;
            }
        }
        let mut stack = vec![at];
        while let Some(next) = stack.pop() {
            order.push(next);
            if let Some(uuid) = records[next].str_at("uuid") {
                done.insert(uuid);
                if let Some(children) = waiting.remove(uuid) {
                    stack.extend(children.into_iter().rev());
                }
            }
        }
    }
    if !waiting.is_empty() {
        return Err(Failure::new(
            "{user}.parentUuid",
            "records whose parent links form a cycle",
        ));
    }
    for at in order {
        builder.record(at, &records[at], &members[at])?;
    }
    builder.finish(&records)
}

/// Whether two records with one id hold the same thing: every top-level
/// member in which they differ is one the table drops as machinery or a
/// provider id.
fn same_kept(a: &Json, b: &Json) -> bool {
    use crate::archive_v3::fields::{Class, DropReason};
    let (Json::Obj(left), Json::Obj(right)) = (a, b) else {
        return false;
    };
    if a.str_at("type") != b.str_at("type") {
        return false;
    }
    let kind = a.str_at("type").unwrap_or_default();
    let names: BTreeSet<&str> = left
        .iter()
        .chain(right.iter())
        .map(|(name, _)| name.as_str())
        .collect();
    names.into_iter().all(|name| {
        let equal = match (a.get(name), b.get(name)) {
            (Some(x), Some(y)) => x.same(y),
            (None, None) => true,
            _ => false,
        };
        let path = format!("{{{kind}}}.{name}");
        let mut entries = TABLE
            .entries
            .iter()
            .filter(|entry| entry.path == path)
            .peekable();
        equal
            || (entries.peek().is_some()
                && entries.all(|entry| {
                    matches!(
                        entry.class,
                        Class::Drop(DropReason::Machinery(_) | DropReason::ProviderId)
                    )
                }))
    })
}

/// Who a window item belongs to when the request's messages are counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// The system section: the settings header, the system prompt and the
    /// tool definitions.
    Prefix,
    User,
    Assistant,
    /// A system-role turn among the messages.
    System,
    /// Harness text in her slot.
    Voiced,
    /// A span of unknown items: what a record outside the unit held.
    Gap,
}

#[derive(Clone, Debug)]
struct Item {
    fact: Id,
    role: Role,
    thinking: bool,
    /// The head of a tool result.
    head: bool,
    /// The head whose part this is.
    part_of: Option<usize>,
    record: Option<usize>,
    original: Option<Id>,
    ends: Vec<Id>,
    responds_to: Option<usize>,
    /// The source kept the data, not the rendering she read.
    unrecorded: bool,
}

/// Put `value` at the member path `steps` of `object`, making the objects
/// on the way.
fn insert(object: &mut Json, steps: &[&str], value: &Json) -> Result<(), &'static str> {
    let Json::Obj(members) = object else {
        return Err("a structure member inside a value that is no object");
    };
    let Some((first, rest)) = steps.split_first() else {
        return Err("a structure member with no name");
    };
    let at = match members.iter().position(|(name, _)| name == first) {
        Some(at) => at,
        None => {
            let empty = if rest.is_empty() {
                value.clone()
            } else {
                Json::Obj(Vec::new())
            };
            members.push(((*first).to_owned(), empty));
            if rest.is_empty() {
                return Ok(());
            }
            members.len() - 1
        }
    };
    if rest.is_empty() {
        return Err("a structure member named twice");
    }
    insert(&mut members[at].1, rest, value)
}

/// The end of a line: its last node, the system section it was built on,
/// and the settings header that opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tail {
    node: Option<Id>,
    version: usize,
    header: usize,
}

/// What a record leaves for its children: the line the source marks (with
/// the latest request's own window edits), and the same history unedited,
/// which a new request starts from (I7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Line {
    edited: Tail,
    unedited: Tail,
}

impl Line {
    fn same(tail: Tail) -> Self {
        Line {
            edited: tail,
            unedited: tail,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Markers {
    /// `(message, block)` of each dropped thinking block.
    dropped: Vec<(usize, usize)>,
    /// How many of the oldest results were cleared.
    cleared: usize,
}

impl Markers {
    fn is_empty(&self) -> bool {
        self.dropped.is_empty() && self.cleared == 0
    }
}

#[derive(Debug)]
struct Message {
    /// The unedited history the request was sent with, under its header.
    base: Tail,
    /// Its records so far, with their uuids and items.
    records: Vec<(Option<String>, Vec<usize>)>,
    markers: Option<Markers>,
    last_item: Option<usize>,
}

struct Boundary {
    logical_parent: String,
    /// `(head, tail, preserved uuids)`.
    segment: Option<(String, String, Vec<String>)>,
}

struct Builder {
    out: Fragment,
    /// Every fact built, staged: only those a window holds are emitted.
    facts: HashMap<Id, Fragment>,
    items: Vec<Item>,
    /// node -> (previous, item, the node itself). Lines are built as the
    /// records come; only the nodes on a witnessed window are emitted.
    nodes: HashMap<Id, (Option<Id>, usize, Fragment)>,
    /// Every record id in the unit.
    uuids: HashSet<String>,
    /// What each record contributed and the line it leaves.
    record_items: HashMap<String, Vec<usize>>,
    record_parent: HashMap<String, Option<String>>,
    lines: HashMap<String, Line>,
    /// The system sections, oldest first: `[0]` is the unrecorded one.
    prefixes: Vec<Vec<usize>>,
    /// The system section in force at each record.
    version_at: Vec<usize>,
    /// The settings header when the request records none, and one per
    /// recorded effort.
    header_gap: usize,
    headers: HashMap<Id, usize>,
    /// A span of unknown items.
    span_gap: usize,
    calls: HashMap<String, usize>,
    /// call id -> the record that holds the call
    call_records: HashMap<String, String>,
    messages: HashMap<String, Message>,
    request_drops: HashMap<String, usize>,
    thinking_drops: Vec<(String, usize)>,
    boundaries: HashMap<String, Boundary>,
    /// anchor uuid -> boundary uuid
    anchors: HashMap<String, String>,
    interrupted: Vec<String>,
    /// `(item, category, text, model)`
    item_glosses: Vec<(usize, Id, String, &'static str)>,
    node_glosses: Vec<(Id, Id, String, &'static str)>,
    /// `(file position, title)`: resolved once every line is built.
    titles: Vec<(usize, String)>,
    /// `(first item of the new context, boundary uuid)`
    continues: Vec<(usize, String)>,
    claims: HashMap<usize, WitnessAnnotations>,
}

fn core(modality: Id, direction: Id) -> FactCore {
    FactCore {
        modality: Some(modality),
        direction: Some(direction),
        ..FactCore::default()
    }
}

fn text(modality: Id, direction: Id, payload: &str) -> FactCore {
    FactCore {
        payload: Some(payload.to_owned()),
        ..core(modality, direction)
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

fn bytes(value: Option<&Json>, path: &str) -> Result<anybytes::Bytes, Failure> {
    let text = value
        .and_then(Json::str)
        .ok_or_else(|| Failure::new(path, "base64 data is missing"))?;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map(anybytes::Bytes::from)
        .map_err(|_| Failure::new(path, "base64 data does not decode"))
}

fn media_type(value: Option<&Json>, path: &str) -> Result<MediaType, Failure> {
    let text = value
        .and_then(Json::str)
        .ok_or_else(|| Failure::new(path, "a media type is missing"))?;
    MediaType::parse(text).map_err(|_| Failure::new(path, "a media type does not parse"))
}

fn sealed_type(spelling: &str) -> MediaType {
    MediaType::parse(spelling).expect("the sealed media types are well formed")
}

/// The machinery record kinds: each holds nothing, and a record of them
/// with an id passes its parent's line on.
const MACHINERY_KINDS: &[&str] = &[
    "progress",
    "queue-operation",
    "last-prompt",
    "mode",
    "permission-mode",
    "atis-latch",
    "agent-name",
    "agent-setting",
    "custom-title",
    "frame-link",
    "file-history-snapshot",
    "file-history-delta",
    "cost-state",
    "tools-used",
    "started",
    "result",
    "failed",
    "launched",
    "continued-in",
];

/// The `system` subtypes that hold nothing.
const MACHINERY_SUBTYPES: &[&str] = &[
    "turn_duration",
    "stop_hook_summary",
    "api_error",
    "informational",
    "local_command",
    "bridge_status",
    "scheduled_task_fire",
];

impl Builder {
    fn new(uuids: HashSet<String>) -> Result<Self, Failure> {
        let mut builder = Builder {
            out: Fragment::empty(),
            facts: HashMap::new(),
            items: Vec::new(),
            nodes: HashMap::new(),
            uuids,
            record_items: HashMap::new(),
            record_parent: HashMap::new(),
            lines: HashMap::new(),
            prefixes: Vec::new(),
            version_at: Vec::new(),
            header_gap: 0,
            headers: HashMap::new(),
            span_gap: 0,
            calls: HashMap::new(),
            call_records: HashMap::new(),
            messages: HashMap::new(),
            request_drops: HashMap::new(),
            thinking_drops: Vec::new(),
            boundaries: HashMap::new(),
            anchors: HashMap::new(),
            interrupted: Vec::new(),
            item_glosses: Vec::new(),
            node_glosses: Vec::new(),
            titles: Vec::new(),
            continues: Vec::new(),
            claims: HashMap::new(),
        };
        use v3::fact::{absent, direction::IN, modality::*};
        let gap = |modality, kind| FactCore {
            absent: Some(kind),
            ..core(modality, IN)
        };
        // The settings header she was sampled under (facts.md 1), when the
        // request records nothing of it.
        builder.header_gap =
            builder.item(gap(SYSTEM, absent::BODY), Role::Prefix, None, "<system>")?;
        // Before a system section is recorded: an unknown system prompt and
        // unknown tool definitions.
        let system = builder.item(gap(SYSTEM, absent::BODY), Role::Prefix, None, "<system>")?;
        let tools = builder.item(
            gap(TOOL_DEFINITION, absent::SPAN),
            Role::Prefix,
            None,
            "<system>",
        )?;
        builder.prefixes.push(vec![system, tools]);
        let span = FactCore {
            absent: Some(absent::SPAN),
            ..FactCore::default()
        };
        builder.span_gap = builder.item(span, Role::Gap, None, "<span>")?;
        Ok(builder)
    }

    fn put_fact(&mut self, core: FactCore, path: &str) -> Result<Id, Failure> {
        let built = fact(core).map_err(|_| Failure::new(path, "a fact does not build"))?;
        let id = built.root().expect("a fact is rooted");
        self.facts.entry(id).or_insert_with(|| unexported(built));
        Ok(id)
    }

    fn item(
        &mut self,
        core: FactCore,
        role: Role,
        record: Option<usize>,
        path: &str,
    ) -> Result<usize, Failure> {
        let thinking = core.modality == Some(v3::fact::modality::THINKING);
        let head = core.modality == Some(v3::fact::modality::TOOL_RESULT);
        let fact = self.put_fact(core, path)?;
        self.items.push(Item {
            fact,
            role,
            thinking,
            head,
            part_of: None,
            record,
            original: None,
            ends: Vec::new(),
            responds_to: None,
            unrecorded: false,
        });
        Ok(self.items.len() - 1)
    }

    fn extend(&mut self, tail: Option<Id>, item: usize) -> Id {
        let holds = Fragment::rooted(self.items[item].fact, TribleSet::new());
        let built = node(tail, holds).expect("a node over one fact builds");
        let id = built.root().expect("a node is rooted");
        self.nodes
            .entry(id)
            .or_insert_with(|| (tail, item, unexported(built)));
        if self.items[item].original.is_none() {
            self.items[item].original = Some(id);
        }
        id
    }

    fn build(&mut self, start: Option<Id>, sequence: &[usize]) -> Option<Id> {
        sequence
            .iter()
            .fold(start, |tail, &item| Some(self.extend(tail, item)))
    }

    /// The items of the line ending at `tail`, root first.
    fn sequence(&self, tail: Option<Id>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cursor = tail;
        while let Some(node) = cursor {
            let (previous, item, _) = &self.nodes[&node];
            out.push(*item);
            let previous = *previous;
            cursor = previous;
        }
        out.reverse();
        out
    }

    /// A new root: the header, then the system section.
    fn root(&mut self, version: usize, header: usize) -> Tail {
        let mut sequence = vec![header];
        sequence.extend(self.prefixes[version].clone());
        Tail {
            node: self.build(None, &sequence),
            version,
            header,
        }
    }

    /// A new root after which a span of unknown items stands: what a record
    /// outside the unit held (I1).
    fn gap_root(&mut self, version: usize) -> Tail {
        let mut tail = self.root(version, self.header_gap);
        tail.node = Some(self.extend(tail.node, self.span_gap));
        tail
    }

    /// The same line under another system section or header: the history
    /// re-presented after the new one.
    fn rebuild(&mut self, tail: Tail, version: usize, header: usize) -> Tail {
        if tail.version == version && tail.header == header {
            return tail;
        }
        let mut sequence = vec![header];
        sequence.extend(self.prefixes[version].clone());
        sequence.extend(
            self.sequence(tail.node)
                .into_iter()
                .filter(|&item| self.items[item].role != Role::Prefix),
        );
        Tail {
            node: self.build(None, &sequence),
            version,
            header,
        }
    }

    /// The line a record continues.
    fn parent_line(&mut self, parent: Option<&str>, version: usize) -> Result<Line, Failure> {
        match parent {
            None => Ok(Line::same(self.root(version, self.header_gap))),
            Some(parent) => match self.lines.get(parent) {
                Some(line) => Ok(*line),
                None if self.uuids.contains(parent) => Err(Failure::new(
                    "{user}.parentUuid",
                    "a record's parent is not built before it",
                )),
                None => Ok(Line::same(self.gap_root(version))),
            },
        }
    }

    fn record(
        &mut self,
        at: usize,
        record: &Json,
        members: &[check::Member<'_>],
    ) -> Result<(), Failure> {
        let kind = record.str_at("type").unwrap_or_default();
        let uuid = record.str_at("uuid").map(str::to_owned);
        let parent = record.str_at("parentUuid").map(str::to_owned);
        if let Some(uuid) = &uuid {
            self.record_parent.insert(uuid.clone(), parent.clone());
        }
        match kind {
            // A compaction summary was sent, whatever the transcript flag
            // says (I6); any other transcript-only prompt never was.
            "user"
                if record.is_true("isVisibleInTranscriptOnly")
                    && !record.is_true("isCompactSummary") =>
            {
                self.pass(at, uuid, parent)
            }
            "user" => self.user(at, record, uuid, parent),
            "assistant" => self.assistant(at, record, uuid, parent),
            "attachment" if record.str_at("attachment.type") == Some("thinking_drop") => {
                let request = record.str_at("attachment.requestId").map(str::to_owned);
                let count = match record.at("attachment.newlyDropped.blockCount") {
                    Some(Json::Num(token)) => token.parse::<usize>().ok(),
                    _ => None,
                };
                match (request, count) {
                    (Some(request), Some(count)) => self.thinking_drops.push((request, count)),
                    _ => {
                        return Err(Failure::new(
                            "{attachment}.attachment{thinking_drop}",
                            "a thinking drop names no request or count",
                        ))
                    }
                }
                self.pass(at, uuid, parent)
            }
            "attachment" => self.attachment(at, record, uuid, parent, members),
            "system" => self.system(at, record, uuid, parent),
            "ai-title" => {
                if let Some(title) = record.str_at("aiTitle").filter(|t| !t.is_empty()) {
                    self.titles.push((at, title.to_owned()));
                }
                Ok(())
            }
            kind if MACHINERY_KINDS.contains(&kind) => self.pass(at, uuid, parent),
            _ => Err(Failure::new(
                "<record>",
                "a record kind the importer does not build",
            )),
        }
    }

    /// A record that holds nothing: its line is its parent's.
    fn pass(
        &mut self,
        at: usize,
        uuid: Option<String>,
        parent: Option<String>,
    ) -> Result<(), Failure> {
        if let Some(uuid) = uuid {
            let line = self.parent_line(parent.as_deref(), self.version_at[at])?;
            self.lines.insert(uuid.clone(), line);
            self.record_items.insert(uuid, Vec::new());
        }
        Ok(())
    }

    /// The claims of one record: one occurrence per node it holds (I3).
    fn claims_of(&mut self, at: usize, record: &Json) -> Result<(), Failure> {
        let kind = record.str_at("type").unwrap_or_default();
        let mut claims = WitnessAnnotations::default();
        if let Some(stamp) = record.str_at("timestamp") {
            claims.timestamp.push(time(stamp).ok_or_else(|| {
                Failure::new(&format!("{{{kind}}}.timestamp"), "a time does not parse")
            })?);
        }
        match kind {
            "user" => {
                claims.raw_role = record.str_at("message.role").map(str::to_owned);
                if record.is_true("isMeta") {
                    claims.raw_author.push("isMeta".to_owned());
                }
                if let Some(kind) = record.str_at("origin.kind") {
                    claims.raw_author.push(kind.to_owned());
                }
                claims.compaction_summary = record.is_true("isCompactSummary");
                claims.source_locator = record.str_at("sessionId").map(str::to_owned);
            }
            "assistant" => {
                claims.raw_role = record.str_at("message.role").map(str::to_owned);
                let model = record.str_at("message.model").map(str::to_owned);
                if let Some(model) = &model {
                    if model != "<synthetic>" {
                        claims.generated_by.push(model.clone());
                    }
                }
                claims.raw_model = model;
                claims.source_locator = record.str_at("sessionId").map(str::to_owned);
            }
            "attachment" => {
                claims.raw_role = record.str_at("renderedRole").map(str::to_owned);
                claims.source_locator = record.str_at("sessionId").map(str::to_owned);
            }
            // The table drops the shape's session id.
            "api-request-shape" => {}
            _ => {
                return Err(Failure::new(
                    "<record>",
                    "a record kind whose claims the importer does not build",
                ))
            }
        }
        self.claims.insert(at, claims);
        Ok(())
    }

    /// Extend the lines from `parent` by `items` and remember them as the
    /// record's.
    fn place(
        &mut self,
        at: usize,
        uuid: Option<String>,
        parent: Option<String>,
        items: Vec<usize>,
    ) -> Result<(), Failure> {
        let version = self.version_at[at];
        let line = self.parent_line(parent.as_deref(), version)?;
        self.continue_compaction(parent.as_deref(), &items);
        let mut edited = self.rebuild(line.edited, version, line.edited.header);
        edited.node = self.build(edited.node, &items);
        let unedited = if line.unedited == line.edited {
            edited
        } else {
            let mut unedited = self.rebuild(line.unedited, version, line.unedited.header);
            unedited.node = self.build(unedited.node, &items);
            unedited
        };
        let mut line = Line { edited, unedited };
        if let Some(uuid) = uuid {
            line = self.after_anchor(&uuid, line)?;
            self.lines.insert(uuid.clone(), line);
            self.record_items.insert(uuid, items);
        }
        Ok(())
    }

    fn user(
        &mut self,
        at: usize,
        record: &Json,
        uuid: Option<String>,
        parent: Option<String>,
    ) -> Result<(), Failure> {
        use v3::fact::{direction::IN, modality::*};
        self.claims_of(at, record)?;
        let mut items = Vec::new();
        let mut results = Vec::new();
        let base = "{user}.message.content";
        match record.at("message.content") {
            Some(Json::Str(prompt)) if !prompt.is_empty() => {
                items.push(self.item(text(TEXT, IN, prompt), Role::User, Some(at), base)?);
            }
            // `""` holds nothing; neither does a record without content.
            Some(Json::Str(_)) | None => {}
            Some(Json::Arr(blocks)) => {
                for block in blocks {
                    let kind = block.str_at("type").unwrap_or_default();
                    let path = format!("{base}[]{{{kind}}}");
                    match kind {
                        "tool_result" => {
                            // The rendering table: a result is an error only
                            // when it says so; otherwise it renders ok (I2).
                            let status = match block.get("is_error") {
                                Some(Json::Bool(true)) => v3::fact::status::ERROR,
                                Some(Json::Bool(false)) | None => v3::fact::status::OK,
                                Some(_) => {
                                    return Err(Failure::new(
                                        &format!("{path}.is_error"),
                                        "a result status the importer does not build",
                                    ))
                                }
                            };
                            let head = self.item(
                                FactCore {
                                    status: Some(status),
                                    ..core(TOOL_RESULT, IN)
                                },
                                Role::User,
                                Some(at),
                                &path,
                            )?;
                            if let Some(call) = block.str_at("tool_use_id") {
                                self.items[head].responds_to = self.calls.get(call).copied();
                                results.push(call.to_owned());
                            }
                            items.push(head);
                            let mut parts = Vec::new();
                            match block.get("content") {
                                Some(Json::Str(content)) if !content.is_empty() => {
                                    parts.push(self.item(
                                        text(TEXT, IN, content),
                                        Role::User,
                                        Some(at),
                                        &path,
                                    )?);
                                }
                                Some(Json::Str(_)) | Some(Json::Null) | None => {}
                                Some(Json::Arr(content)) => {
                                    for part in content {
                                        if let Some(item) =
                                            self.shown(part, &format!("{path}.content[]"), at)?
                                        {
                                            parts.push(item);
                                        }
                                    }
                                }
                                Some(_) => {
                                    return Err(Failure::new(
                                        &format!("{path}.content"),
                                        "a result content the importer does not build",
                                    ))
                                }
                            }
                            for &part in &parts {
                                self.items[part].part_of = Some(head);
                            }
                            items.extend(parts);
                        }
                        _ => {
                            if let Some(item) = self.shown(block, &format!("{base}[]"), at)? {
                                items.push(item);
                            }
                        }
                    }
                }
            }
            Some(_) => {
                return Err(Failure::new(
                    base,
                    "a message content the importer does not build",
                ))
            }
        }
        // Correlators: the record that holds the call this result answers,
        // and the call a harness message follows.
        if let Some(source) = record.str_at("sourceToolAssistantUUID") {
            if results.is_empty() {
                return Err(Failure::new(
                    "{user}.sourceToolAssistantUUID",
                    "a call record named by a record that holds no result",
                ));
            }
            for call in &results {
                if let Some(holder) = self.call_records.get(call) {
                    if holder != source {
                        return Err(Failure::new(
                            "{user}.sourceToolAssistantUUID",
                            "a result names another record than the one holding its call",
                        ));
                    }
                }
            }
        }
        if let Some(call) = record.str_at("sourceToolUseID") {
            if !self.calls.contains_key(call) {
                return Err(Failure::new(
                    "{user}.sourceToolUseID",
                    "a harness message follows a call the unit does not hold",
                ));
            }
        }
        if let Some(message) = record.str_at("interruptedMessageId") {
            self.interrupted.push(message.to_owned());
        }
        self.place(at, uuid, parent, items)
    }

    /// One item she was shown in a content list: text, image, document or a
    /// tool reference.
    fn shown(&mut self, block: &Json, prefix: &str, at: usize) -> Result<Option<usize>, Failure> {
        use v3::fact::{direction::IN, modality::*};
        let kind = block.str_at("type").unwrap_or_default();
        let path = format!("{prefix}{{{kind}}}");
        let core = match kind {
            "text" => match block.str_at("text") {
                Some(shown) if !shown.is_empty() => text(TEXT, IN, shown),
                _ => return Ok(None),
            },
            "image" | "document" => FactCore {
                blob: Some(bytes(block.at("source.data"), &path)?),
                media_type: Some(media_type(block.at("source.media_type"), &path)?),
                ..core(if kind == "image" { IMAGE } else { FILE }, IN)
            },
            // The reference loads the named tool's definition where it
            // stands. No definition is recorded for it, so it is a body gap
            // that carries the name (I9).
            "tool_reference" => FactCore {
                tool_name: block.str_at("tool_name").map(str::to_owned),
                absent: Some(v3::fact::absent::BODY),
                ..core(TOOL_DEFINITION, IN)
            },
            _ => {
                return Err(Failure::new(
                    &path,
                    "an item kind the importer does not build",
                ))
            }
        };
        self.item(core, Role::User, Some(at), &path).map(Some)
    }

    fn markers(&self, record: &Json) -> Result<Markers, Failure> {
        let mut markers = Markers::default();
        let path = "{assistant}.message{message}.input_transformations[]";
        for change in record
            .at("message.input_transformations")
            .map(Json::arr)
            .unwrap_or(&[])
        {
            match change.str_at("type") {
                Some("thinking_dropped") => {}
                // The block still reached her: her window is unchanged.
                Some("thinking_mismatch_allowed") => continue,
                _ => {
                    return Err(Failure::new(
                        path,
                        "an input transformation the importer does not build",
                    ))
                }
            }
            let place = change.str_at("path").unwrap_or_default();
            let steps: Vec<&str> = place.split('.').collect();
            let parsed = match steps.as_slice() {
                ["messages", message, "content", block] => message
                    .parse::<usize>()
                    .ok()
                    .zip(block.parse::<usize>().ok()),
                _ => None,
            };
            markers.dropped.push(parsed.ok_or_else(|| {
                Failure::new(
                    "{assistant}.message{message}.input_transformations[]{thinking_dropped}.path",
                    "a dropped block's path does not parse",
                )
            })?);
        }
        let path = "{assistant}.message{message}.context_management.applied_edits[]";
        for edit in record
            .at("message.context_management.applied_edits")
            .map(Json::arr)
            .unwrap_or(&[])
        {
            match (edit.str_at("type"), edit.get("cleared_tool_uses")) {
                (Some("clear_tool_uses_20250919"), Some(Json::Num(token))) => {
                    markers.cleared += token
                        .parse::<usize>()
                        .map_err(|_| Failure::new(path, "a cleared count does not parse"))?;
                }
                _ => {
                    return Err(Failure::new(
                        path,
                        "an applied edit the importer does not build",
                    ))
                }
            }
        }
        Ok(markers)
    }

    /// The window the source marks: the request's unedited history `base`
    /// with this request's own edits applied, and no other (I7).
    fn window(&mut self, base: Tail, markers: &Markers) -> Result<Tail, Failure> {
        let sequence = self.sequence(base.node);
        let mut drop = BTreeSet::new();
        if !markers.dropped.is_empty() {
            // The request's messages: runs of one role after the system
            // section.
            let mut runs: Vec<(Role, Vec<usize>)> = Vec::new();
            for (at, &item) in sequence.iter().enumerate() {
                let role = self.items[item].role;
                if role == Role::Prefix {
                    continue;
                }
                match runs.last_mut() {
                    Some((last, members)) if *last == role => members.push(at),
                    _ => runs.push((role, vec![at])),
                }
            }
            let path =
                "{assistant}.message{message}.input_transformations[]{thinking_dropped}.path";
            for &(message, block) in &markers.dropped {
                if runs[..message.min(runs.len())]
                    .iter()
                    .any(|(role, _)| matches!(role, Role::System | Role::Voiced | Role::Gap))
                {
                    return Err(Failure::new(
                        path,
                        "a window with system turns, harness text or unknown items cannot be indexed",
                    ));
                }
                let at = match runs.get(message) {
                    Some((Role::Assistant, members)) => members.get(block).copied(),
                    _ => None,
                };
                match at {
                    Some(at) if self.items[sequence[at]].thinking => {
                        drop.insert(at);
                    }
                    _ => {
                        return Err(Failure::new(
                            path,
                            "a dropped block is not a thinking block",
                        ))
                    }
                }
            }
        }
        let mut cleared = BTreeSet::new();
        let heads: Vec<usize> = sequence
            .iter()
            .copied()
            .filter(|&item| self.items[item].head)
            .collect();
        if markers.cleared > heads.len() {
            return Err(Failure::new(
                "{assistant}.message{message}.context_management.applied_edits[]{clear_tool_uses_20250919}.cleared_tool_uses",
                "more results cleared than the window holds",
            ));
        }
        cleared.extend(heads.into_iter().take(markers.cleared));
        let mut window = Vec::new();
        for (at, &item) in sequence.iter().enumerate() {
            if drop.contains(&at) {
                continue;
            }
            if self.items[item]
                .part_of
                .is_some_and(|head| cleared.contains(&head))
            {
                continue;
            }
            window.push(item);
            if cleared.contains(&item) {
                let gap = FactCore {
                    absent: Some(v3::fact::absent::BODY),
                    ..core(v3::fact::modality::TEXT, v3::fact::direction::IN)
                };
                let gap = self.item(gap, Role::User, None, "<cleared>")?;
                window.push(gap);
            }
        }
        Ok(Tail {
            node: self.build(None, &window),
            ..base
        })
    }

    /// The settings header a request was sampled under (S1).
    fn header(&mut self, record: &Json) -> Result<usize, Failure> {
        let Some(spelling) = record.str_at("effort") else {
            return Ok(self.header_gap);
        };
        let term = v3::fact::effort::SPECS
            .iter()
            .find(|(_, name)| *name == spelling)
            .map(|(term, _)| *term)
            .ok_or_else(|| {
                Failure::new(
                    "{assistant}.effort",
                    "an effort the vocabulary does not name",
                )
            })?;
        if let Some(&item) = self.headers.get(&term) {
            return Ok(item);
        }
        let header = FactCore {
            absent: Some(v3::fact::absent::BODY),
            effort: Some(term),
            ..core(v3::fact::modality::SYSTEM, v3::fact::direction::IN)
        };
        let item = self.item(header, Role::Prefix, None, "{assistant}.effort")?;
        self.headers.insert(term, item);
        Ok(item)
    }

    fn assistant(
        &mut self,
        at: usize,
        record: &Json,
        uuid: Option<String>,
        parent: Option<String>,
    ) -> Result<(), Failure> {
        use v3::fact::{direction::*, modality::*};
        self.claims_of(at, record)?;
        let synthetic = record.str_at("message.model") == Some("<synthetic>");
        let model = record.str_at("message.model").unwrap_or_default();
        let message_id = record.str_at("message.id").unwrap_or_default().to_owned();
        let base = "{assistant}.message{message}.content[]";
        let mut items = Vec::new();
        for block in record.at("message.content").map(Json::arr).unwrap_or(&[]) {
            let kind = block.str_at("type").unwrap_or_default();
            let path = format!("{base}{{{kind}}}");
            match kind {
                "text" => {
                    let Some(said) = block.str_at("text").filter(|t| !t.is_empty()) else {
                        continue;
                    };
                    if synthetic {
                        let voiced = FactCore {
                            form: Some(v3::fact::form::VOICED),
                            ..text(TEXT, IN, said)
                        };
                        items.push(self.item(voiced, Role::Voiced, Some(at), &path)?);
                    } else {
                        items.push(self.item(
                            text(TEXT, OUT, said),
                            Role::Assistant,
                            Some(at),
                            &path,
                        )?);
                    }
                }
                "thinking" | "redacted_thinking" if !synthetic => {
                    let (field, spelling) = if kind == "thinking" {
                        ("signature", sealed::ANTHROPIC_THINKING_SIGNATURE)
                    } else {
                        ("data", sealed::ANTHROPIC_REDACTED_THINKING)
                    };
                    let sealed = FactCore {
                        blob: Some(bytes(block.get(field), &path)?),
                        media_type: Some(sealed_type(spelling)),
                        ..core(THINKING, OUT)
                    };
                    let item = self.item(sealed, Role::Assistant, Some(at), &path)?;
                    if let Some(readable) = block.str_at("thinking").filter(|t| !t.is_empty()) {
                        if !SUMMARIZED_THINKING
                            .iter()
                            .any(|family| model.starts_with(family))
                        {
                            return Err(Failure::new(
                                &format!("{path}.thinking"),
                                "readable thinking of a model the gloss table does not name",
                            ));
                        }
                        self.item_glosses.push((
                            item,
                            v3::gloss::category::REASONING_SUMMARY,
                            readable.to_owned(),
                            THINKING_SUMMARIZER,
                        ));
                    }
                    items.push(item);
                }
                "tool_use" if !synthetic => {
                    let call = FactCore {
                        tool_name: block.str_at("name").map(str::to_owned),
                        structure: Some(structure(
                            block
                                .get("input")
                                .ok_or_else(|| Failure::new(&path, "a call has no input"))?,
                            &path,
                        )?),
                        ..core(TOOL_CALL, OUT)
                    };
                    let item = self.item(call, Role::Assistant, Some(at), &path)?;
                    if let Some(id) = block.str_at("id") {
                        self.calls.insert(id.to_owned(), item);
                        if let Some(uuid) = &uuid {
                            self.call_records.insert(id.to_owned(), uuid.clone());
                        }
                    }
                    items.push(item);
                }
                _ => {
                    return Err(Failure::new(
                        &path,
                        "a block kind the importer does not build",
                    ))
                }
            }
        }
        if let Some(&last) = items.last() {
            if !synthetic {
                let ends = record.str_at("message.stop_reason").map(|reason| {
                    v3::node::ends::SPECS
                        .iter()
                        .find(|(_, name, _)| *name == reason)
                        .map(|(term, _, _)| *term)
                });
                match ends {
                    Some(Some(term)) => self.items[last].ends.push(term),
                    Some(None) => {
                        return Err(Failure::new(
                            "{assistant}.message{message}.stop_reason",
                            "a stop reason the vocabulary does not name",
                        ))
                    }
                    None => {}
                }
            }
            if record.is_true("isApiErrorMessage") {
                self.items[last].ends.push(v3::node::ends::ERROR);
            }
        }
        if let Some(request) = record.str_at("requestId") {
            let drops = record
                .at("message.input_transformations")
                .map(Json::arr)
                .unwrap_or(&[])
                .iter()
                .filter(|change| change.str_at("type") == Some("thinking_dropped"))
                .count();
            let entry = self.request_drops.entry(request.to_owned()).or_default();
            *entry = (*entry).max(drops);
        }
        let markers = self.markers(record)?;
        let header = self.header(record)?;
        let version = self.version_at[at];

        // One API message spans the records that share its id; its window
        // is decided once, from the unedited history at the records'
        // common base.
        let continuing = parent
            .as_deref()
            .and_then(|p| self.messages.get(&message_id).map(|m| (p, m)))
            .is_some_and(|(p, m)| m.records.iter().any(|(u, _)| u.as_deref() == Some(p)));
        if !continuing {
            let line = self.parent_line(parent.as_deref(), version)?;
            let base = self.rebuild(line.unedited, version, header);
            self.messages.insert(
                message_id.clone(),
                Message {
                    base,
                    records: Vec::new(),
                    markers: None,
                    last_item: None,
                },
            );
        }
        let mut message = self
            .messages
            .remove(&message_id)
            .expect("the message is staged");
        if message.base.header != header {
            return Err(Failure::new(
                "{assistant}.effort",
                "the records of one message name different efforts",
            ));
        }
        self.continue_compaction(parent.as_deref(), &items);
        let from = match parent.as_deref() {
            Some(parent) if continuing => self.lines[parent],
            _ => Line::same(message.base),
        };
        let mut edited = self.rebuild(from.edited, version, header);
        let mut unedited = self.rebuild(from.unedited, version, header);
        if !markers.is_empty() {
            match &message.markers {
                Some(seen) if *seen != markers => {
                    return Err(Failure::new(
                        "{assistant}.message{message}",
                        "the records of one message mark different windows",
                    ))
                }
                Some(_) => {}
                None => {
                    message.markers = Some(markers.clone());
                    edited = self.window(message.base, &markers)?;
                    for (uuid, earlier) in &message.records {
                        edited.node = self.build(edited.node, earlier);
                        if let Some(uuid) = uuid {
                            if let Some(line) = self.lines.get_mut(uuid) {
                                line.edited = edited;
                            }
                        }
                    }
                }
            }
        }
        let same = edited == unedited;
        edited.node = self.build(edited.node, &items);
        if same {
            unedited = edited;
        } else {
            unedited.node = self.build(unedited.node, &items);
        }
        let mut line = Line { edited, unedited };
        if let Some(uuid) = &uuid {
            line = self.after_anchor(uuid, line)?;
            self.lines.insert(uuid.clone(), line);
            self.record_items.insert(uuid.clone(), items.clone());
        }
        if let Some(&last) = items.last() {
            message.last_item = Some(last);
        }
        message.records.push((uuid, items));
        self.messages.insert(message_id, message);
        Ok(())
    }

    fn attachment(
        &mut self,
        at: usize,
        record: &Json,
        uuid: Option<String>,
        parent: Option<String>,
        members: &[check::Member<'_>],
    ) -> Result<(), Failure> {
        use v3::fact::{direction::IN, modality::*};
        self.claims_of(at, record)?;
        if record.get("rendered").is_none() && record.get("renderedInHumanTurn").is_none() {
            let items = self.unrecorded(at, members)?;
            return self.place(at, uuid, parent, items);
        }
        let rendered = record.at("rendered").map(Json::arr).unwrap_or(&[]);
        let (modality, role) = match record.str_at("renderedRole") {
            Some("system") => (SYSTEM, Role::System),
            Some("user") => (TEXT, Role::User),
            None if rendered.is_empty() => (TEXT, Role::User),
            _ => {
                return Err(Failure::new(
                    "{attachment}.renderedRole",
                    "a rendered role the importer does not build",
                ))
            }
        };
        let mut items = Vec::new();
        for shown in rendered {
            if let Some(content) = shown.str_at("content").filter(|c| !c.is_empty()) {
                items.push(self.item(
                    text(modality, IN, content),
                    role,
                    Some(at),
                    "{attachment}.rendered[].content",
                )?);
            }
        }
        for shown in record
            .at("renderedInHumanTurn")
            .map(Json::arr)
            .unwrap_or(&[])
        {
            if let Some(content) = shown.str_at("content").filter(|c| !c.is_empty()) {
                items.push(self.item(
                    text(TEXT, IN, content),
                    Role::User,
                    Some(at),
                    "{attachment}.renderedInHumanTurn[].content",
                )?);
            }
        }
        self.place(at, uuid, parent, items)
    }

    /// An attachment recorded before its rendering was (S3): the members
    /// the table names become `in` event facts, one per object, in the
    /// walk's order, and their nodes are marked `rendering: unrecorded`.
    fn unrecorded(
        &mut self,
        at: usize,
        members: &[check::Member<'_>],
    ) -> Result<Vec<usize>, Failure> {
        let path = "{attachment}.attachment";
        let mut groups: BTreeMap<&[(String, usize)], Vec<&check::Member<'_>>> = BTreeMap::new();
        for member in members {
            if member.modality != "event" || member.direction != "in" {
                return Err(Failure::new(
                    path,
                    "a structure member the importer does not build",
                ));
            }
            groups.entry(&member.levels).or_default().push(member);
        }
        if groups.is_empty() {
            return Err(Failure::new(
                path,
                "an unrecorded attachment that holds nothing",
            ));
        }
        let mut items = Vec::new();
        for (_, group) in groups {
            let mut object = Json::Obj(Vec::new());
            for member in group {
                let steps: Vec<&str> = member.pointer.split('/').skip(1).collect();
                insert(&mut object, &steps, member.value).map_err(|why| Failure::new(path, why))?;
            }
            let core = FactCore {
                structure: Some(structure(&object, path)?),
                ..core(v3::fact::modality::EVENT, v3::fact::direction::IN)
            };
            let item = self.item(core, Role::User, Some(at), path)?;
            self.items[item].unrecorded = true;
            items.push(item);
        }
        Ok(items)
    }

    fn system(
        &mut self,
        at: usize,
        record: &Json,
        uuid: Option<String>,
        parent: Option<String>,
    ) -> Result<(), Failure> {
        match record.str_at("subtype") {
            Some("compact_boundary") => {
                let Some(uuid) = uuid else {
                    return Err(Failure::new(
                        "{system}.uuid",
                        "a compaction boundary has no id",
                    ));
                };
                // The context this one replaces: `continues` needs it (I6).
                let logical_parent = record
                    .str_at("logicalParentUuid")
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        Failure::new(
                            "{system}.logicalParentUuid",
                            "a compaction boundary names no context it replaces",
                        )
                    })?;
                let segment = match record.at("compactMetadata.preservedSegment") {
                    Some(segment) => {
                        let field = |name: &str| {
                            segment.str_at(name).map(str::to_owned).ok_or_else(|| {
                                Failure::new(
                                    "{system}.compactMetadata.preservedSegment",
                                    "a preserved segment is incomplete",
                                )
                            })
                        };
                        let (head, tail, anchor) =
                            (field("headUuid")?, field("tailUuid")?, field("anchorUuid")?);
                        let listed = |name: &str| -> Option<Vec<String>> {
                            record
                                .at(&format!("compactMetadata.preservedMessages.{name}"))
                                .map(|ids| {
                                    ids.arr()
                                        .iter()
                                        .filter_map(|id| id.str().map(str::to_owned))
                                        .collect()
                                })
                        };
                        let preserved = listed("uuids").unwrap_or_default();
                        // `allUuids` may only name what `uuids` names: any
                        // other preserved record is one this unit cannot
                        // re-present.
                        if let Some(all) = listed("allUuids") {
                            if all != preserved {
                                return Err(Failure::new(
                                    "{system}.compactMetadata.preservedMessages.allUuids[]",
                                    "preserved messages the preserved segment does not hold",
                                ));
                            }
                        }
                        if let Some(listed_anchor) =
                            record.str_at("compactMetadata.preservedMessages.anchorUuid")
                        {
                            if listed_anchor != anchor {
                                return Err(Failure::new(
                                    "{system}.compactMetadata.preservedMessages.anchorUuid",
                                    "the preserved messages name another anchor than the segment",
                                ));
                            }
                        }
                        self.anchors.insert(anchor, uuid.clone());
                        Some((head, tail, preserved))
                    }
                    None => {
                        if record.at("compactMetadata.preservedMessages").is_some() {
                            return Err(Failure::new(
                                "{system}.compactMetadata.preservedMessages",
                                "preserved messages without a preserved segment",
                            ));
                        }
                        None
                    }
                };
                // The boundary's children start a new root. When the context
                // it replaces is outside the unit, a span of unknown items
                // stands before the summary.
                let version = self.version_at[at];
                let tail = if self.uuids.contains(&logical_parent) {
                    self.root(version, self.header_gap)
                } else {
                    self.gap_root(version)
                };
                self.boundaries.insert(
                    uuid.clone(),
                    Boundary {
                        logical_parent,
                        segment,
                    },
                );
                self.lines.insert(uuid.clone(), Line::same(tail));
                self.record_items.insert(uuid, Vec::new());
                Ok(())
            }
            Some("away_summary") => {
                let recap = record.str_at("content").unwrap_or_default();
                if !recap.is_empty() {
                    // The nearest node on its own record's parent chain (I5).
                    let node = parent
                        .as_deref()
                        .and_then(|p| self.lines.get(p))
                        .and_then(|line| line.edited.node)
                        .ok_or_else(|| {
                            Failure::new("{system}.content", "a recap with no node to describe")
                        })?;
                    self.node_glosses.push((
                        node,
                        v3::gloss::category::RECAP,
                        recap.to_owned(),
                        RECAP_MODEL,
                    ));
                }
                self.pass(at, uuid, parent)
            }
            Some(subtype) if MACHINERY_SUBTYPES.contains(&subtype) => self.pass(at, uuid, parent),
            _ => Err(Failure::new(
                "{system}.subtype",
                "a system record the importer does not build",
            )),
        }
    }

    /// A record whose parent is a compaction boundary opens the new context:
    /// its first item `continues` from the old tail.
    fn continue_compaction(&mut self, parent: Option<&str>, items: &[usize]) {
        let Some(parent) = parent else {
            return;
        };
        if let (true, Some(&first)) = (self.boundaries.contains_key(parent), items.first()) {
            self.continues.push((first, parent.to_owned()));
        }
    }

    /// After the anchor of a preserved segment, the segment is re-presented.
    fn after_anchor(&mut self, uuid: &str, mut line: Line) -> Result<Line, Failure> {
        let Some(boundary) = self.anchors.get(uuid).cloned() else {
            return Ok(line);
        };
        let Some((head, last, preserved)) = self.boundaries[&boundary].segment.clone() else {
            return Ok(line);
        };
        let path = "{system}.compactMetadata.preservedSegment";
        let mut chain = Vec::new();
        let mut cursor = Some(last);
        while let Some(at) = cursor {
            chain.push(at.clone());
            if at == head {
                break;
            }
            cursor = self.record_parent.get(&at).cloned().flatten();
            if cursor.is_none() {
                return Err(Failure::new(
                    path,
                    "the preserved segment does not reach its head",
                ));
            }
        }
        chain.reverse();
        if chain != preserved {
            return Err(Failure::new(
                "{system}.compactMetadata.preservedMessages",
                "the preserved messages differ from the preserved segment",
            ));
        }
        let mut sequence = Vec::new();
        for at in &chain {
            let items = self.record_items.get(at).ok_or_else(|| {
                Failure::new(path, "a preserved record is not built before its anchor")
            })?;
            sequence.extend(items.iter().copied());
        }
        let same = line.edited == line.unedited;
        line.edited.node = self.build(line.edited.node, &sequence);
        if same {
            line.unedited = line.edited;
        } else {
            line.unedited.node = self.build(line.unedited.node, &sequence);
        }
        Ok(line)
    }

    fn shape(&mut self, at: usize, record: &Json) -> Result<(), Failure> {
        use v3::fact::{direction::IN, modality::*};
        self.claims_of(at, record)?;
        let mut prefix = Vec::new();
        for block in record.at("shape.system").map(Json::arr).unwrap_or(&[]) {
            if let Some(said) = block.str_at("text").filter(|t| !t.is_empty()) {
                prefix.push(self.item(
                    text(SYSTEM, IN, said),
                    Role::Prefix,
                    Some(at),
                    "{api-request-shape}.shape.system[]{text}.text",
                )?);
            }
        }
        for tool in record.at("shape.tools").map(Json::arr).unwrap_or(&[]) {
            let path = "{api-request-shape}.shape.tools[]";
            let definition = FactCore {
                tool_name: tool.str_at("name").map(str::to_owned),
                payload: tool
                    .str_at("description")
                    .filter(|d| !d.is_empty())
                    .map(str::to_owned),
                structure: match tool.get("input_schema") {
                    Some(schema) => Some(structure(schema, path)?),
                    None => None,
                },
                ..core(TOOL_DEFINITION, IN)
            };
            prefix.push(self.item(definition, Role::Prefix, Some(at), path)?);
        }
        if prefix.is_empty() {
            return Err(Failure::new(
                "{api-request-shape}.shape",
                "a shape that holds nothing",
            ));
        }
        self.prefixes.push(prefix);
        // The shape's own window: it opens with no request's settings.
        let version = self.prefixes.len() - 1;
        self.root(version, self.header_gap);
        Ok(())
    }

    fn finish(mut self, records: &[Json]) -> Result<Fragment, Failure> {
        // A thinking drop the client reported must be one the API marked.
        for (request, count) in &self.thinking_drops {
            if self.request_drops.get(request).copied().unwrap_or(0) < *count {
                return Err(Failure::new(
                    "{attachment}.attachment{thinking_drop}",
                    "a reported thinking drop the message does not mark",
                ));
            }
        }
        for message in std::mem::take(&mut self.interrupted) {
            let last = self
                .messages
                .get(&message)
                .and_then(|m| m.last_item)
                .ok_or_else(|| {
                    Failure::new(
                        "{user}.interruptedMessageId",
                        "the interrupted message is not in the unit",
                    )
                })?;
            self.items[last].ends.push(v3::node::ends::INTERRUPTED);
        }
        // The windows: every node on the line of a node a record holds. A
        // line built along the way that no record's window holds (the
        // unedited history no later request used) is not emitted.
        let mut emitted: HashSet<Id> = HashSet::new();
        for item in &self.items {
            let (Some(_), Some(node)) = (item.record, item.original) else {
                continue;
            };
            let mut cursor = Some(node);
            while let Some(at) = cursor {
                if !emitted.insert(at) {
                    break;
                }
                cursor = self.nodes[&at].0;
            }
        }
        let mut held_facts: HashSet<Id> = HashSet::new();
        for &node in &emitted {
            let (_, item, built) = &self.nodes[&node];
            self.out += built.clone();
            let item = &self.items[*item];
            if held_facts.insert(item.fact) {
                self.out += self.facts[&item.fact].clone();
            }
            if let Some(original) = item
                .original
                .filter(|original| *original != node && emitted.contains(original))
            {
                self.out += annotate_node(
                    node,
                    &NodeAnnotations {
                        recalls: vec![original],
                        ..NodeAnnotations::default()
                    },
                );
            }
        }
        let on_window = |node: Id, path: &str| -> Result<Id, Failure> {
            if emitted.contains(&node) {
                Ok(node)
            } else {
                Err(Failure::new(path, "a link to a node no window holds"))
            }
        };
        // Every compaction root whose old context is in the unit continues
        // from its tail (I6).
        let continues = std::mem::take(&mut self.continues);
        let mut continued: HashSet<String> = HashSet::new();
        for (first, boundary) in continues {
            let old = &self.boundaries[&boundary].logical_parent;
            if !self.uuids.contains(old) {
                continue;
            }
            let tail = self.lines.get(old).and_then(|line| line.edited.node);
            let (Some(node), Some(tail)) = (self.items[first].original, tail) else {
                return Err(Failure::new(
                    "{system}.logicalParentUuid",
                    "a compaction root that continues no node",
                ));
            };
            let tail = on_window(tail, "{system}.logicalParentUuid")?;
            self.out += annotate_node(
                node,
                &NodeAnnotations {
                    continues: vec![tail],
                    ..NodeAnnotations::default()
                },
            );
            continued.insert(boundary);
        }
        for (uuid, boundary) in &self.boundaries {
            if self.uuids.contains(&boundary.logical_parent) && !continued.contains(uuid) {
                return Err(Failure::new(
                    "{system}.logicalParentUuid",
                    "a compaction whose new context holds nothing that continues",
                ));
            }
        }
        // Node annotations.
        for at in 0..self.items.len() {
            let Some(node) = self.items[at].original else {
                continue;
            };
            let responds_to: Vec<Id> = self.items[at]
                .responds_to
                .and_then(|call| self.items[call].original)
                .into_iter()
                .collect();
            let ends = self.items[at].ends.clone();
            if !responds_to.is_empty() || !ends.is_empty() {
                self.out += annotate_node(
                    node,
                    &NodeAnnotations {
                        responds_to,
                        ends,
                        ..NodeAnnotations::default()
                    },
                );
            }
        }
        // Every node holding an unrecorded rendering says so, copies too.
        let unrecorded: Vec<Id> = emitted
            .iter()
            .filter(|node| self.items[self.nodes[*node].1].unrecorded)
            .copied()
            .collect();
        for node in unrecorded {
            self.out += annotate_node(
                node,
                &NodeAnnotations {
                    rendering: vec![v3::node::rendering::UNRECORDED],
                    ..NodeAnnotations::default()
                },
            );
        }
        // Witnesses: one per node a record holds, and one occurrence of it
        // per record, holding that record's claims together.
        let mut held: BTreeMap<usize, Vec<Id>> = BTreeMap::new();
        for item in &self.items {
            if let (Some(record), Some(node)) = (item.record, item.original) {
                let nodes = held.entry(record).or_default();
                if !nodes.contains(&node) {
                    nodes.push(node);
                }
            }
        }
        for (record, nodes) in held {
            let claims = self.claims.get(&record).cloned().unwrap_or_default();
            for node in nodes {
                let built = witness(SOURCE_CLAUDE_CODE, node)
                    .map_err(|_| Failure::new("<witness>", "a witness does not build"))?;
                let id = built.root().expect("a witness is rooted");
                self.out += unexported(built);
                let built = occurrence(id, claims.clone()).map_err(|_| {
                    Failure::new("<witness>", "a witness occurrence does not build")
                })?;
                self.out += unexported(built);
            }
        }
        // Titles: the nearest node on the parent chain of the record before
        // the title in the file (I5).
        for (at, title) in std::mem::take(&mut self.titles) {
            let node = records[..at]
                .iter()
                .rev()
                .find_map(|before| before.str_at("uuid"))
                .and_then(|uuid| self.lines.get(uuid))
                .and_then(|line| line.edited.node)
                .ok_or_else(|| {
                    Failure::new("{ai-title}.aiTitle", "a title with no node to describe")
                })?;
            self.node_glosses
                .push((node, v3::gloss::category::TITLE, title, TITLE_MODEL));
        }
        // Glosses.
        let mut glosses = std::mem::take(&mut self.node_glosses);
        for (item, category, text, model) in std::mem::take(&mut self.item_glosses) {
            if let Some(node) = self.items[item].original {
                glosses.push((node, category, text, model));
            }
        }
        for (node, category, text, model) in glosses {
            let node = on_window(node, "<gloss>")?;
            let built = gloss(node, category, &text)
                .map_err(|_| Failure::new("<gloss>", "a gloss does not build"))?;
            let id = built.root().expect("a gloss is rooted");
            self.out += unexported(built);
            self.out += annotate_gloss(id, model)
                .map_err(|_| Failure::new("<gloss>", "a gloss annotation does not build"))?;
        }
        Ok(self.out)
    }
}
