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
    path: &str,
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
        if place != Place::Here {
            return not_built();
        }
        let category = term(v3::gloss::category::SPECS, category).ok_or("a gloss category")?;
        let Some(text) = text()? else { return Ok(()) };
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
            let built = fact(core).map_err(|e| format!("{}: {e}", at.path))?;
            parts.items.push(Item {
                fact: built.root().ok_or("a fact is rooted")?,
                modality,
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
                _ => {}
            }
        }
    }
    Ok(parts)
}

/// One node: `holds` the fact, after `previous`.
fn extend(out: &mut Fragment, previous: Option<Id>, fact_id: Id) -> Result<Id, String> {
    let holds = Fragment::rooted(fact_id, TribleSet::new());
    let built = node(previous, holds).map_err(|e| format!("a node does not build: {e}"))?;
    let id = built.root().ok_or("a node is rooted")?;
    *out += unexported(built);
    Ok(id)
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

    let mut out = Fragment::empty();
    // The unrecorded section (the settings header, then the system section)
    // and the orphan span behind it, built only when a record starts there,
    // as the importer builds them.
    let mut root_tail: Option<Option<Id>> = None;
    let mut orphan_tail: Option<Option<Id>> = None;
    let mut root = |out: &mut Fragment| -> Result<Option<Id>, String> {
        if let Some(tail) = root_tail {
            return Ok(tail);
        }
        let mut tail = None;
        for g in table
            .gaps
            .header
            .iter()
            .chain(table.gaps.prefix)
            .chain(table.gaps.after_prefix)
        {
            let built = gap(g)?;
            let at = built.root().ok_or("a gap is rooted")?;
            *out += unexported(built);
            tail = Some(extend(out, tail, at)?);
        }
        root_tail = Some(tail);
        Ok(tail)
    };
    let mut lines: HashMap<String, Option<Id>> = HashMap::new();
    let mut first: HashMap<String, Id> = HashMap::new();
    // For links: each item node's predecessor and the call ids it holds,
    // and the results waiting for their call.
    let mut previous_of: HashMap<Id, Option<Id>> = HashMap::new();
    let mut calls_at: HashMap<Id, Vec<String>> = HashMap::new();
    let mut answers: Vec<(Id, String)> = Vec::new();
    let mut marks: std::collections::BTreeMap<Id, NodeAnnotations> = Default::default();
    let mut occurrences: Vec<(Id, WitnessAnnotations)> = Vec::new();
    for (_, record) in records {
        let shape = field_at(&unit, record, "type")
            .and_then(|v| text_of(&unit, v))
            .unwrap_or_default();
        if table.windows.prefix.contains(&shape.as_str()) {
            return Err(format!("not built yet: a system section ({{{shape}}})"));
        }
        if let Some(header) = table.windows.header {
            if header.starts_with(&format!("{{{shape}}}.")) {
                let field = &header[shape.len() + 3..];
                if field_at(&unit, record, field).is_some() {
                    return Err(format!("not built yet: a settings header ({header})"));
                }
            }
        }
        if let Some(compaction) = table.windows.compaction {
            if compaction.shape == shape && field_at(&unit, record, compaction.parent).is_some() {
                return Err(format!("not built yet: a compaction ({{{shape}}})"));
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
        let Some(own) = read(record, id) else {
            if !parts.items.is_empty() || claims_something {
                return Err(format!(
                    "not built yet: a {{{shape}}} record without {id} that holds items"
                ));
            }
            continue;
        };
        match first.get(&own) {
            Some(&earlier) if earlier == record => continue,
            Some(_) => {
                return Err(format!(
                    "{{{shape}}}.{id}: a record id repeats with other content"
                ))
            }
            None => {
                first.insert(own.clone(), record);
            }
        }
        let mut tail = match read(record, parent) {
            None => root(&mut out)?,
            Some(p) => match lines.get(&p) {
                Some(&tail) => tail,
                None if all_ids.contains(&p) => {
                    return Err(format!(
                        "{{{shape}}}.{parent}: a record's parent is not built before it"
                    ))
                }
                None => match orphan_tail {
                    Some(tail) => tail,
                    None => {
                        let mut tail = root(&mut out)?;
                        if let Some(g) = &table.gaps.orphan {
                            let built = gap(g)?;
                            let at = built.root().ok_or("a gap is rooted")?;
                            out += unexported(built);
                            tail = Some(extend(&mut out, tail, at)?);
                        }
                        orphan_tail = Some(tail);
                        tail
                    }
                },
            },
        };
        if parts.items.is_empty() && claims_something {
            return Err(format!(
                "{{{shape}}}: a record that holds nothing makes claims"
            ));
        }
        // The nodes the record holds, each once, in record order.
        let mut held: Vec<Id> = Vec::new();
        for item in &parts.items {
            let at = extend(&mut out, tail, item.fact)?;
            previous_of.entry(at).or_insert(tail);
            tail = Some(at);
            calls_at
                .entry(at)
                .or_default()
                .extend(item.calls.iter().cloned());
            if let Some(call) = &item.answers {
                answers.push((at, call.clone()));
            }
            for (category, text) in &item.glosses {
                out += unexported(gloss(at, *category, text).map_err(|e| format!("a gloss: {e}"))?);
            }
            if parts.unrecorded {
                let mark = marks.entry(at).or_default();
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
        lines.insert(own, tail);
    }
    // Each result answers the nearest node on its own line holding the call
    // it names (C1).
    for (node, call) in answers {
        let mut cursor = previous_of.get(&node).copied().flatten();
        let target = loop {
            match cursor {
                None => {
                    return Err(format!(
                        "a result whose call {} is not in its window",
                        "named"
                    ))
                }
                Some(at) if calls_at.get(&at).is_some_and(|c| c.contains(&call)) => break at,
                Some(at) => cursor = previous_of.get(&at).copied().flatten(),
            }
        };
        let mark = marks.entry(node).or_default();
        if !mark.responds_to.contains(&target) {
            mark.responds_to.push(target);
        }
    }
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
