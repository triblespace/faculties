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
//! Built: `Order::Parents` (Claude Code) and `Order::File` (Codex), each
//! exact with its importer on the live samples where it builds. Still
//! refused as not built yet, never built wrong: a record's supplied members
//! under parent order, or of an attribute but a register (`Class::Supply`),
//! and a beside-mark whose look-back reaches a node two records hold as
//! different items (R1).
//!
//! Mirrors are not stated here a second time: the unit's records, read back
//! as the JSON they hold, go through the importers' own record check
//! (`record::check`), whose unit mirrors are verified once every record
//! is checked. That is the whole record check, not its mirrors alone, so a
//! unit it refuses the importer refuses too, for the same reason; it stays
//! shared until the rules replace it.
//!
//! Nor is a history budget (X18, X18'): the table's `HistoryBudget` says
//! where a budget and the tool outputs it applies to are, and the Codex
//! importer's own rule (`codex::Budget`, `codex::Unrecorded`,
//! `codex::History`), over the same records read back, says what her window
//! held of each output, or refuses where and as the importer refuses.
//!
//! Nor is a supplied member (X1, X13): the table's `Supply` says which
//! facts of her message a copy's member is the source of, and the Codex
//! importer's own rule (`codex::supplied_phases`, `codex::phase_register`),
//! over the same records read back, says which message each copy names and
//! what register its phase is, or refuses where and as the importer
//! refuses.
//!
//! Nor is a rendering the client marks beside a tool result (R1, R1',
//! R1'', R1'''): the Claude Code importer's own look-back
//! (`claude_code::beside_his_prompt`, `claude_code::in_one_message`) reads
//! the places before it on its parent's line, each as the importer reads
//! its item (`claude_code::typed_prompt`, its record's claims), and builds
//! it only after his prompt written as a plain string, or refuses where
//! and as the importer refuses.

use std::collections::{HashMap, HashSet};

use triblespace::core::import::json_tree;
use triblespace::prelude::inlineencodings::U256BE;
use triblespace::prelude::*;
use triblespace_cxg::{PathStep, Unit};

use super::super::claude_code::{
    beside_his_prompt, beside_marked, in_one_message, typed_prompt, Before,
};
use super::super::codex::{phase_register, supplied_phases, Budget, History, Unrecorded};
use super::super::fields::{
    Beside, Class, Cond, Decode, DropReason, FieldTable, FileWindows, Gap, HistoryBudget, Order,
    Place, Rewrite, Supply,
};
use super::super::record::check;
use super::super::record::json::{self, Json};
use super::super::{
    annotate_node, ends_disagree, fact, gloss, gloss_part, node, occurrence, unexported, witness,
    FactCore, Model, NodeAnnotations, WitnessAnnotations,
};
use super::{
    apply, decode_text, field_at, holds, json_text, kind, spelling, structure, term, text_of, Cat,
    Table,
};
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
    /// The value of the assembly point it came from: an array element, or
    /// the record itself.
    element: Id,
    /// It carries a request's settings (`effort`): the window's header.
    effort: bool,
    /// The call ids it is a call of, and the call id a result names.
    calls: Vec<String>,
    answers: Option<String>,
    /// Glosses describing it: (category, text, its place among the parts
    /// of a gloss written in parts).
    glosses: Vec<(Id, String, Option<u64>)>,
    /// A record holds it (a gap beside a value may stand in no record).
    held: bool,
    /// She produced it (direction `out`).
    out: bool,
    /// It is inside a result and not its head (`Requests::result`).
    part: bool,
    /// The assembly point it came from.
    levels: Levels,
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
    /// What the client sent in later requests in place of members of its
    /// calls (`Class::Rewrite`), with the object that holds it.
    rewrites: Vec<(Rewrite, &'static str, Id)>,
    /// Glosses on the end of the line of the record's parent
    /// (`Place::ParentTail`): (category, text, path).
    parent_tail: Vec<(Id, String, &'static str)>,
    /// Ends of the request a value names, on its last item
    /// (`Place::Request`): (ends term, the request's id, path).
    request_ends: Vec<(Id, String, &'static str)>,
    /// Facts the relations built for the record (gaps beside its values,
    /// outputs a history budget cut, texts a supply registers), staged with
    /// the lines.
    built: Vec<Fragment>,
    /// Glosses at their own place (`Place::Here`), by the levels of the
    /// value: (levels, category, text, path).
    here: Vec<(Levels, Id, String, &'static str)>,
    /// Why the turn ended, known from a record after its items
    /// ([`Order::File`]'s turn places): (attribute, ends term, place, path).
    turn: Vec<(&'static str, Id, Place, &'static str)>,
    /// An entry that makes items consumed one of its values, whether or not
    /// it rendered any.
    bears: bool,
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
    levels: &Levels,
    path: &'static str,
    attribute: &'static str,
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
        if place == Place::ParentTail {
            parts.parent_tail.push((category, text, path));
            return Ok(());
        }
        if place != Place::Here {
            return not_built();
        }
        parts.here.push((levels.clone(), category, text, path));
        return Ok(());
    }
    match (attribute, place) {
        ("ends", Place::Last)
        | ("ends", Place::LastOut)
        | ("turn_ends", Place::TurnTail | Place::TurnTailWithoutOut) => {
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
            let term = ends_term(&name).ok_or("an ends term")?;
            match place {
                Place::Last => parts.ends.push(term),
                _ => parts.turn.push((attribute, term, place, path)),
            }
        }
        ("rendering", Place::Every) => parts.unrecorded = true,
        ("ends", Place::Request) => {
            let name = match decode {
                Decode::ConstTerm(name) => name,
                _ => return not_built(),
            };
            let term = ends_term(name).ok_or("an ends term")?;
            let message = spelling(unit, v).ok_or_else(|| format!("{path}: names no request"))?;
            parts.request_ends.push((term, message, path));
        }
        (_, Place::Claim) => {
            let claims = &mut parts.claims;
            match (attribute, decode) {
                ("timestamp", Decode::Timestamp) => {
                    // A time is read as the string it is, not by a text
                    // decode, which takes only text-shaped decodes.
                    if let Some(at) = text_of(unit, v).filter(|t| !t.is_empty()) {
                        claims.timestamp.push(
                            crate::archive_v3::record::time(&at)
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
/// Under a history budget, the tool outputs the budget names are what her
/// window held of them. Where the unit's supply is built, its copies hold
/// nothing, and the phase the record's message takes from its copy is the
/// register of its facts the supply names.
fn record_parts(
    t: &Table,
    unit: &Unit,
    record: Id,
    cat: &Cat,
    history: Option<(&HistoryBudget, History)>,
    supplied: Option<(&Supply, Option<&str>)>,
) -> Result<Parts, String> {
    let mut found = vec![(Levels::new(), record, cat.clone())];
    points(t, unit, record, cat, &mut Levels::new(), &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let result = t.table.windows.requests.map(|requests| requests.result);
    let mut parts = Parts::default();
    // The list outputs read, by their array.
    let mut lists = HashSet::new();
    // The results' assembly points: every other item inside one is a part.
    let mut results: Vec<Levels> = Vec::new();
    for (levels, value, at) in found {
        let mut cores = t
            .assemble(unit, value, &at)
            .ok_or_else(|| format!("{}: a field needs a decode not built", at.path))?;
        // A result's head is the item of its head modality it holds
        // directly: it comes first, and counts even when it holds no value
        // (Requests::result).
        let mut headed = false;
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
                results.push(levels.clone());
                headed = true;
            }
        }
        let inside = results.iter().any(|point| levels.starts_with(point));
        let mut consumed = Vec::new();
        t.consumed(unit, value, &at, &mut consumed);
        // X1: her text at a path the supply names takes the phase as its
        // register, by the importer's own rule; the facts are the supply's
        // modality and direction, staged with the lines (the image's fact
        // without the register stays, held by no node). A failure names
        // the phase of the message holding the text, as the importer names
        // it.
        if let Some((supply, Some(phase))) = supplied {
            for (entry, _, _) in &consumed {
                if !supply.of.iter().any(|(text, _)| *text == entry.path) {
                    continue;
                }
                let (item, _) = entry
                    .path
                    .split_once("{message}")
                    .ok_or_else(|| format!("{}: a supplied text outside a message", entry.path))?;
                let register = phase_register(Some(phase), &format!("{item}{{message}}.phase"))
                    .map_err(|failure| failure.to_string())?;
                let held = (
                    term(v3::fact::modality::SPECS, supply.modality),
                    term(v3::fact::direction::SPECS, supply.direction),
                );
                for core in cores
                    .iter_mut()
                    .filter(|core| (core.modality, core.direction) == held)
                {
                    core.register = register;
                    parts
                        .built
                        .push(fact(core.clone()).map_err(|e| format!("{}: {e}", entry.path))?);
                }
            }
        }
        // X18, X18': an output as her client's history held it, by the
        // importer's own rule. A text the budget cuts is her window's text,
        // and its full text the record's claim (the image's fact of the full
        // text stays, held by no node); where the importer refuses, the unit
        // is refused as it refuses.
        if let Some((budget, history)) = history {
            for (entry, v, _) in &consumed {
                if !budget.outputs.contains(&entry.path)
                    || kind(unit, *v) != Some(json_tree::kind_string)
                {
                    continue;
                }
                let text = text_of(unit, *v).ok_or("an output's text")?;
                let shown = history
                    .text(&text, entry.path)
                    .map_err(|failure| failure.to_string())?;
                if shown == text {
                    continue;
                }
                let Class::Fact {
                    modality,
                    direction,
                    ..
                } = entry.class
                else {
                    return Err(format!("{}: a cut output that is no fact", entry.path));
                };
                let held = (
                    term(v3::fact::modality::SPECS, modality),
                    term(v3::fact::direction::SPECS, direction),
                );
                let core = cores
                    .iter_mut()
                    .find(|core| (core.modality, core.direction) == held)
                    .ok_or_else(|| format!("{}: a cut output holds no fact", entry.path))?;
                core.payload = Some(shown);
                parts
                    .built
                    .push(fact(core.clone()).map_err(|e| format!("{}: {e}", entry.path))?);
                parts.claims.untruncated = Some(text);
            }
            // A list output's elements are assembly points of their own: the
            // list is read once, at its first.
            let output = at.path.strip_suffix("[]");
            if let (Some(output), Some(list)) = (output, at.parent) {
                if budget.outputs.contains(&output) && lists.insert(list) {
                    let mut text = String::new();
                    json_text(unit, list, &mut text)
                        .ok_or("a list output is no json_tree value")?;
                    let read =
                        json::parse(text.as_bytes()).ok_or("a list output does not read back")?;
                    history
                        .list(read.arr(), output)
                        .and_then(|()| history.list_whole(read.arr(), output))
                        .map_err(|failure| failure.to_string())?;
                }
            }
        }
        let first = parts.items.len();
        for (n, core) in cores.into_iter().enumerate() {
            let modality = core.modality;
            let effort = core.effort.is_some();
            let out = core.direction == Some(v3::fact::direction::OUT);
            let built = fact(core).map_err(|e| format!("{}: {e}", at.path))?;
            parts.items.push(Item {
                fact: built.root().ok_or("a fact is rooted")?,
                modality,
                element: value,
                effort,
                calls: Vec::new(),
                answers: None,
                glosses: Vec::new(),
                held: true,
                out,
                part: inside && !(headed && n == 0),
                levels: levels.clone(),
            });
        }
        // Gaps the rendering rule puts beside a value: an item of their own
        // right before or after the point's items, held as the rule says.
        for (entry, v, cat) in &consumed {
            for rule in t.table.gaps.beside {
                if rule.at == Beside::Same
                    || rule.path != entry.path
                    || !holds(rule.when, unit, t.record_of(cat, *v), cat.parent)
                {
                    continue;
                }
                let built = gap(&rule.gap)?;
                let item = Item {
                    fact: built.root().ok_or("a gap is rooted")?,
                    modality: term(v3::fact::modality::SPECS, rule.gap.modality),
                    element: value,
                    effort: false,
                    calls: Vec::new(),
                    answers: None,
                    glosses: Vec::new(),
                    held: rule.held,
                    out: rule.gap.direction == "out",
                    part: inside,
                    levels: levels.clone(),
                };
                parts.built.push(built);
                match rule.at {
                    Beside::Before => parts.items.insert(first, item),
                    _ => parts.items.push(item),
                }
            }
        }
        let mine = first..parts.items.len();
        for (entry, v, _) in consumed {
            if matches!(entry.class, Class::Fact { .. } | Class::Structure(_)) {
                parts.bears = true;
            }
            match entry.class {
                Class::Annotation(list) => {
                    for (attribute, decode) in list {
                        let place = t.table.place(entry.path, attribute);
                        annotate(
                            &mut parts, &levels, entry.path, attribute, *decode, place, unit, v,
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
                Class::Rewrite(rewrite) => parts.rewrites.push((rewrite, entry.path, v)),
                // X1: a copy holds nothing of its own; its phase is her
                // message's register (above).
                Class::Supply(_) if supplied.is_some() => {}
                Class::Supply(_) => {
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
    // A gloss describes the one item a record holds at the value's own
    // place, else at the nearest place enclosing it; one in an array below
    // that item is one part of it, with its place there (X16).
    for (levels, category, text, path) in std::mem::take(&mut parts.here) {
        let mut found = Vec::new();
        let mut depth = 0;
        for d in (0..=levels.len()).rev() {
            found = (0..parts.items.len())
                .filter(|&i| parts.items[i].held && parts.items[i].levels[..] == levels[..d])
                .collect();
            if !found.is_empty() {
                depth = d;
                break;
            }
        }
        let [item] = found[..] else {
            return Err(format!(
                "not built yet: a gloss at {path} on no single item"
            ));
        };
        let part = (levels.len() > depth).then(|| {
            let raw = levels[depth].1;
            u64::from_be_bytes(raw[24..].try_into().expect("eight bytes"))
        });
        parts.items[item].glosses.push((category, text, part));
    }
    Ok(parts)
}

/// The items a record leaves for later requests: its own, except that a
/// call whose member the client sent changed stands as what it sent
/// (`Class::Rewrite`). A member equal to hers is a copy and changes
/// nothing; one that names no call of the record is refused.
fn wired(
    unit: &Unit,
    record: Id,
    items: &[&Item],
    rewrites: &[(Rewrite, &'static str, Id)],
    staged: &mut HashMap<Id, Fragment>,
) -> Result<Vec<Id>, String> {
    let facts = unit.facts;
    let mut out: Vec<Id> = items.iter().map(|item| item.fact).collect();
    for (rewrite, path, object) in rewrites {
        let calls: Vec<Id> = field_at(unit, record, rewrite.array)
            .map(|array| {
                find!(
                    element: Id,
                    pattern!(facts, [{ _?entry @
                        json_tree::array_parent: array,
                        json_tree::array_value: ?element,
                    }])
                )
                .collect()
            })
            .unwrap_or_default();
        let sent: Vec<(Name, Id)> = find!(
            (name: Name, value: Id),
            pattern!(facts, [
                { *object @ json_tree::object_field: _?field },
                { _?field @ json_tree::field_name: ?name, json_tree::field_value: ?value },
            ])
        )
        .collect();
        for (name, value) in sent {
            let key = unit.text(name).ok_or("a rewritten member's name")?;
            let named: Vec<Id> = calls
                .iter()
                .copied()
                .filter(|call| {
                    field_at(unit, *call, rewrite.key)
                        .and_then(|k| text_of(unit, k))
                        .is_some_and(|k| k == key)
                })
                .collect();
            let [call] = named.as_slice() else {
                return Err(format!(
                    "{path}: a rewritten member names no single call of its record"
                ));
            };
            // The same value has the same id: json_tree derives it from
            // content.
            if field_at(unit, *call, rewrite.member) == Some(value) {
                continue;
            }
            let mut core = FactCore {
                modality: term(v3::fact::modality::SPECS, rewrite.modality),
                direction: term(v3::fact::direction::SPECS, rewrite.direction),
                ..FactCore::default()
            };
            let mut text = String::new();
            json_text(unit, value, &mut text).ok_or_else(|| format!("{path}: the sent value"))?;
            match rewrite.value {
                ("structure", Decode::Json) => {
                    core.structure =
                        Some(structure(&text).ok_or_else(|| format!("{path}: the sent JSON"))?);
                }
                _ => {
                    return Err(format!(
                        "not built yet: a rewrite's value as {}",
                        rewrite.value.0
                    ))
                }
            }
            if let Some(name) = field_at(unit, *call, rewrite.name.0) {
                apply(&mut core, rewrite.name.1, Decode::Text, unit, name)
                    .ok_or_else(|| format!("{path}: the call's name"))?;
            }
            for (attribute, decode) in rewrite.with {
                apply(&mut core, attribute, *decode, unit, value)
                    .ok_or_else(|| format!("{path}: {attribute}"))?;
            }
            let built = fact(core).map_err(|e| format!("{path}: {e}"))?;
            let id = built.root().ok_or("a fact is rooted")?;
            staged.insert(id, unexported(built));
            for (at, item) in items.iter().enumerate() {
                if item.element == *call {
                    out[at] = id;
                }
            }
        }
    }
    Ok(out)
}

/// Whether two records with one id hold the same thing: the same shape,
/// and every top-level member in which they differ is one the table drops
/// as machinery or a provider id. Values compare by id, which json_tree
/// derives from content.
fn same_kept(table: &FieldTable, unit: &Unit, a: Id, b: Id) -> bool {
    let facts = unit.facts;
    let members = |record: Id| -> HashMap<String, Id> {
        find!(
            (name: Name, value: Id),
            pattern!(facts, [
                { record @ json_tree::object_field: _?field },
                { _?field @ json_tree::field_name: ?name, json_tree::field_value: ?value },
            ])
        )
        .filter_map(|(name, value)| Some((unit.text(name)?, value)))
        .collect()
    };
    let shape = |record: Id| field_at(unit, record, "type").and_then(|v| text_of(unit, v));
    let (Some(kind), true) = (shape(a), shape(a) == shape(b)) else {
        return false;
    };
    let (left, right) = (members(a), members(b));
    let names: HashSet<&String> = left.keys().chain(right.keys()).collect();
    names.into_iter().all(|name| {
        if left.get(name) == right.get(name) {
            return true;
        }
        let path = format!("{{{kind}}}.{name}");
        let mut entries = table
            .entries
            .iter()
            .filter(|entry| entry.path == path)
            .peekable();
        entries.peek().is_some()
            && entries.all(|entry| {
                matches!(
                    entry.class,
                    Class::Drop(DropReason::Machinery(_) | DropReason::ProviderId)
                )
            })
    })
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

/// What a record leaves for its children: the line the source marks, and
/// the same history as later requests are sent it (with the client's
/// rewrites), which a new request starts from.
#[derive(Clone, Copy, PartialEq, Eq)]
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

/// The lines being built: every node with its place, the windows' roots per
/// header, and the node annotations. Nodes, and the facts only relations
/// build, are staged: only those on the line of a node a record holds are
/// emitted.
struct Lines<'t> {
    table: &'t FieldTable,
    out: Fragment,
    nodes: HashMap<Id, Fragment>,
    facts: HashMap<Id, Fragment>,
    places: HashMap<Id, Position>,
    roots: HashMap<Id, Option<Id>>,
    orphans: HashMap<Id, Option<Id>>,
    marks: std::collections::BTreeMap<Id, NodeAnnotations>,
}

impl Lines<'_> {
    fn fact(&mut self, built: Fragment) -> Result<Id, String> {
        let at = built.root().ok_or("a fact is rooted")?;
        self.facts.insert(at, unexported(built));
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
        self.nodes.entry(id).or_insert_with(|| unexported(built));
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

    /// A copy of `original` at `node`, when they differ: it recalls what it
    /// re-presents, and keeps its rendering.
    fn copied(&mut self, node: Id, original: Id) {
        if node == original {
            return;
        }
        let rendering = self
            .marks
            .get(&original)
            .map(|m| m.rendering.clone())
            .unwrap_or_default();
        let mark = self.marks.entry(node).or_default();
        if !mark.recalls.contains(&original) {
            mark.recalls.push(original);
        }
        if mark.rendering.is_empty() {
            mark.rendering = rendering;
        }
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
            self.copied(at, item.original);
            node = Some(at);
        }
        Ok(Tail { node, header })
    }
}

/// What both orders leave for the end: the call ids each node holds, the
/// results waiting for their call, and every occurrence.
#[derive(Default)]
struct Wiring {
    calls_at: HashMap<Id, Vec<String>>,
    answers: Vec<(Id, String)>,
    occurrences: Vec<(Id, WitnessAnnotations)>,
}

/// A gloss of `node`, whole or as one part.
fn described(node: Id, category: Id, text: &str, part: Option<u64>) -> Result<Fragment, String> {
    let built = match part {
        Some(part) => gloss_part(node, category, part, text),
        None => gloss(node, category, text),
    };
    Ok(unexported(built.map_err(|e| format!("a gloss: {e}"))?))
}

/// The nodes, witnesses and occurrences of a unit whose facts `image`
/// holds, or why the unit has none.
pub fn build(
    table: &'static FieldTable,
    unit: Unit,
    root: Id,
    _image: &Fragment,
) -> Result<Fragment, String> {
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
    // Mirrors: the importers' own check of each record, over the JSON the
    // unit holds, then of the copies that reach across it. One predicate,
    // so a copy is refused exactly where and as the importer refuses it.
    // The records read back are kept for the history budget (X18).
    let mut mirrors = check::UnitMirrors::new(table);
    let mut reads = Vec::with_capacity(records.len());
    for (_, record) in &records {
        let mut text = String::new();
        json_text(&unit, *record, &mut text).ok_or("a record is no json_tree value")?;
        let read = crate::archive_v3::record::json::parse(text.as_bytes())
            .ok_or("a record does not read back as JSON")?;
        check::record_in(table, &read, &mut mirrors).map_err(|failure| failure.to_string())?;
        reads.push(read);
    }
    mirrors.verify().map_err(|failure| failure.to_string())?;
    let record_cat = Cat {
        path: table.record.to_owned(),
        record: None,
        parent: None,
    };
    let mut lines = Lines {
        table,
        out: Fragment::empty(),
        nodes: HashMap::new(),
        facts: HashMap::new(),
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
    let mut wiring = Wiring::default();
    let records: Vec<Id> = records.into_iter().map(|(_, record)| record).collect();
    match table.order {
        Order::Parents { id, parent } => parents(
            &t,
            &unit,
            &records,
            &reads,
            &record_cat,
            &mut lines,
            header_gap,
            (id, parent),
            &mut wiring,
        )?,
        Order::File(rules) => file(
            &t,
            &unit,
            &records,
            &reads,
            &record_cat,
            &mut lines,
            header_gap,
            &rules,
            &mut wiring,
        )?,
    }
    finish(table, lines, wiring)
}

/// [`Order::Parents`]: records chained by their parent ids. `reads` are
/// the records as the JSON they hold.
#[allow(clippy::too_many_arguments)]
fn parents(
    t: &Table,
    unit: &Unit,
    records: &[Id],
    reads: &[Json],
    record_cat: &Cat,
    lines: &mut Lines,
    header_gap: Id,
    (id, parent): (&'static str, &'static str),
    wiring: &mut Wiring,
) -> Result<(), String> {
    use v3::fact::modality::{TEXT, TOOL_CALL, TOOL_RESULT};
    let table = t.table;
    let read = |record: Id, field: &str| -> Option<String> {
        field_at(&unit, record, field)
            .and_then(|v| spelling(&unit, v))
            .filter(|s| s != "null")
    };
    let all_ids: HashSet<String> = records.iter().filter_map(|r| read(*r, id)).collect();
    let shape_of = |record: Id| -> String {
        field_at(&unit, record, "type")
            .and_then(|v| text_of(&unit, v))
            .unwrap_or_default()
    };

    // Repeats, in file order: the first record with an id is the record,
    // and a repeat must hold the same thing.
    let mut first: HashMap<String, Id> = HashMap::new();
    let mut repeat = vec![false; records.len()];
    for (at, record) in records.iter().enumerate() {
        let Some(own) = read(*record, id) else {
            continue;
        };
        match first.get(&own) {
            Some(&earlier) if same_kept(table, &unit, earlier, *record) => repeat[at] = true,
            Some(_) => {
                let shape = shape_of(*record);
                return Err(format!(
                    "{{{shape}}}.{id}: a record id repeats with other content"
                ));
            }
            None => {
                first.insert(own, *record);
            }
        }
    }
    // Parents before children; otherwise the file's order. A child whose
    // parent the unit holds but has not built yet waits for it.
    let mut order = Vec::with_capacity(records.len());
    let mut done: HashSet<String> = HashSet::new();
    let mut waiting: HashMap<String, Vec<usize>> = HashMap::new();
    for (at, record) in records.iter().enumerate() {
        if repeat[at] {
            continue;
        }
        if let Some(p) = read(*record, parent) {
            if all_ids.contains(&p) && !done.contains(&p) {
                waiting.entry(p).or_default().push(at);
                continue;
            }
        }
        let mut stack = vec![at];
        while let Some(next) = stack.pop() {
            order.push(next);
            if let Some(own) = read(records[next], id) {
                done.insert(own.clone());
                if let Some(children) = waiting.remove(&own) {
                    stack.extend(children.into_iter().rev());
                }
            }
        }
    }
    if !waiting.is_empty() {
        return Err(format!(
            "{{user}}.{parent}: records whose parent links form a cycle"
        ));
    }

    let requests = table.windows.requests;
    let mut tails: HashMap<String, Line> = HashMap::new();
    // Each request's records so far, and the header it was sampled under;
    // and the occurrence of its last item.
    let mut messages: HashMap<String, (Id, HashSet<String>)> = HashMap::new();
    let mut message_last: HashMap<String, usize> = HashMap::new();
    let mut request_ends: Vec<(Id, String, &'static str)> = Vec::new();
    // Each compaction boundary and the context it replaces, and the last
    // node of each record that opens a new context under one.
    let mut boundaries: HashMap<String, String> = HashMap::new();
    let mut continuing: Vec<(Id, String)> = Vec::new();
    // Preserved segments by their anchor: (head, tail); and each record's
    // own items (fact, node) and parent, which a segment re-presents.
    let mut segments: HashMap<String, (String, String)> = HashMap::new();
    let mut record_items: HashMap<String, Vec<(Id, Id)>> = HashMap::new();
    let mut record_parent: HashMap<String, Option<String>> = HashMap::new();
    // R1: each node a record holds, as the importer's look-back before a
    // beside-mark reads its item (none where two records hold it as
    // different items); and each attachment's parent, and whether it is
    // beside-marked (R1'').
    let mut as_before: HashMap<Id, Option<Before>> = HashMap::new();
    let mut attachment_parents: HashMap<String, (Option<String>, bool)> = HashMap::new();
    let Wiring {
        calls_at,
        answers,
        occurrences,
    } = wiring;
    // Glosses on the file's tail, by the file position of their record.
    let mut file_tail: Vec<(usize, Id, String, &'static str)> = Vec::new();
    for at in order {
        let record = records[at];
        let shape = shape_of(record);
        if table.windows.prefix.contains(&shape.as_str()) {
            return Err(format!("not built yet: a system section ({{{shape}}})"));
        }
        // A compaction boundary holds nothing: its children start a new
        // root, on a span when the context it replaces is outside the unit.
        if let Some(compaction) = table.windows.compaction {
            if compaction.shape == shape && field_at(&unit, record, compaction.parent).is_some() {
                let Some(own) = read(record, id) else {
                    return Err(format!("{{{shape}}}.{id}: a compaction boundary has no id"));
                };
                // A preserved segment, re-presented after its anchor.
                if let Some(head) = read(record, compaction.head) {
                    let (Some(last), Some(anchor)) = (
                        read(record, compaction.tail),
                        read(record, compaction.anchor),
                    ) else {
                        return Err(format!(
                            "{}: a preserved segment is incomplete",
                            compaction.head
                        ));
                    };
                    segments.insert(anchor, (head, last));
                }
                let replaces = read(record, compaction.parent).unwrap_or_default();
                let node = if all_ids.contains(&replaces) {
                    lines.root(header_gap)?
                } else {
                    lines.orphan(header_gap)?
                };
                tails.insert(
                    own.clone(),
                    Line::same(Tail {
                        node,
                        header: header_gap,
                    }),
                );
                boundaries.insert(own, replaces);
                continue;
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
        let parts = record_parts(&t, &unit, record, &record_cat, None, None)?;
        if !parts.turn.is_empty() {
            return Err("not built yet: a turn place under parent order".to_owned());
        }
        request_ends.extend(parts.request_ends.iter().cloned());
        for built in &parts.built {
            lines.fact(built.clone())?;
        }
        for (category, text, path) in &parts.file_tail {
            file_tail.push((at, *category, text.clone(), path));
        }
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
        let from = match read(record, parent) {
            None => Line::same(Tail {
                node: lines.root(header_gap)?,
                header: header_gap,
            }),
            Some(p) => match tails.get(&p) {
                Some(&line) => line,
                None => Line::same(Tail {
                    node: lines.orphan(header_gap)?,
                    header: header_gap,
                }),
            },
        };
        // R1'': an attachment's parent and its mark, as the importer keeps
        // them (a thinking drop is no attachment to it).
        let read_back = &reads[at];
        let attachment =
            shape == "attachment" && read_back.str_at("attachment.type") != Some("thinking_drop");
        if attachment {
            let beside = matches!(
                read_back.get("renderedBesideToolResult"),
                Some(Json::Bool(true))
            );
            attachment_parents.insert(own.clone(), (read(record, parent), beside));
        }
        // R1, R1', R1''': a rendering the client marks beside a tool result,
        // recorded with its rendering (S3), is built only where the
        // importer's own look-back builds it, over the places before it on
        // its parent's line; where it refuses, the unit is refused as it
        // refuses. A window's opening holds no item the look-back reads; a
        // node outside it that no record holds is the span of unknown items.
        if attachment
            && !parts.unrecorded
            && beside_marked(read_back).map_err(|failure| failure.to_string())?
        {
            let mut twice = false;
            let places = &lines.places;
            let line =
                std::iter::successors(from.edited.node, |at| places[at].previous).map_while(|at| {
                    let place = places[&at];
                    match as_before
                        .get(&at)
                        .or_else(|| as_before.get(&place.original))
                    {
                        Some(&Some(item)) => Some(item),
                        Some(None) => {
                            twice = true;
                            None
                        }
                        None => Some(Before {
                            span: !place.section,
                            ..Before::default()
                        }),
                    }
                });
            let decided = beside_his_prompt(line);
            if twice {
                return Err(
                    "not built yet: a beside-mark after a node two records hold as different items"
                        .to_owned(),
                );
            }
            decided.map_err(|failure| failure.to_string())?;
        }
        // A gloss on the parent's tail describes the end of the parent's own
        // line: never a root or an orphan span standing in for one.
        for (category, text, path) in &parts.parent_tail {
            let node = read(record, parent)
                .and_then(|p| tails.get(&p))
                .and_then(|line| line.edited.node)
                .ok_or_else(|| format!("{path}: a gloss with no node to describe"))?;
            lines.out +=
                unexported(gloss(node, *category, text).map_err(|e| format!("a gloss: {e}"))?);
        }
        let (mut edited, mut unedited) = (from.edited, from.unedited);
        if let Some(r) = request {
            // One request spans the records that share its id. Its window is
            // decided once, from the unedited history its first record
            // continues; a record continuing it continues its parent's line.
            let header = header.unwrap_or(header_gap);
            let message = read(record, r.id).unwrap_or_default();
            // R1'': a beside-mark in the middle of one request, by the
            // importer's own rule.
            in_one_message(
                messages.contains_key(&message),
                read(record, parent).as_deref(),
                &attachment_parents,
            )
            .map_err(|failure| failure.to_string())?;
            let continuing = read(record, parent).is_some_and(|p| {
                messages
                    .get(&message)
                    .is_some_and(|(_, records)| records.contains(&p))
            });
            let from = if continuing {
                from
            } else {
                Line::same(lines.rebuild(from.unedited, header)?)
            };
            let (sampled, records) = messages
                .entry(message)
                .or_insert_with(|| (header, HashSet::new()));
            if *sampled != header {
                return Err(format!(
                    "{{{shape}}}: the records of one request name different settings"
                ));
            }
            records.insert(own.clone());
            edited = lines.rebuild(from.edited, header)?;
            unedited = lines.rebuild(from.unedited, header)?;
        }
        // What later requests hold: the same items, with the client's
        // rewrites standing in for the calls it changed. Whether the two
        // lines stay one is decided before either grows.
        let wired = wired(&unit, record, &items, &parts.rewrites, &mut lines.facts)?;
        let own_facts: Vec<Id> = items.iter().map(|item| item.fact).collect();
        let same = edited == unedited && wired == own_facts;
        // The nodes the record holds, each once, in record order.
        let mut held: Vec<Id> = Vec::new();
        let mut own_nodes: Vec<Id> = Vec::new();
        for item in &items {
            let at = lines.place(edited.node, item.fact, false, None)?;
            edited.node = Some(at);
            own_nodes.push(at);
            // R1: the item as the importer's look-back reads it: his typed
            // prompt by the importer's own rule, the claims its record's.
            let head = item.modality == Some(TOOL_RESULT);
            let (prompt, plain_prompt) = if shape == "user" {
                typed_prompt(read_back, item.modality == Some(TEXT), head, item.part)
            } else {
                (false, false)
            };
            let mark = Before {
                attachment,
                head,
                part: item.part,
                call: item.modality == Some(TOOL_CALL),
                span: false,
                prompt,
                plain_prompt,
                harness_written: parts.claims.harness_written,
                compaction_summary: parts.claims.compaction_summary,
            };
            read_before(&mut as_before, at, mark);
            calls_at
                .entry(at)
                .or_default()
                .extend(item.calls.iter().cloned());
            if let Some(call) = &item.answers {
                answers.push((at, call.clone()));
            }
            for (category, text, part) in &item.glosses {
                lines.out += described(at, *category, text, *part)?;
            }
            if parts.unrecorded {
                let mark = lines.marks.entry(at).or_default();
                if mark.rendering.is_empty() {
                    mark.rendering.push(v3::node::rendering::UNRECORDED);
                }
            }
            if item.held && !held.contains(&at) {
                held.push(at);
            }
        }
        // A record whose parent is a compaction boundary opens the new
        // context (resolved once every line is built).
        if let (Some(p), Some(&end)) = (read(record, parent), own_nodes.last()) {
            if boundaries.contains_key(&p) {
                continuing.push((end, p));
            }
        }
        if same {
            unedited = edited;
        } else {
            for (at, fact_id) in wired.iter().enumerate() {
                let original = (*fact_id == own_facts[at]).then_some(own_nodes[at]);
                let node = lines.place(unedited.node, *fact_id, false, original)?;
                match original {
                    Some(original) => lines.copied(node, original),
                    // R1: a call the client rewrote is, to the importer, a
                    // call of no record.
                    None => read_before(
                        &mut as_before,
                        node,
                        Before {
                            call: true,
                            ..Before::default()
                        },
                    ),
                }
                unedited.node = Some(node);
            }
        }
        // Why the record's last sampling stopped is a claim of its
        // occurrence on the last item's node (M1).
        let last = held.last().copied();
        if let (Some(_), Some(r)) = (last, request) {
            // The occurrence the loop below pushes for the last node.
            let at = occurrences.len() + held.len() - 1;
            message_last.insert(read(record, r.id).unwrap_or_default(), at);
        }
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
        record_items.insert(
            own.clone(),
            own_facts
                .iter()
                .copied()
                .zip(own_nodes.iter().copied())
                .collect(),
        );
        record_parent.insert(own.clone(), read(record, parent));
        // After the anchor of a preserved segment, the segment is
        // re-presented: each of its records' items, head first, as copies.
        if let Some((head, last)) = segments.get(&own).cloned() {
            let mut chain = Vec::new();
            let mut cursor = Some(last);
            while let Some(at) = cursor {
                chain.push(at.clone());
                if at == head {
                    break;
                }
                cursor = record_parent.get(&at).cloned().flatten();
                if cursor.is_none() {
                    return Err("the preserved segment does not reach its head".to_owned());
                }
            }
            chain.reverse();
            let mut sequence = Vec::new();
            for at in &chain {
                let items = record_items
                    .get(at)
                    .ok_or("a preserved record is not built before its anchor")?;
                sequence.extend(items.iter().copied());
            }
            let same = edited == unedited;
            for (fact_id, original) in &sequence {
                let node = lines.place(edited.node, *fact_id, false, Some(*original))?;
                lines.copied(node, *original);
                edited.node = Some(node);
            }
            if same {
                unedited = edited;
            } else {
                for (fact_id, original) in &sequence {
                    let node = lines.place(unedited.node, *fact_id, false, Some(*original))?;
                    lines.copied(node, *original);
                    unedited.node = Some(node);
                }
            }
        }
        tails.insert(own, Line { edited, unedited });
    }
    // A gloss on the file's tail describes the end of the line of the latest
    // record before it in the file that names itself. Before every such
    // record there is no line: a restatement, which places nothing when a
    // later gloss of its category, placed after a line, says the same.
    let named: Vec<(usize, String)> = records
        .iter()
        .enumerate()
        .filter_map(|(at, record)| read(*record, id).map(|own| (at, own)))
        .collect();
    let mut placed: HashSet<(Id, String)> = HashSet::new();
    let mut restated: Vec<(Id, String, &'static str)> = Vec::new();
    file_tail.sort_by_key(|(at, ..)| *at);
    for (at, category, text, path) in file_tail {
        let before = named.iter().rev().find(|(position, _)| *position < at);
        match before {
            Some((_, own)) => {
                let Some(node) = tails.get(own).and_then(|line| line.edited.node) else {
                    return Err(format!(
                        "not built yet: {path} on a line that holds nothing"
                    ));
                };
                lines.out +=
                    unexported(gloss(node, category, &text).map_err(|e| format!("a gloss: {e}"))?);
                placed.insert((category, text));
            }
            None => restated.push((category, text, path)),
        }
    }
    for (category, text, path) in restated {
        if !placed.contains(&(category, text)) {
            return Err(format!(
                "{path}: a title before every record that no later title restates"
            ));
        }
    }
    // Every compaction root whose old context is in the unit continues from
    // its tail at the first node of the new line that is not on the old
    // line, so no continues path returns to where it starts.
    let mut continued: HashSet<String> = HashSet::new();
    for (end, boundary) in continuing {
        let old = &boundaries[&boundary];
        if !all_ids.contains(old) {
            continue;
        }
        let Some(tail) = tails.get(old).and_then(|line| line.edited.node) else {
            return Err(format!(
                "{}: a compaction root that continues no node",
                "logicalParentUuid"
            ));
        };
        let mut on_old: HashSet<Id> = HashSet::new();
        let mut cursor = Some(tail);
        while let Some(at) = cursor {
            on_old.insert(at);
            cursor = lines.places[&at].previous;
        }
        let mut new_line = Vec::new();
        let mut cursor = Some(end);
        while let Some(at) = cursor {
            new_line.push(at);
            cursor = lines.places[&at].previous;
        }
        let first = new_line
            .into_iter()
            .rev()
            .find(|node| !on_old.contains(node))
            .ok_or("a compaction whose new context lies on the old line")?;
        let mark = lines.marks.entry(first).or_default();
        if !mark.continues.contains(&tail) {
            mark.continues.push(tail);
        }
        continued.insert(boundary);
    }
    for (boundary, old) in &boundaries {
        if all_ids.contains(old) && !continued.contains(boundary) {
            return Err("a compaction whose new context holds nothing that continues".to_owned());
        }
    }
    // An end placed on a request: the occurrence of its last item.
    for (term, message, path) in request_ends {
        let at = *message_last
            .get(&message)
            .ok_or_else(|| format!("{path}: the request it names is not in the unit"))?;
        let ends = &mut occurrences[at].1.ends;
        if !ends.contains(&term) {
            ends.push(term);
        }
    }
    Ok(())
}

/// What the R1 look-back reads at `node`: none where two records hold it
/// as different items.
fn read_before(as_before: &mut HashMap<Id, Option<Before>>, node: Id, item: Before) {
    let known = as_before.entry(node).or_insert(Some(item));
    if *known != Some(item) {
        *known = None;
    }
}

/// A record's kind under [`Order::File`]: the first rule whose conditions
/// all hold.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Header,
    Prefix,
    Replace,
    Turn,
    Rollback,
    Content,
}

fn file_kind(rules: &FileWindows, unit: &Unit, record: Id) -> FileKind {
    let is = |when: &[Cond]| !when.is_empty() && holds(when, unit, Some(record), None);
    if is(rules.header) {
        FileKind::Header
    } else if is(rules.prefix) {
        FileKind::Prefix
    } else if is(rules.replace) {
        FileKind::Replace
    } else if is(rules.turn) {
        FileKind::Turn
    } else if is(rules.rollback) {
        FileKind::Rollback
    } else {
        FileKind::Content
    }
}

fn claims_something(claims: &WitnessAnnotations) -> bool {
    !claims.timestamp.is_empty()
        || claims.raw_role.is_some()
        || claims.raw_model.is_some()
        || claims.source_locator.is_some()
        || !claims.generated_by.is_empty()
        || !claims.raw_author.is_empty()
        || !claims.pointer.is_empty()
        || claims.harness_written
        || claims.compaction_summary
}

/// One place on the line: its node, and the item there.
#[derive(Clone, Copy)]
struct Spot {
    node: Id,
    fact: Id,
    held: bool,
    out: bool,
    /// The record that holds the item, and the node it holds it in.
    own: Option<(usize, Id)>,
}

/// [`Order::File`]: one line, in file order. A header or prefix record
/// sets its part of the opening and the line is re-presented under it; a
/// replacement record starts the line again after the opening; a turn
/// record marks where a turn starts, and a rollback removes the latest
/// turns; any other record extends the line. Claims of a record that holds
/// no node go to the next node the file places (X19 where the file ends
/// before it). `reads` are the records as the JSON they hold.
#[allow(clippy::too_many_arguments)]
fn file(
    t: &Table,
    unit: &Unit,
    records: &[Id],
    reads: &[Json],
    record_cat: &Cat,
    lines: &mut Lines,
    header_gap: Id,
    rules: &FileWindows,
    wiring: &mut Wiring,
) -> Result<(), String> {
    let table = t.table;
    // X18': the budgets the unit's clients could have cut an output under
    // unrecorded, by the versions of the records that name them.
    let unrecorded = rules.budget.as_ref().map(|budget| {
        let ((field, client), version) = budget.client;
        Unrecorded::of_clients(
            reads
                .iter()
                .filter(|read| read.str_at(field) == Some(client))
                .map(|read| read.str_at(version)),
        )
    });
    // X18: the latest header's budget.
    let mut header_budget: Option<Budget> = None;
    // X1, X13: the phase each of her messages that holds none takes from
    // the copy that names it, by record, under the table's one supply.
    let mut supplies = table.entries.iter().filter_map(|entry| match &entry.class {
        Class::Supply(supply) => Some(supply),
        _ => None,
    });
    let supply = supplies.next();
    if supplies.next().is_some() {
        return Err("not built yet: several supplies".to_owned());
    }
    let phases = match supply {
        Some(supply) if supply.attribute != "register" => {
            return Err(format!("not built yet: a supply of {}", supply.attribute))
        }
        Some(_) => supplied_phases(reads).map_err(|failure| failure.to_string())?,
        None => HashMap::new(),
    };
    let marked = records
        .iter()
        .any(|&record| file_kind(rules, unit, record) == FileKind::Turn);
    let mut header: Option<Id> = None;
    let mut prefix: Vec<Id> = Vec::new();
    // The opening's last node, and the nodes of the recorded prefix on it.
    let opening = |lines: &mut Lines, header: Option<Id>, prefix: &[Id]| {
        let mut tail = Some(lines.place(None, header.unwrap_or(header_gap), true, None)?);
        let mut at_prefix = Vec::new();
        if prefix.is_empty() {
            for g in table.gaps.prefix {
                let at = lines.fact(gap(g)?)?;
                tail = Some(lines.place(tail, at, true, None)?);
            }
        } else {
            for &fact_id in prefix {
                let at = lines.place(tail, fact_id, true, None)?;
                at_prefix.push(at);
                tail = Some(at);
            }
        }
        for g in table.gaps.after_prefix {
            let at = lines.fact(gap(g)?)?;
            tail = Some(lines.place(tail, at, true, None)?);
        }
        Ok::<_, String>((tail, at_prefix))
    };
    let (mut root, _) = opening(lines, None, &[])?;
    let mut line: Vec<Spot> = Vec::new();
    let mut turns: Vec<usize> = Vec::new();
    // Every (record, node) a record holds, each record's claims, and the
    // claims waiting for the next node placed.
    let mut held: Vec<(usize, Id)> = Vec::new();
    let mut claims_of: HashMap<usize, WitnessAnnotations> = HashMap::new();
    let mut pending: Vec<(WitnessAnnotations, &'static str)> = Vec::new();
    let mut extra: Vec<(Id, WitnessAnnotations)> = Vec::new();
    // (record, node, term): why the sampling of what that record holds at
    // that node stopped (M1).
    let mut ends: Vec<(usize, Id, Id)> = Vec::new();
    // Each call id's latest opening: its place on the line and its node.
    let mut latest: HashMap<String, (usize, Id)> = HashMap::new();
    // The node the prefix record held each recorded prefix item in.
    let mut prefix_own: Vec<Option<Id>> = Vec::new();
    let witness = |held: &mut Vec<(usize, Id)>,
                   pending: &mut Vec<(WitnessAnnotations, &'static str)>,
                   extra: &mut Vec<(Id, WitnessAnnotations)>,
                   record: usize,
                   node: Id| {
        held.push((record, node));
        for (claims, _) in pending.drain(..) {
            extra.push((node, claims));
        }
    };
    let since = |turns: &[usize], line: &[Spot]| turns.last().copied().unwrap_or(0).min(line.len());
    for (at, &record) in records.iter().enumerate() {
        let kind = file_kind(rules, unit, record);
        // X18: a header sets the budget, a record's own limit overrides it.
        // The outputs the table names are response items, each recorded as
        // it entered her history (X18').
        let history = match (&rules.budget, unrecorded) {
            (Some(budget), Some(unrecorded)) => {
                if kind == FileKind::Header {
                    header_budget = Budget::of_header(reads[at].at(budget.policy.0))
                        .map_err(|failure| failure.to_string())?;
                }
                let own = Budget::of_record(reads[at].at(budget.limit.0), header_budget)
                    .map_err(|failure| failure.to_string())?;
                let history = History {
                    budget: own,
                    unrecorded,
                    recorded_here: true,
                };
                Some((budget, history))
            }
            _ => None,
        };
        let supplied = supply.map(|supply| (supply, phases.get(&at).map(String::as_str)));
        let parts = record_parts(t, unit, record, record_cat, history, supplied)?;
        if !parts.file_tail.is_empty()
            || !parts.parent_tail.is_empty()
            || !parts.rewrites.is_empty()
            || !parts.request_ends.is_empty()
            || parts.unrecorded
        {
            return Err("not built yet: a place only parent order names".to_owned());
        }
        for built in &parts.built {
            lines.fact(built.clone())?;
        }
        let items: Vec<&Item> = parts.items.iter().collect();
        let claims = parts.claims.clone();
        // A header or prefix record that holds nothing, and a record that
        // bears no item at all, claim for the next node placed; an item
        // record whose values rendered nothing claims for none.
        let opens = matches!(kind, FileKind::Header | FileKind::Prefix);
        if !items.is_empty() {
            claims_of.insert(at, claims.clone());
        } else if opens || (!parts.bears && claims_something(&claims)) {
            pending.push((claims.clone(), "<record>"));
        }
        // X17: his prompt after her answer starts a turn where the unit
        // marks none.
        let prompt = rules
            .prompt
            .iter()
            .any(|when| !when.is_empty() && holds(when, unit, Some(record), None));
        if !marked && prompt {
            let from = since(&turns, &line);
            if line[from..].iter().any(|spot| spot.held && spot.out) {
                turns.push(line.len());
            }
        }
        // Place the items after `tail`, extending the line.
        let mut place_items = |lines: &mut Lines,
                               line: &mut Vec<Spot>,
                               latest: &mut HashMap<String, (usize, Id)>,
                               tail: Option<Id>|
         -> Result<(), String> {
            let mut tail = tail;
            for item in &items {
                let node = lines.place(tail, item.fact, false, None)?;
                tail = Some(node);
                // X4: a result answers the latest call with its id, which
                // must still stand on her line.
                if let Some(call) = &item.answers {
                    let found = latest.get(call).and_then(|&(position, own)| {
                        line.get(position)
                            .filter(|spot| spot.own.map(|(_, n)| n) == Some(own))
                            .map(|spot| spot.node)
                    });
                    let Some(target) = found else {
                        return Err("a result whose call is not in its window".to_owned());
                    };
                    let mark = lines.marks.entry(node).or_default();
                    if !mark.responds_to.contains(&target) {
                        mark.responds_to.push(target);
                    }
                }
                for call in &item.calls {
                    latest.insert(call.clone(), (line.len(), node));
                }
                for (category, text, part) in &item.glosses {
                    lines.out += described(node, *category, text, *part)?;
                }
                line.push(Spot {
                    node,
                    fact: item.fact,
                    held: item.held,
                    out: item.out,
                    own: item.held.then_some((at, node)),
                });
                if item.held {
                    witness(&mut held, &mut pending, &mut extra, at, node);
                }
            }
            Ok(())
        };
        match kind {
            FileKind::Header | FileKind::Prefix => {
                let facts: Vec<Id> = items.iter().map(|item| item.fact).collect();
                if kind == FileKind::Header {
                    if facts.len() > 1 {
                        return Err("a header record that holds more than one item".to_owned());
                    }
                    header = facts.first().copied();
                } else {
                    prefix = facts;
                    prefix_own.clear();
                }
                // The line re-presented under the new opening: each copy
                // recalls the node its record held, the prefix's too.
                let (tail, at_prefix) = opening(lines, header, &prefix)?;
                for (node, own) in at_prefix.iter().zip(&prefix_own) {
                    if let Some(own) = own {
                        lines.copied(*node, *own);
                    }
                }
                if tail != root {
                    root = tail;
                    let mut cursor = root;
                    for spot in line.iter_mut() {
                        let own = spot.own.map(|(_, own)| own);
                        let node = lines.place(cursor, spot.fact, false, own)?;
                        if let Some(own) = own {
                            lines.copied(node, own);
                        }
                        spot.node = node;
                        cursor = Some(node);
                    }
                }
                if kind == FileKind::Header {
                    if let Some(fact_id) = header {
                        let node = lines.place(None, fact_id, true, None)?;
                        witness(&mut held, &mut pending, &mut extra, at, node);
                    }
                } else {
                    for (item, node) in items.iter().zip(at_prefix) {
                        prefix_own.push(item.held.then_some(node));
                        if item.held {
                            witness(&mut held, &mut pending, &mut extra, at, node);
                        }
                    }
                }
            }
            FileKind::Replace => {
                if items.is_empty() {
                    return Err("a compaction whose new context holds nothing".to_owned());
                }
                let old_tip = line.last().map(|spot| spot.node).or(root);
                let tail = line
                    .iter()
                    .rev()
                    .find_map(|spot| spot.own.map(|(_, own)| own))
                    .or(old_tip);
                line.clear();
                turns.clear();
                latest.clear();
                place_items(lines, &mut line, &mut latest, root)?;
                // X2: the first node of the new line that is not on the old
                // line continues the old tail.
                let mut on_old: HashSet<Id> = HashSet::new();
                let mut cursor = old_tip;
                while let Some(node) = cursor {
                    on_old.insert(node);
                    cursor = lines.places[&node].previous;
                }
                let mut new_line = Vec::new();
                let mut cursor = line.last().map(|spot| spot.node);
                while let Some(node) = cursor {
                    new_line.push(node);
                    cursor = lines.places[&node].previous;
                }
                let first = new_line
                    .into_iter()
                    .rev()
                    .find(|node| !on_old.contains(node))
                    .ok_or("a compaction whose new context lies on the old line")?;
                if let Some(tail) = tail {
                    let mark = lines.marks.entry(first).or_default();
                    if !mark.continues.contains(&tail) {
                        mark.continues.push(tail);
                    }
                }
            }
            FileKind::Turn => turns.push(line.len()),
            FileKind::Rollback => {
                let count = field_at(unit, record, rules.rollback_count)
                    .and_then(|v| spelling(unit, v))
                    .and_then(|count| count.parse::<usize>().ok())
                    .ok_or_else(|| {
                        format!("{}: a rollback count is no count", rules.rollback_count)
                    })?;
                if count > turns.len() {
                    return Err("a rollback of more turns than the line holds".to_owned());
                }
                if count > 0 {
                    let keep = turns.len() - count;
                    line.truncate(turns[keep]);
                    turns.truncate(keep);
                }
            }
            FileKind::Content => {
                let tail = line.last().map(|spot| spot.node).or(root);
                place_items(lines, &mut line, &mut latest, tail)?;
            }
        }
        // Why the turn ended, from a record after its items: on her last
        // item of the turn (`LastOut`), or a claim on the turn's last node,
        // or on the next node placed when the turn holds none (X15).
        for (_, term, place, path) in &parts.turn {
            let from = since(&turns, &line);
            let turn = &line[from..];
            let last_out = turn
                .iter()
                .rev()
                .find(|spot| spot.held && spot.out)
                .and_then(|spot| spot.own);
            match place {
                Place::LastOut => {
                    if let Some((by, node)) = last_out {
                        ends.push((by, node, *term));
                    }
                }
                Place::TurnTailWithoutOut if last_out.is_some() => {}
                _ => {
                    let claims = WitnessAnnotations {
                        turn_ends: vec![*term],
                        ..WitnessAnnotations::default()
                    };
                    match turn.iter().rev().find_map(|spot| spot.own) {
                        Some((_, node)) => extra.push((node, claims)),
                        None => pending.push((claims, path)),
                    }
                }
            }
        }
    }
    // X19: claims that wait for a node the file does not reach.
    if !held.is_empty() {
        if let Some((_, path)) = pending.first() {
            return Err(format!(
                "{path}: a record's claims wait for a node beyond the end of the file"
            ));
        }
    }
    for (record, node) in held {
        let mut claims = claims_of.get(&record).cloned().unwrap_or_default();
        ends.retain(|&(by, at, term)| {
            if (by, at) != (record, node) {
                return true;
            }
            if !claims.ends.contains(&term) {
                claims.ends.push(term);
            }
            false
        });
        wiring.occurrences.push((node, claims));
    }
    if !ends.is_empty() {
        return Err("a turn end on an item no record holds".to_owned());
    }
    wiring.occurrences.extend(extra);
    Ok(())
}

/// What both orders end with: each result's link to its call, the nodes
/// on the windows records hold, and the witnesses.
fn finish(table: &FieldTable, mut lines: Lines, wiring: Wiring) -> Result<Fragment, String> {
    let Wiring {
        calls_at,
        answers,
        occurrences,
    } = wiring;
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
    // Every node on the line of a node a record holds, and nothing else: a
    // line built along the way that no record's window holds is not
    // emitted.
    let mut emitted: HashSet<Id> = HashSet::new();
    for (node, _) in &occurrences {
        let mut cursor = Some(*node);
        while let Some(at) = cursor {
            if !emitted.insert(at) {
                break;
            }
            cursor = lines.places[&at].previous;
        }
    }
    let Lines {
        mut out,
        marks,
        nodes,
        facts: staged,
        places,
        ..
    } = lines;
    for node in &emitted {
        out += nodes[node].clone();
        if let Some(held) = staged.get(&places[node].fact) {
            out += held.clone();
        }
    }
    for (node, annotations) in &marks {
        if emitted.contains(node) {
            out += annotate_node(*node, annotations);
        }
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
