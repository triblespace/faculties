//! The relational half of a compiled field table: what the table's
//! `Order`, `Windows`, `Gaps`, `Links`, mirrors, rewrites and supplies say
//! about the facts the constructions built.
//!
//! One production interpreter, over the frozen construction image and the
//! unit, built on the archive's constructors. The gate's walker and the
//! hand-written importers stay frozen as migration evidence while this
//! replaces them; the spec is the hand-stated fixtures in the tests, each
//! one relational decision with its expected items written out (round-7
//! design, wiki d3f1abd2).
//!
//! Built so far: records and their repeats, a record's items in record
//! order, and nodes chained by `Order::Parents` behind the unrecorded
//! section, with a span gap behind a parent the unit does not hold.
//! Records the rest of the table governs (a system section, a settings
//! header, a compaction) are refused as not built yet, never built wrong.

use std::collections::{HashMap, HashSet};

use triblespace::core::import::json_tree;
use triblespace::prelude::inlineencodings::U256BE;
use triblespace::prelude::*;
use triblespace_cxg::{PathStep, Unit};

use super::super::fields::{Class, Decode, FieldTable, Gap, Order, Place};
use super::super::{
    annotate_node, ends_disagree, fact, gloss, node, occurrence, unexported, witness, FactCore,
    Model, NodeAnnotations, WitnessAnnotations,
};
use super::{decode_text, field_at, kind, spelling, term, text_of, Cat, Table};
use crate::schemas::archive_v3 as v3;

type Name = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;

/// Where a fact sits in its record: the (array path, index) of each array
/// it is inside, outermost first. The record outside every array is empty.
type Levels = Vec<(String, [u8; 32])>;

/// A gap fact: something stood here whose content the source does not hold.
fn gap(g: &Gap) -> Result<Fragment, String> {
    let named = |specs, name: &str| (!name.is_empty()).then(|| term(specs, name)).flatten();
    let core = FactCore {
        modality: named(v3::fact::modality::SPECS, g.modality),
        direction: named(v3::fact::direction::SPECS, g.direction),
        absent: term(v3::fact::absent::SPECS, g.kind),
        ..FactCore::default()
    };
    fact(core).map_err(|e| format!("a gap does not build: {e}"))
}

/// The assembly points under `v`, with their levels: every element of an
/// array the walk descends into, at any depth, but nothing an entry
/// consumes whole.
fn points(
    t: &Table,
    unit: &Unit,
    v: Id,
    cat: &Cat,
    levels: &mut Levels,
    out: &mut Vec<(Levels, Id, Cat)>,
) {
    if kind(unit, v) != Some(json_tree::kind_object) {
        return;
    }
    let facts = unit.facts;
    let members: Vec<(Name, Id)> = find!(
        (name: Name, value: Id),
        pattern!(facts, [
            { v @ json_tree::object_field: _?field },
            { _?field @ json_tree::field_name: ?name, json_tree::field_value: ?value },
        ])
    )
    .collect();
    for (name, value) in members {
        let child = t.child(unit, v, cat, &PathStep::Field(name));
        if t.chosen(unit, value, &child).len() == 1 {
            continue;
        }
        match kind(unit, value) {
            Some(k) if k == json_tree::kind_array => {
                let entries: Vec<(Inline<U256BE>, Id)> = find!(
                    (index: Inline<U256BE>, element: Id),
                    pattern!(facts, [{ _?entry @
                        json_tree::array_parent: value,
                        json_tree::array_index: ?index,
                        json_tree::array_value: ?element,
                    }])
                )
                .collect();
                for (index, element) in entries {
                    let at = t.element(unit, value, &child);
                    levels.push((child.path.clone(), index.raw));
                    out.push((levels.clone(), element, at.clone()));
                    points(t, unit, element, &at, levels, out);
                    levels.pop();
                }
            }
            Some(k) if k == json_tree::kind_object => points(t, unit, value, &child, levels, out),
            _ => {}
        }
    }
}

/// One item of a record: its fact, and what the record says about it.
struct Item {
    fact: Id,
    modality: Option<Id>,
    /// It carries a request's settings (`effort`): the window's header.
    effort: bool,
    /// The call ids it is a call of, and the call id a result names.
    calls: Vec<String>,
    answers: Option<String>,
    /// Glosses describing it: (category, text).
    glosses: Vec<(Id, String)>,
}

/// What one record holds and claims.
#[derive(Default)]
struct Parts {
    items: Vec<Item>,
    claims: WitnessAnnotations,
    /// Why its last item's sampling stopped (`Place::Last`).
    ends: Vec<Id>,
    /// Its items' rendering is unrecorded (`Place::Every`).
    unrecorded: bool,
    /// Glosses on the end of the line of the latest record before it in
    /// the file that names itself (`Place::FileTail`): (category, text).
    file_tail: Vec<(Id, String, &'static str)>,
}

fn ends_term(name: &str) -> Option<Id> {
    v3::node::ends::SPECS
        .iter()
        .find(|(_, n, _)| *n == name)
        .map(|(id, _, _)| *id)
}

/// One annotation value of a record, by its attribute and place.
fn annotate(
    parts: &mut Parts,
    point_items: std::ops::Range<usize>,
    path: &'static str,
    attribute: &str,
    decode: Decode,
    place: Place,
    unit: &Unit,
    v: Id,
) -> Result<(), String> {
    let not_built = || {
        Err(format!(
            "not built yet: {attribute} at {path} placed {place:?}"
        ))
    };
    let text = || {
        decode_text(decode, unit, v).ok_or_else(|| format!("{path}: {attribute} does not decode"))
    };
    if let Some(category) = attribute.strip_prefix("gloss:") {
        let category = term(v3::gloss::category::SPECS, category).ok_or("a gloss category")?;
        let Some(text) = text()? else { return Ok(()) };
        if place == Place::FileTail {
            parts.file_tail.push((category, text, path));
            return Ok(());
        }
        if place != Place::Here {
            return not_built();
        }
        // The item at the value's own place whose modality is the place's
        // only one.
        let at = point_items.clone();
        if at.len() != 1 {
            return not_built();
        }
        parts.items[at.start].glosses.push((category, text));
        return Ok(());
    }
    match (attribute, place) {
        ("ends", Place::Last) => {
            let name = match decode {
                Decode::ConstTerm(name) => name.to_owned(),
                Decode::Term(map) => {
                    let spelled = spelling(unit, v).ok_or("an end has no spelling")?;
                    map.iter()
                        .find(|(from, _)| *from == spelled)
                        .ok_or_else(|| format!("{path}: an end the table does not name"))?
                        .1
                        .to_owned()
                }
                _ => return not_built(),
            };
            parts.ends.push(ends_term(&name).ok_or("an ends term")?);
        }
        ("rendering", Place::Every) => parts.unrecorded = true,
        (_, Place::Claim) => {
            let claims = &mut parts.claims;
            match (attribute, decode) {
                ("timestamp", Decode::Timestamp) => {
                    // A time is read as the string it is, not by a text
                    // decode, which takes only text-shaped decodes.
                    if let Some(at) = text_of(unit, v).filter(|t| !t.is_empty()) {
                        claims.timestamp.push(
                            crate::archive_v3::claude_code::time(&at)
                                .ok_or_else(|| format!("{path}: a time does not parse"))?,
                        );
                    }
                }
                ("generated_by", Decode::VendorModel) => {
                    if let Some(label) = text_of(unit, v).filter(|l| !l.is_empty()) {
                        claims
                            .generated_by
                            .push(Model::vendor(&label).map_err(|e| format!("{path}: {e}"))?);
                    }
                }
                ("harness_written" | "compaction_summary", Decode::Bool) => {
                    let set = spelling(unit, v).as_deref() == Some("true");
                    if attribute == "harness_written" {
                        claims.harness_written |= set;
                    } else {
                        claims.compaction_summary |= set;
                    }
                }
                ("raw_role" | "raw_model" | "source_locator" | "claimed_title", _) => {
                    let value = text()?;
                    let field = match attribute {
                        "raw_role" => &mut claims.raw_role,
                        "raw_model" => &mut claims.raw_model,
                        "source_locator" => &mut claims.source_locator,
                        _ => &mut claims.claimed_title,
                    };
                    if value.is_some() {
                        *field = value;
                    }
                }
                ("raw_author", _) => claims.raw_author.extend(text()?),
                ("pointer", _) => claims.pointer.extend(text()?),
                _ => return not_built(),
            }
        }
        _ => return not_built(),
    }
    Ok(())
}

/// A record's items in record order (the facts of the record outside every
/// array first, then each array element's by array path and index; within
/// one assembly point, in the order they were assembled), and its claims.
fn record_parts(t: &Table, unit: &Unit, record: Id, cat: &Cat) -> Result<Parts, String> {
    let mut found = vec![(Levels::new(), record, cat.clone())];
    points(t, unit, record, cat, &mut Levels::new(), &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let result = t.table.windows.requests.map(|requests| requests.result);
    let mut parts = Parts::default();
    for (_, value, at) in found {
        let mut cores = t
            .assemble(unit, value, &at)
            .ok_or_else(|| format!("{}: a field needs a decode not built", at.path))?;
        // A result's head is the item of its head modality it holds
        // directly: it comes first, and counts even when it holds no value
        // (Requests::result).
        if let Some((path, head)) = result {
            let shaped = match t.shape(unit, value, &at.path) {
                Some(shape) => format!("{}{{{shape}}}", at.path),
                None => at.path.clone(),
            };
            if shaped == path {
                let modality = term(v3::fact::modality::SPECS, head).ok_or("a head modality")?;
                let core = match cores.iter().position(|c| c.modality == Some(modality)) {
                    Some(at) => cores.remove(at),
                    None => FactCore {
                        modality: Some(modality),
                        direction: Some(v3::fact::direction::IN),
                        ..FactCore::default()
                    },
                };
                cores.insert(0, core);
            }
        }
        let first = parts.items.len();
        for core in cores {
            let modality = core.modality;
            let effort = core.effort.is_some();
            let built = fact(core).map_err(|e| format!("{}: {e}", at.path))?;
            parts.items.push(Item {
                fact: built.root().ok_or("a fact is rooted")?,
                modality,
                effort,
                calls: Vec::new(),
                answers: None,
                glosses: Vec::new(),
            });
        }
        let mine = first..parts.items.len();
        let mut consumed = Vec::new();
        t.consumed(unit, value, &at, &mut consumed);
        for (entry, v, _) in consumed {
            match entry.class {
                Class::Annotation(list) => {
                    for (attribute, decode) in list {
                        let place = t.table.place(entry.path, attribute);
                        annotate(
                            &mut parts,
                            mine.clone(),
                            entry.path,
                            attribute,
                            *decode,
                            place,
                            unit,
                            v,
                        )?;
                    }
                }
                Class::Correlator => {
                    let Some(id) = spelling(unit, v) else {
                        continue;
                    };
                    for link in t.table.links {
                        let modality = |name| term(v3::fact::modality::SPECS, name);
                        if link.from == entry.path {
                            let wanted = modality(link.from_modality);
                            if let Some(item) = parts.items[mine.clone()]
                                .iter_mut()
                                .find(|i| i.modality == wanted)
                            {
                                item.answers = Some(id.clone());
                            }
                        }
                        if link.to.contains(&entry.path) {
                            let wanted = modality(link.to_modality);
                            if let Some(item) = parts.items[mine.clone()]
                                .iter_mut()
                                .find(|i| i.modality == wanted)
                            {
                                item.calls.push(id.clone());
                            }
                        }
                    }
                }
                Class::Rewrite(_) | Class::Supply(_) => {
                    return Err(
                        format!("not built yet: a {:?} at {}", entry.class, entry.path)
                            .chars()
                            .take(160)
                            .collect(),
                    )
                }
                _ => {}
            }
        }
    }
    Ok(parts)
}

/// The end of a line, and the settings header that opens it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Tail {
    node: Option<Id>,
    header: Id,
}

/// What a node is: after what, holding which fact, whether it belongs to
/// the section a window opens with (a header or a system section), and the
/// node it re-presents, if any.
#[derive(Clone, Copy)]
struct Position {
    previous: Option<Id>,
    fact: Id,
    section: bool,
    original: Id,
}

/// The lines being built: every node with its place, the windows' roots per
/// header, and the node annotations.
struct Lines<'t> {
    table: &'t FieldTable,
    out: Fragment,
    places: HashMap<Id, Position>,
    roots: HashMap<Id, Option<Id>>,
    orphans: HashMap<Id, Option<Id>>,
    marks: std::collections::BTreeMap<Id, NodeAnnotations>,
}

impl Lines<'_> {
    fn fact(&mut self, built: Fragment) -> Result<Id, String> {
        let at = built.root().ok_or("a fact is rooted")?;
        self.out += unexported(built);
        Ok(at)
    }

    /// One node: `holds` the fact, after `previous`.
    fn place(
        &mut self,
        previous: Option<Id>,
        fact_id: Id,
        section: bool,
        original: Option<Id>,
    ) -> Result<Id, String> {
        let holds = Fragment::rooted(fact_id, TribleSet::new());
        let built = node(previous, holds).map_err(|e| format!("a node does not build: {e}"))?;
        let id = built.root().ok_or("a node is rooted")?;
        self.out += unexported(built);
        self.places.entry(id).or_insert(Position {
            previous,
            fact: fact_id,
            section,
            original: original.unwrap_or(id),
        });
        Ok(id)
    }

    /// A new root under `header`: the header, then the system section
    /// recorded so far (the unrecorded one: its gaps).
    fn root(&mut self, header: Id) -> Result<Option<Id>, String> {
        if let Some(&tail) = self.roots.get(&header) {
            return Ok(tail);
        }
        let mut tail = Some(self.place(None, header, true, None)?);
        for g in self
            .table
            .gaps
            .prefix
            .iter()
            .chain(self.table.gaps.after_prefix)
        {
            let at = self.fact(gap(g)?)?;
            tail = Some(self.place(tail, at, true, None)?);
        }
        self.roots.insert(header, tail);
        Ok(tail)
    }

    /// A new root under `header` after which a span of unknown items stands:
    /// what a record outside the unit held.
    fn orphan(&mut self, header: Id) -> Result<Option<Id>, String> {
        if let Some(&tail) = self.orphans.get(&header) {
            return Ok(tail);
        }
        let mut tail = self.root(header)?;
        if let Some(g) = &self.table.gaps.orphan {
            let at = self.fact(gap(g)?)?;
            tail = Some(self.place(tail, at, false, None)?);
        }
        self.orphans.insert(header, tail);
        Ok(tail)
    }

    /// The same line under another header: its items re-presented after
    /// the new one, each recalling the node it copies.
    fn rebuild(&mut self, tail: Tail, header: Id) -> Result<Tail, String> {
        if tail.header == header {
            return Ok(tail);
        }
        let mut sequence = Vec::new();
        let mut cursor = tail.node;
        while let Some(at) = cursor {
            let place = self.places[&at];
            if !place.section {
                sequence.push(place);
            }
            cursor = place.previous;
        }
        sequence.reverse();
        let mut node = self.root(header)?;
        for item in sequence {
            let at = self.place(node, item.fact, false, Some(item.original))?;
            if at != item.original {
                // A copy recalls what it re-presents, and keeps its rendering.
                let rendering = self
                    .marks
                    .get(&item.original)
                    .map(|m| m.rendering.clone())
                    .unwrap_or_default();
                let mark = self.marks.entry(at).or_default();
                if !mark.recalls.contains(&item.original) {
                    mark.recalls.push(item.original);
                }
                if mark.rendering.is_empty() {
                    mark.rendering = rendering;
                }
            }
            node = Some(at);
        }
        Ok(Tail { node, header })
    }
}

/// The nodes, witnesses and occurrences of a unit whose facts `image`
/// holds, or why the unit has none.
pub fn build(
    table: &'static FieldTable,
    unit: Unit,
    root: Id,
    _image: &Fragment,
) -> Result<Fragment, String> {
    let Order::Parents { id, parent } = table.order else {
        return Err("not built yet: a table ordered by file".to_owned());
    };
    let t = Table::new(table);
    let facts = unit.facts;
    let mut records: Vec<(Inline<U256BE>, Id)> = find!(
        (index: Inline<U256BE>, record: Id),
        pattern!(facts, [{ _?entry @
            json_tree::array_parent: root,
            json_tree::array_index: ?index,
            json_tree::array_value: ?record,
        }])
    )
    .collect();
    records.sort_by(|a, b| a.0.raw.cmp(&b.0.raw));
    let record_cat = Cat {
        path: table.record.to_owned(),
        record: None,
        parent: None,
    };
    let read = |record: Id, field: &str| -> Option<String> {
        field_at(&unit, record, field)
            .and_then(|v| spelling(&unit, v))
            .filter(|s| s != "null")
    };
    let all_ids: HashSet<String> = records.iter().filter_map(|(_, r)| read(*r, id)).collect();

    let mut lines = Lines {
        table,
        out: Fragment::empty(),
        places: HashMap::new(),
        roots: HashMap::new(),
        orphans: HashMap::new(),
        marks: Default::default(),
    };
    // The settings header of a window whose request records none.
    let header_gap = match &table.gaps.header {
        Some(g) => lines.fact(gap(g)?)?,
        None => return Err("not built yet: a table with no header gap".to_owned()),
    };
    let requests = table.windows.requests;
    let mut tails: HashMap<String, Tail> = HashMap::new();
    let mut first: HashMap<String, Id> = HashMap::new();
    // For links: the call ids each node holds, and the results waiting for
    // their call.
    let mut calls_at: HashMap<Id, Vec<String>> = HashMap::new();
    let mut answers: Vec<(Id, String)> = Vec::new();
    let mut occurrences: Vec<(Id, WitnessAnnotations)> = Vec::new();
    // The end of the line of the latest record so far that names itself,
    // the file-tail glosses placed, and those before every such record.
    let mut file_tail: Option<Option<Id>> = None;
    let mut placed: HashSet<(Id, String)> = HashSet::new();
    let mut restated: Vec<(Id, String, &'static str)> = Vec::new();
    for (_, record) in records {
        let shape = field_at(&unit, record, "type")
            .and_then(|v| text_of(&unit, v))
            .unwrap_or_default();
        if table.windows.prefix.contains(&shape.as_str()) {
            return Err(format!("not built yet: a system section ({{{shape}}})"));
        }
        if let Some(compaction) = table.windows.compaction {
            if compaction.shape == shape && field_at(&unit, record, compaction.parent).is_some() {
                return Err(format!("not built yet: a compaction ({{{shape}}})"));
            }
        }
        let request = requests.filter(|r| r.shape == shape);
        if let Some(r) = request {
            // A request's own window edits are not built yet.
            for field in [r.dropped.0, r.cleared.0] {
                if field_at(&unit, record, field).is_some_and(|v| !super::is_empty(&unit, v)) {
                    return Err(format!("not built yet: a request's window edits ({field})"));
                }
            }
        }
        let parts = record_parts(&t, &unit, record, &record_cat)?;
        let claims = &parts.claims;
        let claims_something = !claims.timestamp.is_empty()
            || claims.raw_role.is_some()
            || claims.raw_model.is_some()
            || claims.source_locator.is_some()
            || !claims.generated_by.is_empty()
            || !claims.raw_author.is_empty()
            || claims.harness_written
            || claims.compaction_summary;
        for (category, text, path) in &parts.file_tail {
            match file_tail {
                Some(node) => {
                    let Some(node) = node else {
                        return Err("not built yet: a title on an empty line".to_owned());
                    };
                    lines.out += unexported(
                        gloss(node, *category, text).map_err(|e| format!("a gloss: {e}"))?,
                    );
                    placed.insert((*category, text.clone()));
                }
                // Before every record that names itself there is no line:
                // a restatement, which a later title must say again.
                None => restated.push((*category, text.clone(), path)),
            }
        }
        let Some(own) = read(record, id) else {
            if !parts.items.is_empty() || claims_something {
                return Err(format!(
                    "not built yet: a {{{shape}}} record without {id} that holds items"
                ));
            }
            continue;
        };
        match first.get(&own) {
            Some(&earlier) if earlier == record => {
                file_tail = Some(tails[&own].node);
                continue;
            }
            Some(_) => {
                return Err(format!(
                    "{{{shape}}}.{id}: a record id repeats with other content"
                ))
            }
            None => {
                first.insert(own.clone(), record);
            }
        }
        if parts.items.is_empty() && claims_something {
            return Err(format!(
                "{{{shape}}}: a record that holds nothing makes claims"
            ));
        }
        // A request's settings head its window and are none of its items;
        // a request that records none opens on the header gap.
        let mut header = None;
        let mut items = Vec::new();
        for item in &parts.items {
            if item.effort {
                if request.is_none() || header.is_some() {
                    return Err(format!(
                        "{{{shape}}}: settings outside one request's header"
                    ));
                }
                header = Some(item.fact);
            } else {
                items.push(item);
            }
        }
        let mut tail = match read(record, parent) {
            None => Tail {
                node: lines.root(header_gap)?,
                header: header_gap,
            },
            Some(p) => match tails.get(&p) {
                Some(&tail) => tail,
                None if all_ids.contains(&p) => {
                    return Err(format!(
                        "{{{shape}}}.{parent}: a record's parent is not built before it"
                    ))
                }
                None => Tail {
                    node: lines.orphan(header_gap)?,
                    header: header_gap,
                },
            },
        };
        if request.is_some() {
            tail = lines.rebuild(tail, header.unwrap_or(header_gap))?;
        }
        // The nodes the record holds, each once, in record order.
        let mut held: Vec<Id> = Vec::new();
        for item in items {
            let at = lines.place(tail.node, item.fact, false, None)?;
            tail.node = Some(at);
            calls_at
                .entry(at)
                .or_default()
                .extend(item.calls.iter().cloned());
            if let Some(call) = &item.answers {
                answers.push((at, call.clone()));
            }
            for (category, text) in &item.glosses {
                lines.out +=
                    unexported(gloss(at, *category, text).map_err(|e| format!("a gloss: {e}"))?);
            }
            if parts.unrecorded {
                let mark = lines.marks.entry(at).or_default();
                if mark.rendering.is_empty() {
                    mark.rendering.push(v3::node::rendering::UNRECORDED);
                }
            }
            if !held.contains(&at) {
                held.push(at);
            }
        }
        // Why the record's last sampling stopped is a claim of its
        // occurrence on the last item's node (M1).
        let last = held.last().copied();
        for node in held {
            let ends = if Some(node) == last {
                parts.ends.clone()
            } else {
                Vec::new()
            };
            occurrences.push((
                node,
                WitnessAnnotations {
                    ends,
                    ..parts.claims.clone()
                },
            ));
        }
        tails.insert(own, tail);
        file_tail = Some(tail.node);
    }
    for (category, text, path) in restated {
        if !placed.contains(&(category, text)) {
            return Err(format!(
                "{path}: a title before every record that no later title restates"
            ));
        }
    }
    // Each result answers the nearest node on its own line holding the call
    // it names (C1). A copy holds what its original held.
    for (node, call) in answers {
        let mut cursor = lines.places.get(&node).and_then(|p| p.previous);
        let target = loop {
            match cursor {
                None => return Err("a result whose call is not in its window".to_owned()),
                Some(at) => {
                    let original = lines.places[&at].original;
                    if calls_at.get(&original).is_some_and(|c| c.contains(&call)) {
                        break at;
                    }
                    cursor = lines.places[&at].previous;
                }
            }
        };
        let mark = lines.marks.entry(node).or_default();
        if !mark.responds_to.contains(&target) {
            mark.responds_to.push(target);
        }
    }
    let Lines { mut out, marks, .. } = lines;
    for (node, annotations) in &marks {
        out += annotate_node(*node, annotations);
    }
    // Witnesses: one per node a record holds, and one occurrence of it per
    // record. Two records whose own claims are the same converge on one
    // occurrence; if they ended differently, the unit fails (M2, P1).
    if ends_disagree(&occurrences) {
        return Err("two records with the same claims end differently".to_owned());
    }
    let namespace = v3::witness::NAMESPACES
        .iter()
        .find(|(_, name)| *name == table.namespace)
        .map(|(id, _)| *id)
        .ok_or("the table's namespace is no witness namespace")?;
    for (node, claims) in occurrences {
        let built = witness(namespace, node).map_err(|e| format!("a witness: {e}"))?;
        let at = built.root().ok_or("a witness is rooted")?;
        out += unexported(built);
        out += unexported(occurrence(at, claims).map_err(|e| format!("an occurrence: {e}"))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
