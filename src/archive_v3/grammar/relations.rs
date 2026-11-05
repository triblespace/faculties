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

use super::super::fields::{FieldTable, Gap, Order};
use super::super::{fact, node, unexported, FactCore};
use super::{field_at, kind, spelling, term, text_of, Cat, Table};
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

/// A record's items in record order: the facts of the record outside every
/// array first, then each array element's by (array path, index); within
/// one assembly point, in the order they were assembled.
fn items(t: &Table, unit: &Unit, record: Id, cat: &Cat) -> Result<Vec<Id>, String> {
    let mut found = vec![(Levels::new(), record, cat.clone())];
    points(t, unit, record, cat, &mut Levels::new(), &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    let result = t.table.windows.requests.map(|requests| requests.result);
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
        for core in cores {
            let built = fact(core).map_err(|e| format!("{}: {e}", at.path))?;
            out.push(built.root().ok_or("a fact is rooted")?);
        }
    }
    Ok(out)
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
    // The unrecorded section: the settings header, then the system section.
    let mut section = Vec::new();
    for g in table
        .gaps
        .header
        .iter()
        .chain(table.gaps.prefix)
        .chain(table.gaps.after_prefix)
    {
        let built = gap(g)?;
        section.push(built.root().ok_or("a gap is rooted")?);
        out += unexported(built);
    }
    let mut root_tail = None;
    for &g in &section {
        root_tail = Some(extend(&mut out, root_tail, g)?);
    }
    let orphan_tail = match &table.gaps.orphan {
        Some(g) => {
            let built = gap(g)?;
            let at = built.root().ok_or("a gap is rooted")?;
            out += unexported(built);
            Some(extend(&mut out, root_tail, at)?)
        }
        None => root_tail,
    };

    let mut lines: HashMap<String, Option<Id>> = HashMap::new();
    let mut first: HashMap<String, Id> = HashMap::new();
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
        let Some(own) = read(record, id) else {
            if !items(&t, &unit, record, &record_cat)?.is_empty() {
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
            None => root_tail,
            Some(p) => match lines.get(&p) {
                Some(&tail) => tail,
                None if all_ids.contains(&p) => {
                    return Err(format!(
                        "{{{shape}}}.{parent}: a record's parent is not built before it"
                    ))
                }
                None => orphan_tail,
            },
        };
        for item in items(&t, &unit, record, &record_cat)? {
            tail = Some(extend(&mut out, tail, item)?);
        }
        lines.insert(own, tail);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
