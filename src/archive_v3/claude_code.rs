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
//! record's parent (`parentUuid`). A window starts with what the system
//! section held: the recorded system prompt and tool definitions
//! (`api-request-shape`), or, before one is recorded, a `system` body gap and
//! a `tool_definition` span gap. When a recorded shape appears, the next
//! window is rebuilt on it, its earlier items re-presented as copies that
//! `recalls` their originals.
//!
//! The window an `out` fact was sampled from is exactly what the source
//! marks:
//!
//! - `input_transformations` of type `thinking_dropped` remove the named
//!   thinking blocks (`messages.N.content.M`);
//! - `context_management.applied_edits` of type `clear_tool_uses_20250919`
//!   replace the content of the oldest N results with a body gap (the
//!   server's placeholder text is unpublished);
//! - a compaction boundary starts a new root whose first node `continues`
//!   from the old tail, and its preserved segment is re-presented after the
//!   summary.
//!
//! A window that differs branches at the first difference; node ids are
//! content addresses, so shared prefixes are shared nodes. Rules the source
//! does not mark (old thinking stripped per model) are never applied here.
//!
//! Each node a record holds gets a witness with the record's time, role and
//! model; leaves also carry the session id. Readable thinking, titles and
//! away recaps are glosses.

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
    annotate_gloss, annotate_node, annotate_witness, fact, gloss, node, unexported, witness,
    FactCore, MediaType, NodeAnnotations, WitnessAnnotations,
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
        check::record(&TABLE, &record)?;
        records.push(record);
    }
    let mut builder = Builder::default();
    let mut first: HashMap<String, usize> = HashMap::new();
    for (at, record) in records.iter().enumerate() {
        if let Some(uuid) = record.str_at("uuid") {
            match first.get(uuid) {
                // The same record written again: only what the table drops
                // may differ.
                Some(&earlier) => {
                    if !same_kept(&records[earlier], record) {
                        let kind = record.str_at("type").unwrap_or_default();
                        return Err(Failure::new(
                            &format!("{{{kind}}}.uuid"),
                            "a record id repeats with other content",
                        ));
                    }
                    continue;
                }
                None => {
                    first.insert(uuid.to_owned(), at);
                }
            }
        }
        builder.record(at, record)?;
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
    /// The system section: system prompt and tool definitions.
    Prefix,
    User,
    Assistant,
    /// A system-role turn among the messages.
    System,
    /// Harness text in her slot.
    Voiced,
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
}

#[derive(Clone, Copy, Debug, Default)]
struct Tail {
    node: Option<Id>,
    /// The system section the line was built on.
    version: usize,
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

#[derive(Debug, Default)]
struct Message {
    /// The tail the message's first record hangs on, as the source holds it.
    base: Tail,
    /// Its records so far, with their uuids and items.
    records: Vec<(Option<String>, Vec<usize>)>,
    markers: Option<Markers>,
    last_item: Option<usize>,
}

struct Boundary {
    logical_parent: Option<String>,
    /// `(head, tail, preserved uuids)`.
    segment: Option<(String, String, Vec<String>)>,
}

#[derive(Default)]
struct Builder {
    out: Fragment,
    facts: HashSet<Id>,
    items: Vec<Item>,
    /// node -> (previous, item)
    nodes: HashMap<Id, (Option<Id>, usize)>,
    order: Vec<Id>,
    recalled: HashSet<Id>,
    /// What each record contributed: its own items, and the line it ends.
    record_items: HashMap<String, Vec<usize>>,
    record_parent: HashMap<String, Option<String>>,
    tails: HashMap<String, Tail>,
    prefix: Vec<usize>,
    version: usize,
    calls: HashMap<String, usize>,
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
    /// `(first item of the new context, old tail uuid)`
    continues: Vec<(usize, String)>,
    witnesses: HashMap<usize, WitnessAnnotations>,
    sessions: HashMap<usize, String>,
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

impl Builder {
    fn put_fact(&mut self, core: FactCore, path: &str) -> Result<Id, Failure> {
        let built = fact(core).map_err(|_| Failure::new(path, "a fact does not build"))?;
        let id = built.root().expect("a fact is rooted");
        if self.facts.insert(id) {
            self.out += unexported(built);
        }
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
        });
        Ok(self.items.len() - 1)
    }

    fn extend(&mut self, tail: Option<Id>, item: usize) -> Id {
        let holds = Fragment::rooted(self.items[item].fact, TribleSet::new());
        let built = node(tail, holds).expect("a node over one fact builds");
        let id = built.root().expect("a node is rooted");
        if !self.nodes.contains_key(&id) {
            self.nodes.insert(id, (tail, item));
            self.order.push(id);
            self.out += unexported(built);
        }
        match self.items[item].original {
            None => self.items[item].original = Some(id),
            Some(original) if original != id && self.recalled.insert(id) => {
                self.out += annotate_node(
                    id,
                    &NodeAnnotations {
                        recalls: vec![original],
                        ..NodeAnnotations::default()
                    },
                );
            }
            Some(_) => {}
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
            let (previous, item) = self.nodes[&node];
            out.push(item);
            cursor = previous;
        }
        out.reverse();
        out
    }

    fn ensure_prefix(&mut self) -> Result<(), Failure> {
        if self.prefix.is_empty() {
            let system = FactCore {
                absent: Some(v3::fact::absent::BODY),
                ..core(v3::fact::modality::SYSTEM, v3::fact::direction::IN)
            };
            let tools = FactCore {
                absent: Some(v3::fact::absent::SPAN),
                ..core(v3::fact::modality::TOOL_DEFINITION, v3::fact::direction::IN)
            };
            let system = self.item(system, Role::Prefix, None, "<system>")?;
            let tools = self.item(tools, Role::Prefix, None, "<system>")?;
            self.prefix = vec![system, tools];
        }
        Ok(())
    }

    fn root(&mut self) -> Result<Tail, Failure> {
        self.ensure_prefix()?;
        let prefix = self.prefix.clone();
        Ok(Tail {
            node: self.build(None, &prefix),
            version: self.version,
        })
    }

    /// The line a record with `parent` continues, on the current system
    /// section.
    fn line(&mut self, parent: Option<&str>) -> Result<Tail, Failure> {
        let Some(tail) = parent.and_then(|parent| self.tails.get(parent)).copied() else {
            return self.root();
        };
        if tail.version == self.version {
            return Ok(tail);
        }
        let mut sequence = self.prefix.clone();
        sequence.extend(
            self.sequence(tail.node)
                .into_iter()
                .filter(|&item| self.items[item].role != Role::Prefix),
        );
        Ok(Tail {
            node: self.build(None, &sequence),
            version: self.version,
        })
    }

    fn record(&mut self, at: usize, record: &Json) -> Result<(), Failure> {
        let kind = record.str_at("type").unwrap_or_default();
        let uuid = record.str_at("uuid").map(str::to_owned);
        let parent = record.str_at("parentUuid").map(str::to_owned);
        if let Some(uuid) = &uuid {
            self.record_parent.insert(uuid.clone(), parent.clone());
        }
        match kind {
            "user" if !record.is_true("isVisibleInTranscriptOnly") => {
                self.user(at, record, uuid, parent)
            }
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
                self.pass(uuid, parent)
            }
            "attachment" => self.attachment(at, record, uuid, parent),
            "system" => self.system(record, uuid, parent),
            "ai-title" => {
                let title = record.str_at("aiTitle").unwrap_or_default();
                if !title.is_empty() {
                    let latest = *self.order.last().ok_or_else(|| {
                        Failure::new("{ai-title}.aiTitle", "a title with no node to describe")
                    })?;
                    self.node_glosses.push((
                        latest,
                        v3::gloss::category::TITLE,
                        title.to_owned(),
                        TITLE_MODEL,
                    ));
                }
                Ok(())
            }
            "api-request-shape" => self.shape(at, record),
            _ => self.pass(uuid, parent),
        }
    }

    /// A record that holds nothing: its line is its parent's.
    fn pass(&mut self, uuid: Option<String>, parent: Option<String>) -> Result<(), Failure> {
        if let Some(uuid) = uuid {
            let tail = match parent.as_deref().and_then(|p| self.tails.get(p)) {
                Some(tail) => *tail,
                None => self.root()?,
            };
            self.tails.insert(uuid.clone(), tail);
            self.record_items.insert(uuid, Vec::new());
        }
        Ok(())
    }

    fn witness_of(&mut self, at: usize, record: &Json) -> Result<(), Failure> {
        let kind = record.str_at("type").unwrap_or_default();
        let mut annotations = WitnessAnnotations::default();
        if let Some(stamp) = record.str_at("timestamp") {
            annotations.timestamp.push(time(stamp).ok_or_else(|| {
                Failure::new(&format!("{{{kind}}}.timestamp"), "a time does not parse")
            })?);
        }
        match kind {
            "user" => {
                annotations.raw_role = record.str_at("message.role").map(str::to_owned);
                if record.is_true("isMeta") {
                    annotations.raw_author.push("isMeta".to_owned());
                }
                if let Some(kind) = record.str_at("origin.kind") {
                    annotations.raw_author.push(kind.to_owned());
                }
                annotations.compaction_summary = record.is_true("isCompactSummary");
            }
            "assistant" => {
                annotations.raw_role = record.str_at("message.role").map(str::to_owned);
                let model = record.str_at("message.model").map(str::to_owned);
                if let Some(model) = &model {
                    if model != "<synthetic>" {
                        annotations.generated_by.push(model.clone());
                    }
                }
                annotations.raw_model = model;
            }
            "attachment" => {
                annotations.raw_role = record.str_at("renderedRole").map(str::to_owned);
            }
            _ => {}
        }
        self.witnesses.insert(at, annotations);
        if let Some(session) = record.str_at("sessionId") {
            if kind != "api-request-shape" {
                self.sessions.insert(at, session.to_owned());
            }
        }
        Ok(())
    }

    /// Extend the line from `parent` by `items` and remember it as the
    /// record's.
    fn place(
        &mut self,
        uuid: Option<String>,
        parent: Option<String>,
        items: Vec<usize>,
    ) -> Result<Option<Id>, Failure> {
        let mut tail = self.line(parent.as_deref())?;
        self.continue_compaction(parent.as_deref(), &items)?;
        tail.node = self.build(tail.node, &items);
        if let Some(uuid) = &uuid {
            tail = self.after_anchor(uuid, tail)?;
            self.tails.insert(uuid.clone(), tail);
            self.record_items.insert(uuid.clone(), items);
        }
        Ok(tail.node)
    }

    fn user(
        &mut self,
        at: usize,
        record: &Json,
        uuid: Option<String>,
        parent: Option<String>,
    ) -> Result<(), Failure> {
        use v3::fact::{direction::IN, modality::*};
        self.witness_of(at, record)?;
        let mut items = Vec::new();
        let base = "{user}.message.content";
        match record.at("message.content") {
            Some(Json::Str(prompt)) if !prompt.is_empty() => {
                items.push(self.item(text(TEXT, IN, prompt), Role::User, Some(at), base)?);
            }
            Some(Json::Arr(blocks)) => {
                for block in blocks {
                    let kind = block.str_at("type").unwrap_or_default();
                    let path = format!("{base}[]{{{kind}}}");
                    match kind {
                        "tool_result" => {
                            let status = match block.get("is_error") {
                                Some(Json::Bool(true)) => Some(v3::fact::status::ERROR),
                                Some(Json::Bool(false)) => Some(v3::fact::status::OK),
                                _ => None,
                            };
                            let head = self.item(
                                FactCore {
                                    status,
                                    ..core(TOOL_RESULT, IN)
                                },
                                Role::User,
                                Some(at),
                                &path,
                            )?;
                            if let Some(call) = block.str_at("tool_use_id") {
                                self.items[head].responds_to = self.calls.get(call).copied();
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
                                Some(Json::Arr(content)) => {
                                    for part in content {
                                        if let Some(item) =
                                            self.shown(part, &format!("{path}.content[]"), at)?
                                        {
                                            parts.push(item);
                                        }
                                    }
                                }
                                _ => {}
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
            _ => {}
        }
        if let Some(message) = record.str_at("interruptedMessageId") {
            self.interrupted.push(message.to_owned());
        }
        self.place(uuid, parent, items)?;
        Ok(())
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
            "tool_reference" => FactCore {
                tool_name: block.str_at("tool_name").map(str::to_owned),
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
        let path = "{assistant}.message{message}.input_transformations[]{thinking_dropped}.path";
        for change in record
            .at("message.input_transformations")
            .map(Json::arr)
            .unwrap_or(&[])
        {
            if change.str_at("type") != Some("thinking_dropped") {
                continue;
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
            markers.dropped.push(
                parsed
                    .ok_or_else(|| Failure::new(path, "a dropped block's path does not parse"))?,
            );
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

    /// The window the source marks, from the line ending at `base`.
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
                    .any(|(role, _)| matches!(role, Role::System | Role::Voiced))
                {
                    return Err(Failure::new(
                        path,
                        "a window with system turns or harness text cannot be indexed",
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
            version: base.version,
        })
    }

    fn assistant(
        &mut self,
        at: usize,
        record: &Json,
        uuid: Option<String>,
        parent: Option<String>,
    ) -> Result<(), Failure> {
        use v3::fact::{direction::*, modality::*};
        self.witness_of(at, record)?;
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

        // One API message spans the records that share its id; its window
        // is decided once, at the records' common base.
        let continuing = parent
            .as_deref()
            .and_then(|p| self.messages.get(&message_id).map(|m| (p, m)))
            .is_some_and(|(p, m)| m.records.iter().any(|(u, _)| u.as_deref() == Some(p)));
        if !continuing {
            let base = self.line(parent.as_deref())?;
            self.messages.insert(
                message_id.clone(),
                Message {
                    base,
                    ..Message::default()
                },
            );
        }
        let mut message = self
            .messages
            .remove(&message_id)
            .expect("the message is staged");
        self.continue_compaction(parent.as_deref(), &items)?;
        let mut tail = match message.records.last() {
            Some((Some(last), _)) if continuing => self.tails[last],
            _ => message.base,
        };
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
                    tail = self.window(message.base, &markers)?;
                    for (uuid, earlier) in &message.records {
                        tail.node = self.build(tail.node, earlier);
                        if let Some(uuid) = uuid {
                            self.tails.insert(uuid.clone(), tail);
                        }
                    }
                }
            }
        }
        tail.node = self.build(tail.node, &items);
        if let Some(uuid) = &uuid {
            tail = self.after_anchor(uuid, tail)?;
            self.tails.insert(uuid.clone(), tail);
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
    ) -> Result<(), Failure> {
        use v3::fact::{direction::IN, modality::*};
        self.witness_of(at, record)?;
        let (modality, role) = match record.str_at("renderedRole") {
            Some("system") => (SYSTEM, Role::System),
            _ => (TEXT, Role::User),
        };
        let mut items = Vec::new();
        for shown in record.at("rendered").map(Json::arr).unwrap_or(&[]) {
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
        self.place(uuid, parent, items)?;
        Ok(())
    }

    fn system(
        &mut self,
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
                        let preserved: Vec<String> = record
                            .at("compactMetadata.preservedMessages.uuids")
                            .map(Json::arr)
                            .unwrap_or(&[])
                            .iter()
                            .filter_map(|id| id.str().map(str::to_owned))
                            .collect();
                        self.anchors.insert(anchor, uuid.clone());
                        Some((head, tail, preserved))
                    }
                    None => None,
                };
                self.boundaries.insert(
                    uuid.clone(),
                    Boundary {
                        logical_parent: record.str_at("logicalParentUuid").map(str::to_owned),
                        segment,
                    },
                );
                // The boundary's children start a new context.
                self.tails.insert(
                    uuid.clone(),
                    Tail {
                        node: None,
                        version: usize::MAX,
                    },
                );
                self.record_items.insert(uuid, Vec::new());
                Ok(())
            }
            Some("away_summary") => {
                let recap = record.str_at("content").unwrap_or_default();
                if !recap.is_empty() {
                    let node = parent
                        .as_deref()
                        .and_then(|p| self.tails.get(p))
                        .and_then(|tail| tail.node)
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
                self.pass(uuid, parent)
            }
            _ => self.pass(uuid, parent),
        }
    }

    /// A record whose parent is a compaction boundary opens the new context:
    /// its first item `continues` from the old tail.
    fn continue_compaction(
        &mut self,
        parent: Option<&str>,
        items: &[usize],
    ) -> Result<(), Failure> {
        let Some(boundary) = parent.and_then(|p| self.boundaries.get(p)) else {
            return Ok(());
        };
        if let (Some(old), Some(&first)) = (&boundary.logical_parent, items.first()) {
            self.continues.push((first, old.clone()));
        }
        Ok(())
    }

    /// After the anchor of a preserved segment, the segment is re-presented.
    fn after_anchor(&mut self, uuid: &str, mut tail: Tail) -> Result<Tail, Failure> {
        let Some(boundary) = self.anchors.get(uuid).cloned() else {
            return Ok(tail);
        };
        let Some((head, last, preserved)) = self.boundaries[&boundary].segment.clone() else {
            return Ok(tail);
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
            sequence.extend(self.record_items.get(at).cloned().unwrap_or_default());
        }
        tail.node = self.build(tail.node, &sequence);
        Ok(tail)
    }

    fn shape(&mut self, at: usize, record: &Json) -> Result<(), Failure> {
        use v3::fact::{direction::IN, modality::*};
        self.witness_of(at, record)?;
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
        self.prefix = prefix;
        self.version += 1;
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
        let continues = std::mem::take(&mut self.continues);
        for (first, old) in continues {
            if let (Some(node), Some(tail)) = (
                self.items[first].original,
                self.tails.get(&old).and_then(|tail| tail.node),
            ) {
                self.out += annotate_node(
                    node,
                    &NodeAnnotations {
                        continues: vec![tail],
                        ..NodeAnnotations::default()
                    },
                );
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
        // Witnesses: one per node a record holds; leaves carry the session.
        let followed: HashSet<Id> = self
            .nodes
            .values()
            .filter_map(|(previous, _)| *previous)
            .collect();
        let mut held: BTreeMap<usize, Vec<Id>> = BTreeMap::new();
        for item in &self.items {
            if let (Some(record), Some(node)) = (item.record, item.original) {
                let nodes = held.entry(record).or_default();
                if !nodes.contains(&node) {
                    nodes.push(node);
                }
            }
        }
        let mut located: HashSet<String> = HashSet::new();
        let mut locate: Vec<(usize, Id)> = Vec::new();
        for (&record, nodes) in &held {
            for &node in nodes {
                if !followed.contains(&node) {
                    if let Some(session) = self.sessions.get(&record) {
                        located.insert(session.clone());
                        locate.push((record, node));
                    }
                }
            }
        }
        // A session id held only by inner records goes on its last node.
        let mut last_of: BTreeMap<String, (usize, Id)> = BTreeMap::new();
        for (&record, nodes) in &held {
            if let (Some(session), Some(&node)) = (self.sessions.get(&record), nodes.last()) {
                last_of.insert(session.clone(), (record, node));
            }
        }
        for (session, placed) in last_of {
            if !located.contains(&session) {
                locate.push(placed);
            }
        }
        let locate: HashSet<(usize, Id)> = locate.into_iter().collect();
        for (record, nodes) in held {
            let annotations = self.witnesses.get(&record).cloned().unwrap_or_default();
            for node in nodes {
                let built = witness(SOURCE_CLAUDE_CODE, node)
                    .map_err(|_| Failure::new("<witness>", "a witness does not build"))?;
                let id = built.root().expect("a witness is rooted");
                self.out += unexported(built);
                let mut annotations = annotations.clone();
                if locate.contains(&(record, node)) {
                    annotations.source_locator = self.sessions.get(&record).cloned();
                }
                self.out += annotate_witness(id, annotations).map_err(|_| {
                    Failure::new("<witness>", "a witness annotation does not build")
                })?;
            }
        }
        // Glosses.
        let mut glosses = std::mem::take(&mut self.node_glosses);
        for (item, category, text, model) in std::mem::take(&mut self.item_glosses) {
            if let Some(node) = self.items[item].original {
                glosses.push((node, category, text, model));
            }
        }
        for (node, category, text, model) in glosses {
            let built = gloss(node, category, &text)
                .map_err(|_| Failure::new("<gloss>", "a gloss does not build"))?;
            let id = built.root().expect("a gloss is rooted");
            self.out += unexported(built);
            self.out += annotate_gloss(id, model)
                .map_err(|_| Failure::new("<gloss>", "a gloss annotation does not build"))?;
        }
        let _ = records;
        Ok(self.out)
    }
}
