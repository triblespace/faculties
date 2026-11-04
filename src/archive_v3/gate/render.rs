//! A: the gate's rendering of what an importer produced.
//!
//! It reads one unit's `TribleSet` (and its blobs) and nothing else. For
//! every witnessed leaf, and for the predecessor of every node that holds an
//! `out` fact, it walks the window back to its root along `previous` and
//! renders each node as tokens:
//!
//! - every attribute of every fact the node holds, under the fact's modality
//!   and direction (a `structure` as its `json_tree` leaves);
//! - the node's own annotations (`ends`, a time) and those of its witnesses
//!   and glosses, as annotation tokens.
//!
//! Attributes that are structure (`previous`, `holds`, links between nodes,
//! kind tags, a gap's `absent`, derived actor ids) render nothing. Every
//! other attribute renders, and one A does not know renders under its id,
//! so it can only ever be reported, never pass unseen.

use std::collections::{BTreeMap, BTreeSet};

use anybytes::{Bytes, View};
use triblespace::core::import::json_tree;
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::metadata;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::{GenId, Handle, NsTAIInterval, I256BE, U256BE};
use triblespace::prelude::*;

use super::{JsonLeaf, Kind, Token, Value};
use crate::schemas::archive_v3 as v3;
use crate::schemas::blockdag as main;

/// What A rendered: each window as its nodes' token sets, root first.
#[derive(Debug, Default)]
pub struct Rendered {
    pub windows: Vec<Vec<BTreeSet<Token>>>,
    /// Findings: (path, kind, occurrence label).
    pub rows: Vec<(String, Kind, String)>,
}

type Raw = Inline<UnknownInline>;

/// Render every window of `facts`.
pub fn render<B: BlobStoreGet>(facts: &TribleSet, blobs: &B) -> Rendered {
    let mut renderer = Renderer {
        facts,
        blobs,
        nodes: BTreeMap::new(),
        rows: Vec::new(),
    };
    let witnessed: BTreeSet<Id> = find!(
        node: Id,
        pattern!(facts, [{ _?witness @ main::source_projection::projects_to: ?node }])
    )
    .collect();
    let mut ends = BTreeSet::new();
    for &node in &witnessed {
        let followed = exists!(pattern!(facts, [{ _?next @ main::block::previous: node }]));
        if !followed {
            ends.insert(node);
        }
        let produced = exists!(pattern!(facts, [
                { node @ main::block::holds: _?fact },
            { _?fact @ main::content_fact::direction: main::content_fact::direction::OUT },
        ]));
        if produced {
            ends.extend(renderer.predecessors(node));
        }
    }
    let mut windows = Vec::new();
    for end in ends {
        let window: Vec<BTreeSet<Token>> = renderer
            .window(end)
            .into_iter()
            .map(|node| renderer.node(node))
            .collect();
        windows.push(window);
    }
    Rendered {
        windows,
        rows: renderer.rows,
    }
}

struct Renderer<'a, B> {
    facts: &'a TribleSet,
    blobs: &'a B,
    nodes: BTreeMap<Id, BTreeSet<Token>>,
    rows: Vec<(String, Kind, String)>,
}

fn hex(raw: &[u8]) -> String {
    raw.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn term(specs: &[(Id, &str)], id: Id) -> Value {
    match specs.iter().find(|(term, _)| *term == id) {
        Some((_, name)) => Value::Term((*name).to_owned()),
        None => Value::Term(format!("{id:X}")),
    }
}

fn annotation(attribute: &str, value: Value) -> Token {
    Token {
        modality: String::new(),
        direction: String::new(),
        attribute: attribute.to_owned(),
        value,
    }
}

impl<'a, B: BlobStoreGet> Renderer<'a, B> {
    fn predecessors(&self, node: Id) -> Vec<Id> {
        find!(
            previous: Id,
            pattern!(self.facts, [{ node @ main::block::previous: ?previous }])
        )
        .collect()
    }

    /// Root first. A node with several predecessors ends the walk there and
    /// is reported: a window she was sampled from has one past.
    fn window(&mut self, end: Id) -> Vec<Id> {
        let mut window = vec![end];
        let mut seen = BTreeSet::from([end]);
        let mut cursor = end;
        loop {
            let previous = self.predecessors(cursor);
            match previous.as_slice() {
                [] => break,
                [one] if seen.insert(*one) => {
                    window.push(*one);
                    cursor = *one;
                }
                [_] => {
                    self.rows.push((
                        "window:cycle".to_owned(),
                        Kind::Ambiguous,
                        format!("{cursor:X}"),
                    ));
                    break;
                }
                _ => {
                    self.rows.push((
                        "window:previous".to_owned(),
                        Kind::Ambiguous,
                        format!("{cursor:X}"),
                    ));
                    break;
                }
            }
        }
        window.reverse();
        window
    }

    fn text(&mut self, raw: Raw, at: &str) -> Value {
        let handle: Inline<Handle<UTF8String>> = raw.transmute();
        match self.blobs.get::<View<str>, UTF8String>(handle) {
            Ok(text) => Value::Text(text.as_ref().to_owned()),
            Err(_) => {
                self.rows
                    .push((format!("window:{at}"), Kind::Decode, hex(&raw.raw)));
                Value::Raw(hex(&raw.raw))
            }
        }
    }

    fn bytes(&mut self, raw: Raw, at: &str) -> Value {
        let handle: Inline<Handle<RawBytes>> = raw.transmute();
        match self.blobs.get::<Bytes, RawBytes>(handle) {
            Ok(bytes) => Value::Bytes(blake3::hash(bytes.as_ref()).to_hex().to_string()),
            Err(_) => {
                self.rows
                    .push((format!("window:{at}"), Kind::Decode, hex(&raw.raw)));
                Value::Raw(hex(&raw.raw))
            }
        }
    }

    fn id(raw: Raw) -> Option<Id> {
        raw.transmute::<GenId>().try_from_inline::<Id>().ok()
    }

    /// The `metadata::name`s of an entity (a media type, a sense, a vendor
    /// model), each as a text value.
    fn names(&mut self, entity: Id, at: &str) -> Vec<Value> {
        let handles: Vec<Inline<Handle<UTF8String>>> = find!(
            name: Inline<Handle<UTF8String>>,
            pattern!(self.facts, [{ entity @ metadata::name: ?name }])
        )
        .collect();
        handles
            .into_iter()
            .map(|handle| self.text(handle.transmute(), at))
            .collect()
    }

    fn attributes(&self, entity: Id) -> Vec<(Id, Raw)> {
        find!(
            (attribute: Id, value: Raw),
            pattern!(self.facts, [{ entity @ ?attribute: ?value }])
        )
        .collect()
    }

    fn node(&mut self, node: Id) -> BTreeSet<Token> {
        if let Some(tokens) = self.nodes.get(&node) {
            return tokens.clone();
        }
        let mut tokens = BTreeSet::new();
        for (attribute, raw) in self.attributes(node) {
            if attribute == main::block::holds.id() {
                if let Some(fact) = Self::id(raw) {
                    self.fact(fact, &mut tokens);
                }
            } else if attribute == main::block::previous.id()
                || attribute == main::block::responds_to.id()
                || attribute == v3::node::continues.id()
                || attribute == v3::node::spawned_by.id()
                || attribute == v3::node::recalls.id()
                || (attribute == metadata::tag.id() && Self::id(raw) == Some(main::block::KIND))
            {
            } else if attribute == v3::node::ends.id() {
                let name = Self::id(raw).map_or(Value::Raw(hex(&raw.raw)), |id| {
                    let specs: Vec<(Id, &str)> = v3::node::ends::SPECS
                        .iter()
                        .map(|(id, name, _)| (*id, *name))
                        .collect();
                    term(&specs, id)
                });
                tokens.insert(annotation("ends", name));
            } else if attribute == main::block::timestamp.id() {
                tokens.insert(annotation("timestamp", time(raw)));
            } else {
                tokens.insert(unknown(attribute, raw));
            }
        }
        let witnesses: Vec<Id> = find!(
            witness: Id,
            pattern!(self.facts, [{ ?witness @ main::source_projection::projects_to: node }])
        )
        .collect();
        for witness in witnesses {
            self.witness(witness, &mut tokens);
        }
        let glosses: Vec<(Id, Inline<Handle<UTF8String>>)> = find!(
            (category: Id, text: Inline<Handle<UTF8String>>),
            pattern!(self.facts, [{ _?gloss @
                v3::gloss::describes: node,
                v3::gloss::category: ?category,
                v3::gloss::text: ?text,
            }])
        )
        .collect();
        for (category, text) in glosses {
            let name = match term(v3::gloss::category::SPECS, category) {
                Value::Term(name) => name,
                _ => unreachable!("term renders a term"),
            };
            let value = self.text(text.transmute(), "annotation/gloss");
            tokens.insert(annotation(&format!("gloss:{name}"), value));
        }
        let citations: Vec<(Inline<U256BE>, Inline<U256BE>, Inline<Handle<UTF8String>>)> = find!(
            (start: Inline<U256BE>, end: Inline<U256BE>, uri: Inline<Handle<UTF8String>>),
            pattern!(self.facts, [{ _?citation @
                v3::citation::cites: node,
                v3::citation::start: ?start,
                v3::citation::end: ?end,
                v3::citation::uri: ?uri,
            }])
        )
        .collect();
        for (start, end, uri) in citations {
            let uri = self.text(uri.transmute(), "annotation/citation");
            let span = |value: Inline<U256BE>| match value.try_from_inline::<u128>() {
                Ok(n) => n.to_string(),
                Err(_) => hex(&value.raw),
            };
            tokens.insert(annotation(
                "citation",
                Value::Text(format!(
                    "{}..{} {}",
                    span(start),
                    span(end),
                    match uri {
                        Value::Text(uri) => uri,
                        other => format!("{other:?}"),
                    }
                )),
            ));
        }
        self.nodes.insert(node, tokens.clone());
        tokens
    }

    fn fact(&mut self, fact: Id, tokens: &mut BTreeSet<Token>) {
        let attributes = self.attributes(fact);
        let of = |attribute: Id| -> Vec<Id> {
            attributes
                .iter()
                .filter(|(a, _)| *a == attribute)
                .filter_map(|(_, raw)| Self::id(*raw))
                .collect()
        };
        let modalities: Vec<String> = of(v3::fact::modality.id())
            .into_iter()
            .map(|id| name_of(v3::fact::modality::SPECS, id))
            .collect();
        let directions: Vec<String> = of(v3::fact::direction.id())
            .into_iter()
            .map(|id| name_of(v3::fact::direction::SPECS, id))
            .collect();
        let modalities = if modalities.is_empty() {
            vec![String::new()]
        } else {
            modalities
        };
        let directions = if directions.is_empty() {
            vec![String::new()]
        } else {
            directions
        };
        let shape = |m: &str, d: &str| format!("{}/{}", m, d);

        let mut values: Vec<(String, Value)> = Vec::new();
        for &(attribute, raw) in &attributes {
            let at = shape(&modalities[0], &directions[0]);
            if attribute == v3::fact::modality.id()
                || attribute == v3::fact::direction.id()
                || attribute == v3::fact::absent.id()
                || (attribute == metadata::tag.id() && Self::id(raw) == Some(v3::fact::KIND))
            {
                continue;
            }
            let text_attributes = [
                ("payload", v3::fact::payload.id()),
                ("tool_name", v3::fact::tool_name.id()),
                ("tool_namespace", v3::fact::tool_namespace.id()),
                ("source_uri", v3::fact::source_uri.id()),
                ("title", v3::fact::title.id()),
                ("asset_pointer", main::content_fact::asset_pointer.id()),
            ];
            if let Some((name, _)) = text_attributes.iter().find(|(_, id)| *id == attribute) {
                let value = self.text(raw, &format!("{at}/{name}"));
                values.push(((*name).to_owned(), value));
                continue;
            }
            let bytes_attributes = [
                ("blob", v3::fact::blob.id()),
                ("resolved_to", main::content_fact::resolved_to.id()),
            ];
            if let Some((name, _)) = bytes_attributes.iter().find(|(_, id)| *id == attribute) {
                let value = self.bytes(raw, &format!("{at}/{name}"));
                values.push(((*name).to_owned(), value));
                continue;
            }
            let terms: [(&str, Id, &[(Id, &str)]); 4] = [
                ("status", v3::fact::status.id(), v3::fact::status::SPECS),
                ("detail", v3::fact::detail.id(), v3::fact::detail::SPECS),
                (
                    "register",
                    v3::fact::register.id(),
                    v3::fact::register::SPECS,
                ),
                ("form", v3::fact::form.id(), v3::fact::form::SPECS),
            ];
            if let Some((name, _, specs)) = terms.iter().find(|(_, id, _)| *id == attribute) {
                let value = Self::id(raw).map_or(Value::Raw(hex(&raw.raw)), |id| term(specs, id));
                values.push(((*name).to_owned(), value));
                continue;
            }
            if attribute == v3::fact::media_type.id() || attribute == v3::fact::sense.id() {
                let name = if attribute == v3::fact::sense.id() {
                    "sense"
                } else {
                    "media_type"
                };
                match Self::id(raw) {
                    Some(entity) => {
                        for value in self.names(entity, &format!("{at}/{name}")) {
                            values.push((name.to_owned(), value));
                        }
                    }
                    None => values.push((name.to_owned(), Value::Raw(hex(&raw.raw)))),
                }
                continue;
            }
            if attribute == v3::fact::exit_code.id() {
                let value = match raw.transmute::<I256BE>().try_from_inline::<i128>() {
                    Ok(code) => Value::Number(code.to_string()),
                    Err(_) => Value::Raw(hex(&raw.raw)),
                };
                values.push(("exit_code".to_owned(), value));
                continue;
            }
            if attribute == main::content_fact::asset_size.id() {
                let value = match raw.transmute::<U256BE>().try_from_inline::<u128>() {
                    Ok(size) => Value::Number(size.to_string()),
                    Err(_) => Value::Raw(hex(&raw.raw)),
                };
                values.push(("asset_size".to_owned(), value));
                continue;
            }
            if attribute == main::content_fact::asset_namespace.id() {
                values.push(("asset_namespace".to_owned(), Value::Raw(hex(&raw.raw))));
                continue;
            }
            if attribute == v3::fact::structure.id() {
                if let Some(root) = Self::id(raw) {
                    let mut leaves = Vec::new();
                    self.json(root, &mut String::new(), &mut leaves, 0);
                    values.extend(
                        leaves
                            .into_iter()
                            .map(|leaf| ("structure".to_owned(), leaf)),
                    );
                }
                continue;
            }
            let token = unknown(attribute, raw);
            values.push((token.attribute, token.value));
        }
        for modality in &modalities {
            for direction in &directions {
                for (attribute, value) in &values {
                    tokens.insert(Token {
                        modality: modality.clone(),
                        direction: direction.clone(),
                        attribute: attribute.clone(),
                        value: value.clone(),
                    });
                }
            }
        }
    }

    fn json(&mut self, node: Id, pointer: &mut String, out: &mut Vec<Value>, depth: usize) {
        if depth > 512 {
            self.rows.push((
                "window:structure".to_owned(),
                Kind::Decode,
                format!("{node:X}"),
            ));
            return;
        }
        let kinds: Vec<Id> = find!(
            kind: Id,
            pattern!(self.facts, [{ node @ json_tree::kind: ?kind }])
        )
        .collect();
        for kind in kinds {
            if kind == json_tree::kind_object {
                let mut fields: Vec<(String, Id)> = Vec::new();
                let entries: Vec<(Inline<Handle<UTF8String>>, Id)> = find!(
                    (name: Inline<Handle<UTF8String>>, value: Id),
                    pattern!(self.facts, [
                        { node @ json_tree::object_field: _?entry },
                        { _?entry @ json_tree::field_name: ?name, json_tree::field_value: ?value },
                    ])
                )
                .collect();
                for (name, value) in entries {
                    match self.text(name.transmute(), "structure") {
                        Value::Text(name) => fields.push((name, value)),
                        _ => continue,
                    }
                }
                if fields.is_empty() {
                    out.push(Value::Json(pointer.clone(), JsonLeaf::EmptyObject));
                }
                for (name, value) in fields {
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&name.replace('~', "~0").replace('/', "~1"));
                    self.json(value, pointer, out, depth + 1);
                    pointer.truncate(len);
                }
            } else if kind == json_tree::kind_array {
                let entries: Vec<(Inline<U256BE>, Id)> = find!(
                    (index: Inline<U256BE>, value: Id),
                    pattern!(self.facts, [{ _?entry @
                        json_tree::array_parent: node,
                        json_tree::array_index: ?index,
                        json_tree::array_value: ?value,
                    }])
                )
                .collect();
                if entries.is_empty() {
                    out.push(Value::Json(pointer.clone(), JsonLeaf::EmptyArray));
                }
                for (index, value) in entries {
                    let index = match index.try_from_inline::<u128>() {
                        Ok(index) => index.to_string(),
                        Err(_) => hex(&index.raw),
                    };
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&index);
                    self.json(value, pointer, out, depth + 1);
                    pointer.truncate(len);
                }
            } else if kind == json_tree::kind_string || kind == json_tree::kind_number {
                let attribute = if kind == json_tree::kind_string {
                    json_tree::string.id()
                } else {
                    json_tree::number_raw.id()
                };
                let handles: Vec<Raw> = self
                    .attributes(node)
                    .into_iter()
                    .filter(|(a, _)| *a == attribute)
                    .map(|(_, raw)| raw)
                    .collect();
                for raw in handles {
                    if let Value::Text(text) = self.text(raw, "structure") {
                        let leaf = if kind == json_tree::kind_string {
                            JsonLeaf::String(text)
                        } else {
                            JsonLeaf::Number(text)
                        };
                        out.push(Value::Json(pointer.clone(), leaf));
                    }
                }
            } else if kind == json_tree::kind_bool {
                let values: Vec<bool> = find!(
                    value: bool,
                    pattern!(self.facts, [{ node @ json_tree::boolean: ?value }])
                )
                .collect();
                for value in values {
                    out.push(Value::Json(pointer.clone(), JsonLeaf::Bool(value)));
                }
            } else if kind == json_tree::kind_null {
                out.push(Value::Json(pointer.clone(), JsonLeaf::Null));
            } else {
                out.push(Value::Json(
                    pointer.clone(),
                    JsonLeaf::String(format!("kind:{kind:X}")),
                ));
                self.rows.push((
                    "window:structure".to_owned(),
                    Kind::Decode,
                    format!("{node:X}"),
                ));
            }
        }
    }

    fn witness(&mut self, witness: Id, tokens: &mut BTreeSet<Token>) {
        for (attribute, raw) in self.attributes(witness) {
            let w = &main::source_projection::KIND;
            if attribute == main::source_projection::source_namespace.id()
                || attribute == main::source_projection::projects_to.id()
                || attribute == main::source_projection::semantic_predecessor_support.id()
                || attribute == main::source_projection::author.id()
                || attribute == main::source_projection::experiencer.id()
                || (attribute == metadata::tag.id() && Self::id(raw).as_ref() == Some(w))
            {
                continue;
            }
            if attribute == metadata::tag.id()
                && Self::id(raw) == Some(main::source_projection::COMPACTION_SUMMARY)
            {
                tokens.insert(annotation("compaction_summary", Value::Bool(true)));
                continue;
            }
            let text_attributes = [
                ("raw_model", main::source_projection::raw_model.id()),
                ("raw_role", main::source_projection::raw_role.id()),
                ("raw_author", main::source_projection::raw_author.id()),
                (
                    "source_locator",
                    main::source_projection::source_locator.id(),
                ),
                ("source_detail", main::source_projection::source_detail.id()),
                ("pointer", v3::witness::pointer.id()),
                ("claimed_title", v3::witness::claimed_title.id()),
            ];
            if let Some((name, _)) = text_attributes.iter().find(|(_, id)| *id == attribute) {
                let value = self.text(raw, &format!("annotation/{name}"));
                tokens.insert(annotation(name, value));
                continue;
            }
            if attribute == main::source_projection::generated_by.id()
                || attribute == v3::witness::claimed_media_type.id()
            {
                let name = if attribute == main::source_projection::generated_by.id() {
                    "generated_by"
                } else {
                    "claimed_media_type"
                };
                match Self::id(raw) {
                    Some(entity) => {
                        for value in self.names(entity, &format!("annotation/{name}")) {
                            tokens.insert(annotation(name, value));
                        }
                    }
                    None => {
                        tokens.insert(annotation(name, Value::Raw(hex(&raw.raw))));
                    }
                }
                continue;
            }
            if attribute == main::source_projection::source_timestamp.id() {
                tokens.insert(annotation("timestamp", time(raw)));
                continue;
            }
            if attribute == v3::witness::claimed_size.id() {
                let value = match raw.transmute::<U256BE>().try_from_inline::<u128>() {
                    Ok(size) => Value::Number(size.to_string()),
                    Err(_) => Value::Raw(hex(&raw.raw)),
                };
                tokens.insert(annotation("claimed_size", value));
                continue;
            }
            if attribute == main::source_projection::resolution.id() {
                let value = self.bytes(raw, "annotation/resolution");
                tokens.insert(annotation("resolution", value));
                continue;
            }
            tokens.insert(unknown(attribute, raw));
        }
    }
}

fn name_of(specs: &[(Id, &str)], id: Id) -> String {
    match term(specs, id) {
        Value::Term(name) => name,
        _ => unreachable!("term renders a term"),
    }
}

fn time(raw: Raw) -> Value {
    match raw
        .transmute::<NsTAIInterval>()
        .try_from_inline::<(i128, i128)>()
    {
        Ok((lower, upper)) if lower == upper => Value::Time(lower),
        Ok((lower, upper)) => Value::Interval(lower, upper),
        Err(_) => Value::Raw(hex(&raw.raw)),
    }
}

/// An attribute A does not model: rendered under its id, so it is reported.
fn unknown(attribute: Id, raw: Raw) -> Token {
    annotation(&format!("attr:{attribute:X}"), Value::Raw(hex(&raw.raw)))
}
