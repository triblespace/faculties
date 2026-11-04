//! A: the gate's rendering of what an importer produced.
//!
//! It reads one unit's `TribleSet` (and its blobs) and nothing else. For
//! every witnessed node it walks the window back to its root along
//! `previous` (each node once: the windows form a graph) and renders what
//! each node reaches as entity tuples (G14):
//!
//! - its item: the tuple of each fact it holds, counted (G15), so a fact
//!   split in two or duplicated under a second entity is another item; a
//!   node that holds nothing is the `empty` item (G11);
//! - its entities: its own tuple (its kind tag and every annotation), each
//!   witness's, each gloss's and each citation's, counted;
//! - the tuple of each occurrence of its witnesses, counted;
//! - its links (`responds_to`, `recalls`, `continues`, `spawned_by`), each
//!   to the node it names, which the gate names by its place.
//!
//! A named entity a tuple refers to (a media type, a sense, a vendor model)
//! renders by its name where it is referred to, and once as its own tuple in
//! [`Rendered::entities`], which the gate compares over the unit.
//!
//! # Every trible renders, by construction
//!
//! A reads an entity only through [`Renderer::take`], which hands out every
//! trible the entity has and records the entity as rendered. Each renderer
//! maps every trible it was handed to exactly one pair of the entity's
//! rendering: there is no branch that reads a trible and renders nothing.
//! A structure trible is consumed by the place it gives: the one decodable
//! `previous` by the window's order, a `holds` by the item, `projects_to`,
//! `of`, `describes` and `cites` by the node the entity is rendered at, a
//! link by the link. Every other value of a structure attribute (a value
//! that is no id, a second target) is a pair of the tuple, so the tuple is
//! no source's. Values A knows are decoded; any other is its raw value under
//! its attribute's id. A kind tag is a pair like any other, so an entity that
//! lost it is another tuple.
//!
//! After rendering, every trible of an entity that was never taken is an
//! `unrendered` row naming its attribute (never its value): nothing in the
//! import can be left out of the comparison.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anybytes::{Bytes, View};
use triblespace::core::import::json_tree;
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::metadata;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::{
    Boolean, GenId, Handle, NsTAIInterval, I256BE, U256BE,
};
use triblespace::prelude::*;

use super::{JsonLeaf, Kind, Multiset, Pointer, Step, Token, Value};
use crate::schemas::archive_v3 as v3;
use crate::schemas::blockdag as main;

/// One entity's tuple, or one fact's: every pair it renders.
pub type Tuple = BTreeSet<Token>;

/// What A rendered: every node on a witnessed window, with its tuples and
/// its place, and every named entity a tuple refers to.
#[derive(Debug, Default)]
pub struct Rendered {
    pub graph: Vec<GraphNode>,
    /// Each named entity a window reached, as its tuple, once.
    pub entities: Multiset<Token>,
    /// Findings: (path, kind, occurrence label).
    pub rows: Vec<(String, Kind, String)>,
}

/// One node of the window graph.
#[derive(Debug)]
pub struct GraphNode {
    /// Its one predecessor (an index into the graph); `None` at a root, and
    /// where the predecessor is ambiguous (reported).
    pub previous: Option<usize>,
    /// Its item: the tuple of each fact it holds, as often as it holds one
    /// (G15). Shape and gap terms are pairs of a fact's tuple.
    pub facts: Multiset<Tuple>,
    /// The entities placed here, each as one token holding its tuple: the
    /// node's own, each witness's, each gloss's and each citation's.
    pub marks: Multiset<Token>,
    /// The tuple of each occurrence of its witnesses, one per occurrence
    /// entity.
    pub occurrences: Multiset<Tuple>,
    /// Its links: the attribute, and the node it names (`None` when no
    /// window holds it).
    pub links: Vec<(String, Option<usize>)>,
    /// Every value of its tuples as the gate compares them over the unit,
    /// in B's flat vocabulary: kind tags and places left out.
    pub tokens: BTreeSet<Token>,
    /// Whether a witness projects to it.
    pub witnessed: bool,
}

/// What A renders of one node, before its links are placed.
#[derive(Clone, Debug, Default)]
struct Rendering {
    facts: Multiset<Tuple>,
    marks: Multiset<Token>,
    occurrences: Multiset<Tuple>,
    links: Vec<(String, Id)>,
    tokens: BTreeSet<Token>,
}

type Raw = Inline<UnknownInline>;

/// Render every window of `facts`.
pub fn render<B: BlobStoreGet>(facts: &TribleSet, blobs: &B) -> Rendered {
    let mut renderer = Renderer {
        facts,
        blobs,
        nodes: BTreeMap::new(),
        rows: Vec::new(),
        taken: HashSet::new(),
        named: BTreeMap::new(),
        names: HashMap::new(),
    };
    let witnessed: BTreeSet<Id> = find!(
        node: Id,
        pattern!(facts, [{ _?witness @ main::source_projection::projects_to: ?node }])
    )
    .collect();
    let mut index: HashMap<Id, usize> = HashMap::new();
    let mut graph: Vec<GraphNode> = Vec::new();
    let mut linked: Vec<Vec<(String, Id)>> = Vec::new();
    for &start in &witnessed {
        // Climb to a node already placed, or to a root.
        let mut climbed: Vec<(Id, Option<Id>)> = Vec::new();
        let mut on_path = BTreeSet::new();
        let mut cursor = Some(start);
        while let Some(node) = cursor {
            if index.contains_key(&node) {
                break;
            }
            on_path.insert(node);
            let previous = renderer.predecessors(node);
            let up = match previous.as_slice() {
                [] => None,
                [one] if on_path.contains(one) => {
                    renderer.rows.push((
                        "window:cycle".to_owned(),
                        Kind::Ambiguous,
                        format!("{node:X}"),
                    ));
                    None
                }
                [one] => Some(*one),
                // A node with several predecessors is reported, never
                // followed: the gate verifies one window at a time, and a
                // node on two windows at once is no window a request was
                // sampled from. A converged node has one predecessor,
                // because its id derives from it.
                _ => {
                    renderer.rows.push((
                        "window:previous".to_owned(),
                        Kind::Ambiguous,
                        format!("{node:X}"),
                    ));
                    None
                }
            };
            climbed.push((node, up));
            cursor = up;
        }
        for (node, up) in climbed.into_iter().rev() {
            let rendering = renderer.node(node);
            index.insert(node, graph.len());
            graph.push(GraphNode {
                previous: up.and_then(|up| index.get(&up).copied()),
                facts: rendering.facts,
                marks: rendering.marks,
                occurrences: rendering.occurrences,
                links: Vec::new(),
                tokens: rendering.tokens,
                witnessed: witnessed.contains(&node),
            });
            linked.push(rendering.links);
        }
    }
    for (at, links) in linked.into_iter().enumerate() {
        graph[at].links = links
            .into_iter()
            .map(|(attribute, target)| (attribute, index.get(&target).copied()))
            .collect();
    }
    renderer.unrendered();
    let mut entities = Multiset::new();
    for token in renderer.named.into_values() {
        *entities.entry(token).or_default() += 1;
    }
    Rendered {
        graph,
        entities,
        rows: renderer.rows,
    }
}

struct Renderer<'a, B> {
    facts: &'a TribleSet,
    blobs: &'a B,
    nodes: BTreeMap<Id, Rendering>,
    rows: Vec<(String, Kind, String)>,
    /// Every entity whose tribles A has rendered.
    taken: HashSet<Id>,
    /// Each named entity a tuple refers to, as its own tuple.
    named: BTreeMap<Id, Token>,
    /// The names of each named entity, as tuples refer to it.
    names: HashMap<Id, Vec<Value>>,
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

/// The kind tags A names, by the term names B uses.
const KINDS: &[(Id, &str)] = &[
    (v3::fact::KIND, "fact"),
    (main::block::KIND, "node"),
    (main::source_projection::KIND, "witness"),
    (v3::occurrence::KIND, "occurrence"),
    (v3::gloss::KIND, "gloss"),
    (v3::citation::KIND, "citation"),
    (v3::media_type::KIND, "media-type"),
    (v3::sense::KIND, "sense"),
    (main::vendor_model::KIND, "vendor-model"),
];

/// The shape of a node that holds no fact: an item of its window that no
/// source item is (G11).
pub fn empty() -> Token {
    Token {
        modality: String::new(),
        direction: String::new(),
        attribute: String::new(),
        value: Value::Term("empty".to_owned()),
    }
}

fn count<T: Ord>(set: &mut Multiset<T>, item: T) {
    *set.entry(item).or_default() += 1;
}

fn annotation(attribute: &str, value: Value) -> Token {
    Token {
        modality: String::new(),
        direction: String::new(),
        attribute: attribute.to_owned(),
        value,
    }
}

/// The place an entity was rendered at, consumed: a second target of the
/// same structure attribute renders as a pair that no source has.
fn elsewhere(attribute: &str) -> Token {
    annotation(attribute, Value::Term("elsewhere".to_owned()))
}

/// The tuple's pairs as the unit-wide comparison sees them: everything but
/// the kind tag.
fn flat(pairs: &[Token]) -> impl Iterator<Item = Token> + '_ {
    pairs.iter().filter(|pair| pair.attribute != "tag").cloned()
}

/// The name of an attribute, for an `unrendered` row: never its value.
fn attribute_name(attribute: Id) -> String {
    let known: &[(Id, &str)] = &[
        (v3::fact::modality.id(), "modality"),
        (v3::fact::direction.id(), "direction"),
        (v3::fact::payload.id(), "payload"),
        (v3::fact::blob.id(), "blob"),
        (v3::fact::structure.id(), "structure"),
        (v3::fact::tool_name.id(), "tool_name"),
        (v3::fact::tool_namespace.id(), "tool_namespace"),
        (v3::fact::status.id(), "status"),
        (v3::fact::exit_code.id(), "exit_code"),
        (v3::fact::media_type.id(), "media_type"),
        (v3::fact::source_uri.id(), "source_uri"),
        (v3::fact::title.id(), "title"),
        (v3::fact::sense.id(), "sense"),
        (v3::fact::detail.id(), "detail"),
        (v3::fact::register.id(), "register"),
        (v3::fact::form.id(), "form"),
        (v3::fact::absent.id(), "absent"),
        (v3::fact::effort.id(), "effort"),
        (main::content_fact::asset_pointer.id(), "asset_pointer"),
        (main::content_fact::resolved_to.id(), "resolved_to"),
        (main::content_fact::asset_size.id(), "asset_size"),
        (main::content_fact::asset_namespace.id(), "asset_namespace"),
        (main::block::holds.id(), "holds"),
        (main::block::previous.id(), "previous"),
        (main::block::responds_to.id(), "responds_to"),
        (main::block::timestamp.id(), "node_timestamp"),
        (v3::node::continues.id(), "continues"),
        (v3::node::spawned_by.id(), "spawned_by"),
        (v3::node::recalls.id(), "recalls"),
        (v3::node::ends.id(), "ends"),
        (v3::node::rendering.id(), "rendering"),
        (v3::node::ends::message_final.id(), "message_final"),
        (main::source_projection::projects_to.id(), "projects_to"),
        (
            main::source_projection::source_namespace.id(),
            "source_namespace",
        ),
        (main::source_projection::generated_by.id(), "generated_by"),
        (
            main::source_projection::source_locator.id(),
            "source_locator",
        ),
        (main::source_projection::source_timestamp.id(), "timestamp"),
        (main::source_projection::author.id(), "author"),
        (main::source_projection::experiencer.id(), "experiencer"),
        (main::source_projection::raw_author.id(), "raw_author"),
        (main::source_projection::raw_model.id(), "raw_model"),
        (main::source_projection::raw_role.id(), "raw_role"),
        (main::source_projection::source_detail.id(), "source_detail"),
        (main::source_projection::resolution.id(), "resolution"),
        (v3::witness::pointer.id(), "pointer"),
        (v3::witness::claimed_size.id(), "claimed_size"),
        (v3::witness::claimed_media_type.id(), "claimed_media_type"),
        (v3::witness::claimed_title.id(), "claimed_title"),
        (v3::witness::turn_ends.id(), "turn_ends"),
        (v3::occurrence::of.id(), "of"),
        (v3::gloss::describes.id(), "describes"),
        (v3::gloss::category.id(), "category"),
        (v3::gloss::text.id(), "text"),
        (v3::citation::cites.id(), "cites"),
        (v3::citation::start.id(), "start"),
        (v3::citation::end.id(), "end"),
        (v3::citation::uri.id(), "uri"),
        (v3::media_type::essence.id(), "essence"),
        (metadata::tag.id(), "tag"),
        (metadata::name.id(), "name"),
        (json_tree::kind.id(), "json_kind"),
        (json_tree::string.id(), "json_string"),
        (json_tree::number_raw.id(), "json_number"),
        (json_tree::boolean.id(), "json_boolean"),
        (json_tree::field_name.id(), "json_field_name"),
        (json_tree::field_value.id(), "json_field_value"),
        (json_tree::object_field.id(), "json_object_field"),
        (json_tree::array_parent.id(), "json_array_parent"),
        (json_tree::array_index.id(), "json_array_index"),
        (json_tree::array_value.id(), "json_array_value"),
    ];
    known.iter().find(|(id, _)| *id == attribute).map_or_else(
        || format!("attr:{attribute:X}"),
        |(_, name)| (*name).to_owned(),
    )
}

impl<'a, B: BlobStoreGet> Renderer<'a, B> {
    fn predecessors(&self, node: Id) -> Vec<Id> {
        find!(
            previous: Id,
            pattern!(self.facts, [{ node @ main::block::previous: ?previous }])
        )
        .collect()
    }

    /// Every trible of `entity`, which A now renders. The only way A reads
    /// an entity's own attributes.
    fn take(&mut self, entity: Id) -> Vec<(Id, Raw)> {
        self.taken.insert(entity);
        find!(
            (attribute: Id, value: Raw),
            pattern!(self.facts, [{ entity @ ?attribute: ?value }])
        )
        .collect()
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

    /// A kind tag's pair, or a source claim a tag carries on an occurrence.
    fn tag(raw: Raw) -> Token {
        match Self::id(raw) {
            Some(id) if id == main::source_projection::COMPACTION_SUMMARY => {
                annotation("compaction_summary", Value::Bool(true))
            }
            Some(id) if id == v3::witness::HARNESS_WRITTEN => {
                annotation("harness_written", Value::Bool(true))
            }
            Some(id) => annotation("tag", term(KINDS, id)),
            None => annotation("tag", Value::Raw(hex(&raw.raw))),
        }
    }

    /// A named entity (a media type, a sense, a vendor model) as a tuple
    /// refers to it: by its names, each a text value; one with no name by
    /// its id, a value no source has. The entity renders once, as its own
    /// tuple, into [`Rendered::entities`].
    fn named(&mut self, entity: Id, at: &str) -> Vec<Value> {
        if let Some(names) = self.names.get(&entity) {
            return names.clone();
        }
        let tribles = self.take(entity);
        let mut names = Vec::new();
        for &(attribute, raw) in &tribles {
            if attribute == metadata::name.id() {
                names.push(self.text(raw, at));
            }
        }
        if names.is_empty() {
            names.push(Value::Raw(format!("{entity:X}")));
        }
        // Before the pairs: an essence may name the entity itself.
        self.names.insert(entity, names.clone());
        let mut pairs = Vec::new();
        for (attribute, raw) in tribles {
            if attribute == metadata::name.id() {
                pairs.push(annotation("name", self.text(raw, at)));
            } else if attribute == metadata::tag.id() {
                pairs.push(Self::tag(raw));
            } else if attribute == v3::media_type::essence.id() {
                match Self::id(raw) {
                    Some(essence) => {
                        for name in self.named(essence, at) {
                            pairs.push(annotation("essence", name));
                        }
                    }
                    None => pairs.push(annotation("essence", Value::Raw(hex(&raw.raw)))),
                }
            } else {
                pairs.push(unknown(attribute, raw));
            }
        }
        self.named.insert(entity, entity_token("entity", pairs));
        names
    }

    fn node(&mut self, node: Id) -> Rendering {
        if let Some(rendering) = self.nodes.get(&node) {
            return rendering.clone();
        }
        let mut rendering = Rendering::default();
        let links = [
            ("responds_to", main::block::responds_to.id()),
            ("continues", v3::node::continues.id()),
            ("spawned_by", v3::node::spawned_by.id()),
            ("recalls", v3::node::recalls.id()),
        ];
        let ends: Vec<(Id, &str)> = v3::node::ends::SPECS
            .iter()
            .map(|(id, name, _)| (*id, *name))
            .collect();
        let mut held = false;
        let mut pairs: Vec<Token> = Vec::new();
        let mut previous: Vec<Id> = Vec::new();
        for (attribute, raw) in self.take(node) {
            if attribute == main::block::holds.id() {
                match Self::id(raw) {
                    Some(fact) => {
                        held = true;
                        let tuple = self.fact(fact);
                        rendering.tokens.extend(
                            tuple
                                .iter()
                                .filter(|token| {
                                    !super::walk::positional(token) && token.attribute != "tag"
                                })
                                .cloned(),
                        );
                        count(&mut rendering.facts, tuple);
                    }
                    None => pairs.push(unknown_as("holds", raw)),
                }
            } else if attribute == main::block::previous.id() {
                match Self::id(raw) {
                    Some(up) => previous.push(up),
                    None => pairs.push(unknown_as("previous", raw)),
                }
            } else if let Some((name, _)) = links.iter().find(|(_, id)| *id == attribute) {
                match Self::id(raw) {
                    Some(target) => rendering.links.push(((*name).to_owned(), target)),
                    None => pairs.push(unknown_as(name, raw)),
                }
            } else if attribute == metadata::tag.id() {
                pairs.push(Self::tag(raw));
            } else if attribute == v3::node::ends.id() {
                let name = Self::id(raw).map_or(Value::Raw(hex(&raw.raw)), |id| term(&ends, id));
                pairs.push(annotation("ends", name));
            } else if attribute == v3::node::rendering.id() {
                let name = Self::id(raw).map_or(Value::Raw(hex(&raw.raw)), |id| {
                    term(v3::node::rendering::SPECS, id)
                });
                pairs.push(annotation("rendering", name));
            } else if attribute == main::block::timestamp.id() {
                pairs.push(annotation("timestamp", time(raw)));
            } else {
                pairs.push(unknown(attribute, raw));
            }
        }
        // One predecessor is the window's order. Several are reported by
        // the climb, and each stays a pair, so the node is no source's.
        if previous.len() > 1 {
            pairs.extend(previous.iter().map(|_| elsewhere("previous")));
        }
        if !held {
            count(&mut rendering.facts, Tuple::from([empty()]));
        }
        rendering.tokens.extend(flat(&pairs));
        count(&mut rendering.marks, entity_token("node", pairs));

        let witnesses: Vec<Id> = find!(
            witness: Id,
            pattern!(self.facts, [{ ?witness @ main::source_projection::projects_to: node }])
        )
        .collect();
        for witness in witnesses {
            let pairs = self.claims(
                witness,
                main::source_projection::projects_to.id(),
                "projects_to",
                node,
            );
            rendering.tokens.extend(flat(&pairs));
            count(&mut rendering.marks, entity_token("witness", pairs));
            let occurrences: Vec<Id> = find!(
                occurrence: Id,
                pattern!(self.facts, [{ ?occurrence @ v3::occurrence::of: witness }])
            )
            .collect();
            for occurrence in occurrences {
                let pairs = self.claims(occurrence, v3::occurrence::of.id(), "of", witness);
                rendering.tokens.extend(flat(&pairs));
                count(&mut rendering.occurrences, pairs.into_iter().collect());
            }
        }

        // Each gloss as one entity. Its kind and text also render as
        // `gloss:<category>`, the model that wrote it under that, and one
        // without a kind or a text as `gloss:incomplete`, which no source
        // names.
        let glossed: Vec<Id> = find!(
            gloss: Id,
            pattern!(self.facts, [{ ?gloss @ v3::gloss::describes: node }])
        )
        .collect();
        for gloss in glossed {
            let mut pairs = Vec::new();
            let (mut kinds, mut texts, mut models) = (Vec::new(), Vec::new(), Vec::new());
            for (attribute, raw) in self.take(gloss) {
                if attribute == v3::gloss::category.id() {
                    let kind = Self::id(raw).map_or_else(
                        || hex(&raw.raw),
                        |id| name_of(v3::gloss::category::SPECS, id),
                    );
                    pairs.push(annotation("category", Value::Term(kind.clone())));
                    kinds.push(kind);
                } else if attribute == v3::gloss::text.id() {
                    let text = self.text(raw, "annotation/gloss");
                    pairs.push(annotation("text", text.clone()));
                    texts.push(text);
                } else if attribute == v3::gloss::generated_by.id() {
                    let values = match Self::id(raw) {
                        Some(model) => self.named(model, "annotation/gloss"),
                        None => vec![Value::Raw(hex(&raw.raw))],
                    };
                    for value in values {
                        pairs.push(annotation("generated_by", value.clone()));
                        models.push(value);
                    }
                } else if attribute == v3::gloss::describes.id() {
                    if Self::id(raw) != Some(node) {
                        let pair = elsewhere("describes");
                        rendering.tokens.insert(pair.clone());
                        pairs.push(pair);
                    }
                } else if attribute == metadata::tag.id() {
                    pairs.push(Self::tag(raw));
                } else {
                    let pair = unknown(attribute, raw);
                    rendering.tokens.insert(pair.clone());
                    pairs.push(pair);
                }
            }
            if kinds.is_empty() || texts.is_empty() {
                rendering.tokens.insert(annotation(
                    "gloss:incomplete",
                    Value::Raw(format!("{gloss:X}")),
                ));
            }
            for kind in &kinds {
                for text in &texts {
                    rendering
                        .tokens
                        .insert(annotation(&format!("gloss:{kind}"), text.clone()));
                }
                for model in &models {
                    rendering.tokens.insert(annotation(
                        &format!("gloss:{kind}:generated_by"),
                        model.clone(),
                    ));
                }
            }
            count(&mut rendering.marks, entity_token("gloss", pairs));
        }

        // Each citation as one entity; one without its span or uri renders
        // as `citation:incomplete`, which no source names.
        let cited: Vec<Id> = find!(
            citation: Id,
            pattern!(self.facts, [{ ?citation @ v3::citation::cites: node }])
        )
        .collect();
        let span = |value: Raw| match value.transmute::<U256BE>().try_from_inline::<u128>() {
            Ok(n) => n.to_string(),
            Err(_) => hex(&value.raw),
        };
        for citation in cited {
            let mut pairs = Vec::new();
            let (mut starts, mut stops, mut uris) = (Vec::new(), Vec::new(), Vec::new());
            for (attribute, raw) in self.take(citation) {
                if attribute == v3::citation::start.id() {
                    pairs.push(annotation("start", Value::Number(span(raw))));
                    starts.push(span(raw));
                } else if attribute == v3::citation::end.id() {
                    pairs.push(annotation("end", Value::Number(span(raw))));
                    stops.push(span(raw));
                } else if attribute == v3::citation::uri.id() {
                    let uri = self.text(raw, "annotation/citation");
                    pairs.push(annotation("uri", uri.clone()));
                    uris.push(match uri {
                        Value::Text(uri) => uri,
                        other => format!("{other:?}"),
                    });
                } else if attribute == v3::citation::cites.id() {
                    if Self::id(raw) != Some(node) {
                        let pair = elsewhere("cites");
                        rendering.tokens.insert(pair.clone());
                        pairs.push(pair);
                    }
                } else if attribute == metadata::tag.id() {
                    pairs.push(Self::tag(raw));
                } else {
                    let pair = unknown(attribute, raw);
                    rendering.tokens.insert(pair.clone());
                    pairs.push(pair);
                }
            }
            if starts.is_empty() || stops.is_empty() || uris.is_empty() {
                rendering.tokens.insert(annotation(
                    "citation:incomplete",
                    Value::Raw(format!("{citation:X}")),
                ));
            }
            for start in &starts {
                for stop in &stops {
                    for uri in &uris {
                        rendering.tokens.insert(annotation(
                            "citation",
                            Value::Text(format!("{start}..{stop} {uri}")),
                        ));
                    }
                }
            }
            count(&mut rendering.marks, entity_token("citation", pairs));
        }
        self.nodes.insert(node, rendering.clone());
        rendering
    }

    /// A fact's tuple: one token per trible, each under the fact's modality
    /// and direction (a `structure` root as its whole document, one pair per
    /// root: G18), with the fact's shape for its modality and direction and its kind tag. A fact
    /// with no single modality and direction renders a shape for each
    /// combination, which no source fact has.
    fn fact(&mut self, fact: Id) -> Tuple {
        let tribles = self.take(fact);
        let mut modalities = Vec::new();
        let mut directions = Vec::new();
        let mut values: Vec<(String, Value)> = Vec::new();
        let mut plain: Vec<Token> = Vec::new();
        // Each `structure` root's document (G18).
        let mut documents: Vec<Vec<(Pointer, JsonLeaf)>> = Vec::new();
        for &(attribute, raw) in &tribles {
            if attribute == v3::fact::modality.id() {
                match Self::id(raw) {
                    Some(id) => modalities.push(name_of(v3::fact::modality::SPECS, id)),
                    None => values.push(("modality".to_owned(), Value::Raw(hex(&raw.raw)))),
                }
            } else if attribute == v3::fact::direction.id() {
                match Self::id(raw) {
                    Some(id) => directions.push(name_of(v3::fact::direction::SPECS, id)),
                    None => values.push(("direction".to_owned(), Value::Raw(hex(&raw.raw)))),
                }
            }
        }
        if modalities.is_empty() {
            modalities.push(String::new());
        }
        if directions.is_empty() {
            directions.push(String::new());
        }
        let at = format!("{}/{}", modalities[0], directions[0]);
        for (attribute, raw) in tribles {
            if attribute == v3::fact::modality.id() || attribute == v3::fact::direction.id() {
                // Rendered as the shape, above.
                continue;
            }
            if attribute == metadata::tag.id() {
                plain.push(Self::tag(raw));
                continue;
            }
            if attribute == v3::fact::absent.id() {
                let value = Self::id(raw).map_or(Value::Raw(hex(&raw.raw)), |id| {
                    term(v3::fact::absent::SPECS, id)
                });
                values.push(("absent".to_owned(), value));
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
            let terms: [(&str, Id, &[(Id, &str)]); 5] = [
                ("status", v3::fact::status.id(), v3::fact::status::SPECS),
                ("detail", v3::fact::detail.id(), v3::fact::detail::SPECS),
                (
                    "register",
                    v3::fact::register.id(),
                    v3::fact::register::SPECS,
                ),
                ("form", v3::fact::form.id(), v3::fact::form::SPECS),
                ("effort", v3::fact::effort.id(), v3::fact::effort::SPECS),
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
                        for value in self.named(entity, &format!("{at}/{name}")) {
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
                match Self::id(raw) {
                    Some(root) => {
                        let mut leaves = Vec::new();
                        self.json(root, &mut Vec::new(), &mut leaves, 0);
                        leaves.sort();
                        documents.push(leaves);
                    }
                    None => values.push(("structure".to_owned(), Value::Raw(hex(&raw.raw)))),
                }
                continue;
            }
            let token = unknown(attribute, raw);
            values.push((token.attribute, token.value));
        }
        // Each root its own pair (G18): a document that several roots render
        // is one pair per root, told apart by its copy.
        documents.sort();
        let mut at = 0;
        while at < documents.len() {
            let same = documents[at..]
                .iter()
                .take_while(|document| **document == documents[at])
                .count();
            for copy in 0..same {
                values.push((
                    "structure".to_owned(),
                    Value::Structure {
                        copy,
                        leaves: documents[at].clone(),
                    },
                ));
            }
            at += same;
        }
        let mut tuple: Tuple = plain.into_iter().collect();
        for modality in &modalities {
            for direction in &directions {
                tuple.insert(super::walk::shape(modality, direction));
                for (attribute, value) in &values {
                    tuple.insert(Token {
                        modality: modality.clone(),
                        direction: direction.clone(),
                        attribute: attribute.clone(),
                        value: value.clone(),
                    });
                }
            }
        }
        tuple
    }

    /// One `json_tree` node as its leaves under `pointer`, each step of
    /// which names its container kind (G18). A node renders
    /// as its JSON value only when its tuple is exactly that value's: one
    /// kind and the attributes of that kind, each once (an object's
    /// entries, each exactly a kind, a name and a value, with no name
    /// twice; an array's entries, each exactly a kind, its parent, an index
    /// and a value, with no index twice). Any other node is a malformed
    /// leaf, which no source has; its children still render, so each of
    /// their tribles renders once.
    fn json(
        &mut self,
        node: Id,
        pointer: &mut Pointer,
        out: &mut Vec<(Pointer, JsonLeaf)>,
        depth: usize,
    ) {
        if depth > 512 {
            self.rows.push((
                "window:structure".to_owned(),
                Kind::Decode,
                format!("{node:X}"),
            ));
            out.push((pointer.clone(), JsonLeaf::Malformed));
            return;
        }
        let mut kinds: Vec<Id> = Vec::new();
        let mut strings: Vec<Raw> = Vec::new();
        let mut numbers: Vec<Raw> = Vec::new();
        let mut bools: Vec<Raw> = Vec::new();
        let mut fields: Vec<Id> = Vec::new();
        let mut malformed = false;
        for (attribute, raw) in self.take(node) {
            if attribute == json_tree::kind.id() {
                match Self::id(raw) {
                    Some(kind) => kinds.push(kind),
                    None => malformed = true,
                }
            } else if attribute == json_tree::string.id() {
                strings.push(raw);
            } else if attribute == json_tree::number_raw.id() {
                numbers.push(raw);
            } else if attribute == json_tree::boolean.id() {
                bools.push(raw);
            } else if attribute == json_tree::object_field.id() {
                match Self::id(raw) {
                    Some(entry) => fields.push(entry),
                    None => malformed = true,
                }
            } else {
                malformed = true;
            }
        }
        let entries: Vec<Id> = find!(
            entry: Id,
            pattern!(self.facts, [{ ?entry @ json_tree::array_parent: node }])
        )
        .collect();
        let kind = match kinds.as_slice() {
            [kind] => Some(*kind),
            _ => None,
        };
        let is = |want: Id| kind == Some(want);
        // The attributes of a kind other than the node's own.
        if !is(json_tree::kind_string) && !strings.is_empty()
            || !is(json_tree::kind_number) && !numbers.is_empty()
            || !is(json_tree::kind_bool) && !bools.is_empty()
            || !is(json_tree::kind_object) && !fields.is_empty()
            || !is(json_tree::kind_array) && !entries.is_empty()
        {
            malformed = true;
        }
        let mut leaves: Vec<(Pointer, JsonLeaf)> = Vec::new();
        match kind {
            Some(kind) if kind == json_tree::kind_object => {
                let mut members: Vec<(String, Id)> = Vec::new();
                for entry in fields {
                    let (mut names, mut values, mut entry_kinds, mut other) =
                        (Vec::new(), Vec::new(), Vec::new(), false);
                    for (attribute, raw) in self.take(entry) {
                        if attribute == json_tree::field_name.id() {
                            names.push(self.text(raw, "structure"));
                        } else if attribute == json_tree::field_value.id() {
                            match Self::id(raw) {
                                Some(value) => values.push(value),
                                None => other = true,
                            }
                        } else if attribute == json_tree::kind.id() {
                            entry_kinds.push(Self::id(raw));
                        } else {
                            other = true;
                        }
                    }
                    match (names.as_slice(), values.as_slice(), entry_kinds.as_slice()) {
                        ([Value::Text(name)], [value], [Some(kind)])
                            if !other && *kind == json_tree::kind_field =>
                        {
                            members.push((name.clone(), *value))
                        }
                        _ => {
                            malformed = true;
                            // Its values still render, each once.
                            for value in values {
                                self.json(value, &mut pointer.clone(), &mut Vec::new(), depth + 1);
                            }
                        }
                    }
                }
                members.sort();
                if members.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                    malformed = true;
                }
                if members.is_empty() {
                    leaves.push((pointer.clone(), JsonLeaf::EmptyObject));
                }
                for (name, value) in members {
                    pointer.push(Step::Key(name));
                    self.json(value, pointer, &mut leaves, depth + 1);
                    pointer.pop();
                }
            }
            Some(kind) if kind == json_tree::kind_array => {
                let mut elements: Vec<(u128, Id)> = Vec::new();
                for entry in entries {
                    let (mut parents, mut indices, mut values, mut entry_kinds, mut other) =
                        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), false);
                    for (attribute, raw) in self.take(entry) {
                        if attribute == json_tree::array_parent.id() {
                            parents.push(Self::id(raw));
                        } else if attribute == json_tree::array_index.id() {
                            indices.push(raw.transmute::<U256BE>().try_from_inline::<u128>().ok());
                        } else if attribute == json_tree::array_value.id() {
                            match Self::id(raw) {
                                Some(value) => values.push(value),
                                None => other = true,
                            }
                        } else if attribute == json_tree::kind.id() {
                            entry_kinds.push(Self::id(raw));
                        } else {
                            other = true;
                        }
                    }
                    match (
                        parents.as_slice(),
                        indices.as_slice(),
                        values.as_slice(),
                        entry_kinds.as_slice(),
                    ) {
                        ([Some(parent)], [Some(index)], [value], [Some(kind)])
                            if !other
                                && *parent == node
                                && *kind == json_tree::kind_array_entry =>
                        {
                            elements.push((*index, *value))
                        }
                        _ => {
                            malformed = true;
                            for value in values {
                                self.json(value, &mut pointer.clone(), &mut Vec::new(), depth + 1);
                            }
                        }
                    }
                }
                elements.sort();
                if elements.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                    malformed = true;
                }
                if elements.is_empty() {
                    leaves.push((pointer.clone(), JsonLeaf::EmptyArray));
                }
                for (index, value) in elements {
                    pointer.push(Step::Index(index));
                    self.json(value, pointer, &mut leaves, depth + 1);
                    pointer.pop();
                }
            }
            Some(kind) if kind == json_tree::kind_string || kind == json_tree::kind_number => {
                let raws = if kind == json_tree::kind_string {
                    &strings
                } else {
                    &numbers
                };
                match raws.as_slice() {
                    [raw] => match self.text(*raw, "structure") {
                        Value::Text(text) => leaves.push((
                            pointer.clone(),
                            if kind == json_tree::kind_string {
                                JsonLeaf::String(text)
                            } else {
                                JsonLeaf::Number(text)
                            },
                        )),
                        _ => malformed = true,
                    },
                    _ => malformed = true,
                }
            }
            Some(kind) if kind == json_tree::kind_bool => match bools.as_slice() {
                [raw] => match raw.transmute::<Boolean>().try_from_inline::<bool>() {
                    Ok(value) => leaves.push((pointer.clone(), JsonLeaf::Bool(value))),
                    Err(_) => malformed = true,
                },
                _ => malformed = true,
            },
            Some(kind) if kind == json_tree::kind_null => {
                leaves.push((pointer.clone(), JsonLeaf::Null));
            }
            _ => malformed = true,
        }
        if malformed {
            out.push((pointer.clone(), JsonLeaf::Malformed));
        } else {
            out.extend(leaves);
        }
    }

    /// Report every trible of an entity A never rendered, by its
    /// attribute's name.
    fn unrendered(&mut self) {
        for trible in self.facts.iter() {
            if self.taken.contains(trible.e()) {
                continue;
            }
            self.rows.push((
                format!("window:unrendered/{}", attribute_name(*trible.a())),
                Kind::Unrendered,
                format!("{:X}/{:X}", trible.e(), trible.a()),
            ));
        }
    }

    /// The tuple of a witness or of one of its occurrences: every trible
    /// as one pair. The one that places it (`projects_to` the node it is
    /// rendered at, `of` the witness) is consumed by that place; another
    /// value of it renders as a pair no source has. Every claim renders:
    /// the witness's `source_namespace`, an occurrence's `ends` (ruling
    /// M1), and an `author` or `experiencer`, which no source table
    /// declares, under its own name.
    fn claims(&mut self, entity: Id, structure: Id, name: &str, place: Id) -> Vec<Token> {
        let ends: Vec<(Id, &str)> = v3::node::ends::SPECS
            .iter()
            .map(|(id, name, _)| (*id, *name))
            .collect();
        let mut tokens = Vec::new();
        for (attribute, raw) in self.take(entity) {
            if attribute == structure {
                match Self::id(raw) {
                    Some(id) if id == place => {}
                    Some(_) => tokens.push(elsewhere(name)),
                    None => tokens.push(unknown_as(name, raw)),
                }
                continue;
            }
            if attribute == metadata::tag.id() {
                tokens.push(Self::tag(raw));
                continue;
            }
            if attribute == main::source_projection::source_namespace.id() {
                let value = Self::id(raw).map_or(Value::Raw(hex(&raw.raw)), |id| {
                    term(v3::witness::NAMESPACES, id)
                });
                tokens.push(annotation("source_namespace", value));
                continue;
            }
            if attribute == main::source_projection::author.id()
                || attribute == main::source_projection::experiencer.id()
            {
                let name = if attribute == main::source_projection::author.id() {
                    "author"
                } else {
                    "experiencer"
                };
                tokens.push(annotation(name, Value::Raw(hex(&raw.raw))));
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
                tokens.push(annotation(name, value));
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
                        for value in self.named(entity, &format!("annotation/{name}")) {
                            tokens.push(annotation(name, value));
                        }
                    }
                    None => tokens.push(annotation(name, Value::Raw(hex(&raw.raw)))),
                }
                continue;
            }
            if attribute == v3::witness::turn_ends.id() || attribute == v3::witness::ends.id() {
                let name = if attribute == v3::witness::ends.id() {
                    "ends"
                } else {
                    "turn_ends"
                };
                let value = match Self::id(raw) {
                    Some(id) => term(&ends, id),
                    None => Value::Raw(hex(&raw.raw)),
                };
                tokens.push(annotation(name, value));
                continue;
            }
            if attribute == main::source_projection::source_timestamp.id() {
                tokens.push(annotation("timestamp", time(raw)));
                continue;
            }
            if attribute == v3::witness::claimed_size.id() {
                let value = match raw.transmute::<U256BE>().try_from_inline::<u128>() {
                    Ok(size) => Value::Number(size.to_string()),
                    Err(_) => Value::Raw(hex(&raw.raw)),
                };
                tokens.push(annotation("claimed_size", value));
                continue;
            }
            if attribute == main::source_projection::resolution.id() {
                let value = self.bytes(raw, "annotation/resolution");
                tokens.push(annotation("resolution", value));
                continue;
            }
            tokens.push(unknown(attribute, raw));
        }
        tokens
    }
}

/// One entity as one token: how it was reached, and its whole tuple,
/// sorted, each pair as often as the entity has it.
fn entity_token(reached_as: &str, mut pairs: Vec<Token>) -> Token {
    pairs.sort();
    annotation(reached_as, Value::Tuple(pairs))
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

/// A value of a structure attribute that is no id: rendered raw, so the
/// tuple is no source's.
fn unknown_as(attribute: &str, raw: Raw) -> Token {
    annotation(attribute, Value::Raw(hex(&raw.raw)))
}
