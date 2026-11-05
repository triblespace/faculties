//! Round 7: a field table compiled into a construction grammar.
//!
//! The gate's walker ([`super::gate::walk`]) interprets a [`FieldTable`] by
//! hand. This states the same rule for one value as constructions for
//! `triblespace_cxg`: the entries at the value's shaped path that apply to it
//! or, when none does, the entries at its own path; exactly one of them
//! consumes the whole value; with none, a leaf fails and anything else is
//! descended into. The context the walk keeps, the record and the object
//! that holds the value, travels in the category, so an anchor reads nothing
//! its parent did not hand it.
//!
//! This is the construction half only. Order, windows, gaps, links, mirrors,
//! rewrites and supplies are relations over the image, and are not here, so
//! a unit this accepts may still fail the importer for one of those.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use triblespace::core::import::json_tree;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{Handle, U256BE};
use triblespace::prelude::*;
use triblespace_cxg::{Category, Constituent, Construction, Grammar, PathStep, Role, Step, Unit};

use super::fields::{Beside, Class, Cond, Decode, DropReason, Entry, FieldTable, Structure};
use super::gate::id_like;
use super::{fact, unexported, FactCore};
use crate::schemas::archive_v3 as v3;

type Name = Inline<Handle<UTF8String>>;

/// Where a value sits, and the context the walk keeps for it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cat {
    /// The normalized path the value sits at.
    pub path: String,
    /// The record the value belongs to, once the walk is inside one.
    pub record: Option<Id>,
    /// The object or array that holds the value.
    pub parent: Option<Id>,
}

impl Category for Cat {
    type Family = String;
    fn family(&self) -> String {
        self.path.clone()
    }
}

/// The family of a JSON Lines unit's root: the array of its lines.
pub const LINES: &str = "<lines>";

/// The category a JSON Lines unit's root is covered as.
pub fn lines() -> Cat {
    Cat {
        path: LINES.to_owned(),
        record: None,
        parent: None,
    }
}

fn name(text: &str) -> Name {
    let blob: Blob<UTF8String> = text.to_blob();
    blob.get_handle()
}

thread_local! {
    /// Node kinds read in one unit ([`Unit::run`]). A reading, an absence
    /// above all, says nothing about another unit, so the cache starts over
    /// with every unit.
    static KINDS: std::cell::RefCell<(u64, HashMap<Id, Option<Id>>)> =
        std::cell::RefCell::new((u64::MAX, HashMap::new()));
}

fn kind(unit: &Unit, e: Id) -> Option<Id> {
    let cached = KINDS.with(|kinds| {
        let mut kinds = kinds.borrow_mut();
        if kinds.0 != unit.run() {
            *kinds = (unit.run(), HashMap::new());
        }
        kinds.1.get(&e).copied()
    });
    if let Some(k) = cached {
        return k;
    }
    let facts = unit.facts;
    let k = find!((k: Id), pattern!(facts, [{ e @ json_tree::kind: ?k }]))
        .map(|(k,)| k)
        .next();
    KINDS.with(|kinds| kinds.borrow_mut().1.insert(e, k));
    k
}

fn text_of(unit: &Unit, e: Id) -> Option<String> {
    let facts = unit.facts;
    find!((h: Name), pattern!(facts, [{ e @ json_tree::string: ?h }]))
        .next()
        .and_then(|(h,)| unit.text(h))
}

/// A scalar's spelling, as conditions and vocabularies compare it.
fn spelling(unit: &Unit, e: Id) -> Option<String> {
    let facts = unit.facts;
    match kind(unit, e)? {
        k if k == json_tree::kind_string => text_of(unit, e),
        k if k == json_tree::kind_number => {
            find!((h: Name), pattern!(facts, [{ e @ json_tree::number_raw: ?h }]))
                .next()
                .and_then(|(h,)| unit.text(h))
        }
        k if k == json_tree::kind_bool => {
            Some(exists!(pattern!(facts, [{ e @ json_tree::boolean: true }])).to_string())
        }
        k if k == json_tree::kind_null => Some("null".to_owned()),
        _ => None,
    }
}

fn is_scalar(unit: &Unit, e: Id) -> bool {
    kind(unit, e).is_some_and(|k| {
        k == json_tree::kind_string
            || k == json_tree::kind_number
            || k == json_tree::kind_bool
            || k == json_tree::kind_null
    })
}

fn is_empty(unit: &Unit, e: Id) -> bool {
    let facts = unit.facts;
    match kind(unit, e) {
        Some(k) if k == json_tree::kind_null => true,
        Some(k) if k == json_tree::kind_string => text_of(unit, e).is_some_and(|t| t.is_empty()),
        Some(k) if k == json_tree::kind_array => {
            !exists!(pattern!(facts, [{ _?entry @ json_tree::array_parent: e }]))
        }
        Some(k) if k == json_tree::kind_object => {
            !exists!(pattern!(facts, [{ e @ json_tree::object_field: _?field }]))
        }
        _ => false,
    }
}

fn is_leaf(unit: &Unit, e: Id) -> bool {
    is_scalar(unit, e) || is_empty(unit, e)
}

/// The value of an object's member, by name. Nothing on an array.
fn member(unit: &Unit, object: Id, field: &str) -> Option<Id> {
    let facts = unit.facts;
    let wanted = name(field);
    find!(
        (value: Id),
        pattern!(facts, [
            { object @ json_tree::object_field: _?field },
            { _?field @ json_tree::field_name: wanted, json_tree::field_value: ?value },
        ])
    )
    .map(|(value,)| value)
    .next()
}

/// A record-relative member path; arrays are not entered.
fn field_at(unit: &Unit, record: Id, path: &str) -> Option<Id> {
    path.split('.')
        .try_fold(record, |at, segment| member(unit, at, segment))
}

fn holds(when: &[Cond], unit: &Unit, record: Option<Id>, parent: Option<Id>) -> bool {
    let sibling = |field: &str| {
        parent
            .and_then(|p| member(unit, p, field))
            .and_then(|v| spelling(unit, v))
    };
    let scalar = |field: &str| {
        record
            .and_then(|r| field_at(unit, r, field))
            .and_then(|v| spelling(unit, v))
    };
    let present = |field: &str| record.and_then(|r| field_at(unit, r, field)).is_some();
    when.iter().all(|cond| match *cond {
        Cond::Sibling(field, value) => sibling(field).is_some_and(|v| v == value),
        Cond::SiblingNot(field, value) => sibling(field).is_none_or(|v| v != value),
        Cond::SiblingAbsent(field) => sibling(field).is_none_or(|v| v == "null"),
        Cond::Is(field, value) => scalar(field).is_some_and(|v| v == value),
        Cond::IsNot(field, value) => scalar(field).is_none_or(|v| v != value),
        Cond::Prefix(field, prefix) => scalar(field).is_some_and(|v| v.starts_with(prefix)),
        Cond::Present(field) => present(field),
        Cond::Absent(field) => !present(field),
        Cond::Scalar(field) => scalar(field).is_some(),
    })
}

fn decode_applies(decode: Decode, unit: &Unit, v: Id) -> bool {
    let k = kind(unit, v);
    match decode {
        Decode::Json | Decode::Member(_) | Decode::ConstText(_) | Decode::ConstTerm(_) => true,
        Decode::Text
        | Decode::VendorModel
        | Decode::TextThen(_)
        | Decode::Base64
        | Decode::Base64Url
        | Decode::DataUri
        | Decode::DataUriType
        | Decode::JsonText
        | Decode::MediaType
        | Decode::Timestamp => k == Some(json_tree::kind_string),
        Decode::Number | Decode::UnixSeconds => k == Some(json_tree::kind_number),
        Decode::Bool => k == Some(json_tree::kind_bool),
        Decode::Term(_) | Decode::TextMap(_) => is_scalar(unit, v),
    }
}

/// Whether an entry's class can take this value (the walker's `applies`).
fn applies(entry: &Entry, unit: &Unit, v: Id) -> bool {
    match entry.class {
        Class::Fact { decode, .. } => decode_applies(decode, unit, v),
        Class::Annotation(list) => list
            .iter()
            .all(|(_, decode)| decode_applies(*decode, unit, v)),
        Class::Structure(_) | Class::Correlator => is_leaf(unit, v),
        Class::Drop(DropReason::Empty) => is_leaf(unit, v),
        Class::Rewrite(_) => kind(unit, v) == Some(json_tree::kind_object),
        Class::Supply(supply) => is_leaf(unit, v) && decode_applies(supply.decode, unit, v),
        Class::Drop(_) | Class::Open(_) => true,
    }
}

fn in_vocabulary(decode: Decode, unit: &Unit, v: Id) -> bool {
    match decode {
        Decode::Term(map) | Decode::TextMap(map) => {
            spelling(unit, v).is_some_and(|s| map.iter().any(|(from, _)| *from == s))
        }
        _ => true,
    }
}

/// Whether the one chosen entry accepts the value: a vocabulary names it, an
/// empty drop is empty, and an open field never does. Mirrors, decodes of
/// bytes and times, and the relational classes are checked elsewhere.
fn accepts(entry: &Entry, unit: &Unit, v: Id) -> bool {
    match entry.class {
        Class::Open(_) => false,
        Class::Drop(DropReason::Empty) => is_empty(unit, v),
        Class::Structure(Structure::Vocabulary(words)) => {
            spelling(unit, v).is_some_and(|s| words.contains(&s.as_str()))
        }
        Class::Fact { decode, .. } => in_vocabulary(decode, unit, v),
        Class::Annotation(list) => list
            .iter()
            .all(|(_, decode)| in_vocabulary(*decode, unit, v)),
        _ => true,
    }
}

fn is_empty_drop(entry: &Entry) -> bool {
    matches!(entry.class, Class::Drop(DropReason::Empty))
}

/// Every path the table names, and every prefix of one that ends before a
/// member, an array or a shape.
fn known_paths(table: &FieldTable) -> HashSet<String> {
    let mut known = HashSet::new();
    for entry in table.entries {
        for (at, c) in entry.path.char_indices() {
            if matches!(c, '.' | '[' | '{') {
                known.insert(entry.path[..at].to_owned());
            }
        }
        known.insert(entry.path.to_owned());
    }
    known
}

struct Table {
    table: &'static FieldTable,
    known: HashSet<String>,
    by_path: HashMap<&'static str, Vec<&'static Entry>>,
    /// The entries chosen for a value, by (value, category), within one
    /// unit ([`Unit::run`]): the answer depends on the whole subtree being
    /// present, which says nothing about another unit. Every entry anchor of
    /// a family asks the same question, so it is answered once.
    chosen: Mutex<(u64, HashMap<(Id, Cat), Vec<&'static Entry>>)>,
}

impl Table {
    fn new(table: &'static FieldTable) -> Self {
        let mut by_path: HashMap<&'static str, Vec<&'static Entry>> = HashMap::new();
        for entry in table.entries {
            by_path.entry(entry.path).or_default().push(entry);
        }
        Table {
            table,
            known: known_paths(table),
            by_path,
            chosen: Mutex::new((u64::MAX, HashMap::new())),
        }
    }

    /// The record of a value at `cat`: the one handed down, or the value
    /// itself where it sits at the table's record path.
    fn record_of(&self, cat: &Cat, e: Id) -> Option<Id> {
        cat.record
            .or_else(|| (cat.path == self.table.record).then_some(e))
    }

    /// The shapes an object carries, in table order, as the walker spells
    /// them: a value the table does not name as `<value>`, an id as `<id>`.
    fn shape(&self, unit: &Unit, v: Id, path: &str) -> Option<String> {
        if kind(unit, v) != Some(json_tree::kind_object) {
            return None;
        }
        let mut at = path.to_owned();
        let mut shapes = Vec::new();
        for key in self.table.discriminators {
            let Some(shape) = member(unit, v, key)
                .filter(|m| kind(unit, *m) == Some(json_tree::kind_string))
                .and_then(|m| text_of(unit, m))
            else {
                continue;
            };
            let named = format!("{at}{{{shape}}}");
            let shape = if id_like(&shape) {
                "<id>".to_owned()
            } else if self.known.contains(&named) {
                shape
            } else {
                "<value>".to_owned()
            };
            at = format!("{at}{{{shape}}}");
            shapes.push(shape);
        }
        (!shapes.is_empty()).then(|| shapes.join("}{"))
    }

    fn candidates(&self, at: &str, unit: &Unit, v: Id, cat: &Cat) -> Vec<&'static Entry> {
        let record = self.record_of(cat, v);
        let mut entries: Vec<&'static Entry> = self
            .by_path
            .get(at)
            .into_iter()
            .flatten()
            .copied()
            .filter(|entry| applies(entry, unit, v) && holds(entry.when, unit, record, cat.parent))
            .collect();
        if entries.len() > 1 && entries.iter().any(|entry| is_empty_drop(entry)) {
            let valued = entries.iter().any(|entry| {
                matches!(
                    entry.class,
                    Class::Fact { .. } | Class::Annotation(_) | Class::Structure(_)
                )
            });
            // An empty drop competes only for an empty value; there it
            // yields to an entry that holds a value and wins over the rest.
            if valued || !is_empty(unit, v) {
                entries.retain(|entry| !is_empty_drop(entry));
            } else {
                entries.retain(|entry| is_empty_drop(entry));
            }
        }
        entries
    }

    /// The entries that apply to the value: at its shaped path first, and
    /// at its own path when none does there.
    fn chosen(&self, unit: &Unit, v: Id, cat: &Cat) -> Vec<&'static Entry> {
        let key = (v, cat.clone());
        {
            let mut cache = self.chosen.lock().unwrap();
            if cache.0 != unit.run() {
                *cache = (unit.run(), HashMap::new());
            }
            if let Some(entries) = cache.1.get(&key) {
                return entries.clone();
            }
        }
        let entries = self.choose(unit, v, cat);
        let mut cache = self.chosen.lock().unwrap();
        if cache.0 == unit.run() {
            cache.1.insert(key, entries.clone());
        }
        entries
    }

    fn choose(&self, unit: &Unit, v: Id, cat: &Cat) -> Vec<&'static Entry> {
        if let Some(shape) = self.shape(unit, v, &cat.path) {
            let shaped = format!("{}{{{shape}}}", cat.path);
            let entries = self.candidates(&shaped, unit, v, cat);
            if !entries.is_empty() {
                return entries;
            }
        }
        self.candidates(&cat.path, unit, v, cat)
    }

    /// The chosen entry of every value under `v` that one entry consumes,
    /// with the value: the walk of one assembly point, which never enters
    /// an array (each element is an assembly point of its own).
    fn consumed(&self, unit: &Unit, v: Id, cat: &Cat, out: &mut Vec<(&'static Entry, Id, Cat)>) {
        let chosen = self.chosen(unit, v, cat);
        if chosen.len() == 1 {
            out.push((chosen[0], v, cat.clone()));
            return;
        }
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
            let child = self.child(unit, v, cat, &PathStep::Field(name));
            // An array an entry consumes whole (a structure member, tool
            // JSON) belongs here. An array the walk descends into does not:
            // each of its elements is an assembly point of its own.
            if kind(unit, value) == Some(json_tree::kind_array)
                && self.chosen(unit, value, &child).len() != 1
            {
                continue;
            }
            self.consumed(unit, value, &child, out);
        }
    }

    /// The facts of one assembly point: its `Fact` entries grouped by
    /// modality and direction. `None` when a field needs a decode not built.
    fn assemble(&self, unit: &Unit, v: Id, cat: &Cat) -> Option<Vec<FactCore>> {
        let mut consumed = Vec::new();
        self.consumed(unit, v, cat, &mut consumed);
        let mut cores: Vec<((&'static str, &'static str), FactCore)> = Vec::new();
        // The fact of one modality and direction at this assembly point.
        fn at<'c>(
            cores: &'c mut Vec<((&'static str, &'static str), FactCore)>,
            modality: &'static str,
            direction: &'static str,
        ) -> Option<&'c mut FactCore> {
            let at = match cores
                .iter()
                .position(|(key, _)| *key == (modality, direction))
            {
                Some(at) => at,
                None => {
                    let core = FactCore {
                        modality: Some(term(v3::fact::modality::SPECS, modality)?),
                        direction: Some(term(v3::fact::direction::SPECS, direction)?),
                        ..FactCore::default()
                    };
                    cores.push(((modality, direction), core));
                    cores.len() - 1
                }
            };
            Some(&mut cores[at].1)
        }
        let mut assembled: Vec<((&'static str, &'static str), Members)> = Vec::new();
        for (entry, value, cat) in consumed {
            // A gap the rendering rule puts on the value's own fact: its body
            // is not in the source.
            for rule in self.table.gaps.beside {
                if rule.at == Beside::Same
                    && rule.path == entry.path
                    && holds(rule.when, unit, self.record_of(&cat, value), cat.parent)
                {
                    let core = at(&mut cores, rule.gap.modality, rule.gap.direction)?;
                    if !rule.gap.kind.is_empty() {
                        core.absent = Some(term(v3::fact::absent::SPECS, rule.gap.kind)?);
                    }
                }
            }
            let Class::Fact {
                modality,
                direction,
                attribute,
                decode,
                with,
            } = entry.class
            else {
                continue;
            };
            let core = at(&mut cores, modality, direction)?;
            if let Decode::Member(pointer) = decode {
                // The importer splits the pointer as it is spelled.
                let steps: Vec<&str> = pointer.split('/').skip(1).collect();
                let mut text = String::new();
                json_text(unit, value, &mut text)?;
                let tree = match assembled
                    .iter()
                    .position(|(key, _)| *key == (modality, direction))
                {
                    Some(at) => &mut assembled[at].1,
                    None => {
                        assembled.push(((modality, direction), Members::Object(Vec::new())));
                        &mut assembled.last_mut()?.1
                    }
                };
                tree.insert(&steps, text)?;
                continue;
            }
            apply(core, attribute, decode, unit, value)?;
            for (attribute, decode) in with {
                apply(core, attribute, *decode, unit, value)?;
            }
        }
        for (key, tree) in assembled {
            let mut text = String::new();
            tree.render(&mut text)?;
            let core = at(&mut cores, key.0, key.1)?;
            core.structure = Some(structure(&text)?);
        }
        // A fact every field of which held no value is no fact: the walker
        // renders nothing for it.
        Some(
            cores
                .into_iter()
                .map(|(_, core)| core)
                .filter(|core| !holds_nothing(core))
                .collect(),
        )
    }

    /// The facts of the value an instance covers, when it sits at an
    /// assembly point: an array element, or the record outside every array.
    /// Either construction can cover one, since a scalar element is
    /// consumed whole by its entry.
    fn emit_point(&self, inst: &triblespace_cxg::Instance<Cat>) -> Fragment {
        let cat = inst.category;
        if !(cat.path.ends_with("[]") || cat.path == self.table.record) {
            return nothing(inst.anchor);
        }
        let Some(cores) = self.assemble(&inst.unit, inst.anchor, cat) else {
            SKIPPED.fetch_add(1, Ordering::Relaxed);
            return nothing(inst.anchor);
        };
        let mut out = Fragment::empty();
        for core in cores {
            match fact(core) {
                Ok(built) => out += unexported(built),
                Err(_) => {
                    SKIPPED.fetch_add(1, Ordering::Relaxed);
                    return nothing(inst.anchor);
                }
            }
        }
        let (_, facts, metafacts, blobs) = out.into_parts();
        Fragment::rooted_from_parts(inst.anchor, facts, metafacts, blobs)
    }

    /// The category of a member of the value at `cat`, by its step.
    fn child(&self, unit: &Unit, v: Id, cat: &Cat, step: &PathStep) -> Cat {
        let inner = match self.shape(unit, v, &cat.path) {
            Some(shape) => format!("{}{{{shape}}}", cat.path),
            None => cat.path.clone(),
        };
        let path = match step {
            PathStep::Field(handle) => {
                let field = unit.text(*handle).unwrap_or_default();
                let named = format!("{inner}.{field}");
                if self.table.id_keyed.contains(&cat.path.as_str()) || id_like(&field) {
                    format!("{inner}.<id>")
                } else if self.known.contains(&named) {
                    named
                } else {
                    format!("{inner}.<key>")
                }
            }
            PathStep::Index(_) => format!("{inner}[]"),
        };
        Cat {
            path,
            record: self.record_of(cat, v),
            parent: Some(v),
        }
    }
}

/// Assembly points the emits skipped because one of their fields needs a
/// decode not built yet (bytes, media types, JSON structure, exit codes):
/// none of their facts is emitted, so none can carry a wrong id.
pub static SKIPPED: AtomicUsize = AtomicUsize::new(0);

fn term(specs: &[(Id, &str)], name: &str) -> Option<Id> {
    specs.iter().find(|(_, n)| *n == name).map(|(id, _)| *id)
}

fn term_specs(attribute: &str) -> Option<&'static [(Id, &'static str)]> {
    Some(match attribute {
        "status" => v3::fact::status::SPECS,
        "detail" => v3::fact::detail::SPECS,
        "register" => v3::fact::register::SPECS,
        "form" => v3::fact::form::SPECS,
        "effort" => v3::fact::effort::SPECS,
        _ => return None,
    })
}

/// The text a text decode reads, as the walker renders it: an empty `Text`
/// is no value (`Some(None)`), and an empty `TextThen` fails, since what an
/// empty rendering became is unverified.
fn decode_text(decode: Decode, unit: &Unit, v: Id) -> Option<Option<String>> {
    Some(match decode {
        Decode::Text => Some(text_of(unit, v)?).filter(|text| !text.is_empty()),
        Decode::TextThen(then) => {
            let text = text_of(unit, v)?;
            if text.is_empty() {
                return None;
            }
            Some(text + then)
        }
        Decode::ConstText(text) => Some(text.to_owned()),
        Decode::TextMap(map) => {
            let spelling = spelling(unit, v)?;
            Some(map.iter().find(|(from, _)| *from == spelling)?.1.to_owned())
        }
        _ => return None,
    })
}

/// The JSON text of a json_tree value. Objects are sets, so member order
/// does not matter to the tree it imports back to; numbers keep their exact
/// token. `None` when a node is not a json_tree node.
fn json_text(unit: &Unit, v: Id, out: &mut String) -> Option<()> {
    let facts = unit.facts;
    let k = kind(unit, v)?;
    if k == json_tree::kind_string {
        out.push_str(&serde_json::to_string(&text_of(unit, v)?).ok()?);
    } else if k == json_tree::kind_number || k == json_tree::kind_bool || k == json_tree::kind_null
    {
        out.push_str(&spelling(unit, v)?);
    } else if k == json_tree::kind_object {
        out.push('{');
        let members: Vec<(Name, Id)> = find!(
            (name: Name, value: Id),
            pattern!(facts, [
                { v @ json_tree::object_field: _?field },
                { _?field @ json_tree::field_name: ?name, json_tree::field_value: ?value },
            ])
        )
        .collect();
        for (i, (name, value)) in members.into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&serde_json::to_string(&unit.text(name)?).ok()?);
            out.push(':');
            json_text(unit, value, out)?;
        }
        out.push('}');
    } else if k == json_tree::kind_array {
        // U256BE is big-endian, so the raw index orders as the number does.
        let mut entries: Vec<(Inline<U256BE>, Id)> = find!(
            (index: Inline<U256BE>, value: Id),
            pattern!(facts, [{ _?entry @
                json_tree::array_parent: v,
                json_tree::array_index: ?index,
                json_tree::array_value: ?value,
            }])
        )
        .collect();
        entries.sort_by(|a, b| (a.0.raw, a.1).cmp(&(b.0.raw, b.1)));
        out.push('[');
        for (i, (_, value)) in entries.into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json_text(unit, value, out)?;
        }
        out.push(']');
    } else {
        return None;
    }
    Some(())
}

/// A json_tree value as its own imported tree, exactly as the importer
/// builds a structure: the same unsalted import of the same JSON.
fn structure(text: &str) -> Option<Fragment> {
    let mut store = triblespace::core::blob::MemoryBlobStore::new();
    let mut tree = triblespace::core::import::json_tree::JsonTreeImporter::new(&mut store, None)
        .import_str(text)
        .ok()?;
    tree.blobs_mut().union(store);
    Some(tree)
}

/// A structure assembled from members (`Decode::Member`), kept as JSON text
/// so numbers keep their exact token.
enum Members {
    Leaf(String),
    Object(Vec<(String, Members)>),
}

impl Members {
    /// Places `value` at the pointer's steps, with the importer's rules: a
    /// member inside a value that is no object, or named twice, fails.
    fn insert(&mut self, steps: &[&str], value: String) -> Option<()> {
        let Members::Object(members) = self else {
            return None;
        };
        let (first, rest) = steps.split_first()?;
        let at = match members.iter().position(|(name, _)| name == first) {
            Some(at) => at,
            None => {
                if rest.is_empty() {
                    members.push(((*first).to_owned(), Members::Leaf(value)));
                    return Some(());
                }
                members.push(((*first).to_owned(), Members::Object(Vec::new())));
                members.len() - 1
            }
        };
        if rest.is_empty() {
            return None;
        }
        members[at].1.insert(rest, value)
    }

    fn render(&self, out: &mut String) -> Option<()> {
        match self {
            Members::Leaf(text) => out.push_str(text),
            Members::Object(members) => {
                out.push('{');
                for (i, (name, member)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(name).ok()?);
                    out.push(':');
                    member.render(out)?;
                }
                out.push('}');
            }
        }
        Some(())
    }
}

/// A base64 `data:` URI's media type and data.
fn data_uri(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("data:")?;
    let (head, data) = rest.split_once(',')?;
    let media_type = head.strip_suffix(";base64")?;
    Some((media_type, data))
}

/// The bytes a bytes decode reads from a value.
fn decode_bytes(decode: Decode, unit: &Unit, v: Id) -> Option<anybytes::Bytes> {
    use base64::Engine;
    let text = text_of(unit, v)?;
    let bytes = match decode {
        Decode::Base64 => base64::engine::general_purpose::STANDARD
            .decode(&text)
            .ok()?,
        Decode::Base64Url => base64::engine::GeneralPurpose::new(
            &base64::alphabet::URL_SAFE,
            base64::engine::GeneralPurposeConfig::new()
                .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
        )
        .decode(&text)
        .ok()?,
        Decode::DataUri => {
            let (_, data) = data_uri(&text)?;
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()?
        }
        _ => return None,
    };
    Some(anybytes::Bytes::from(bytes))
}

/// The media type a media-type decode reads from a value.
fn decode_media_type(decode: Decode, unit: &Unit, v: Id) -> Option<super::media_type::MediaType> {
    let text = match decode {
        Decode::MediaType => text_of(unit, v)?,
        Decode::ConstText(text) => text.to_owned(),
        Decode::DataUriType => data_uri(&text_of(unit, v)?)?.0.to_owned(),
        _ => return None,
    };
    super::media_type::MediaType::parse(&text).ok()
}

/// Sets one attribute of a fact from a value. `None`: a decode not built.
fn apply(core: &mut FactCore, attribute: &str, decode: Decode, unit: &Unit, v: Id) -> Option<()> {
    if let Some(specs) = term_specs(attribute) {
        let name = match decode {
            Decode::ConstTerm(name) => name.to_owned(),
            Decode::Term(map) => {
                let spelling = spelling(unit, v)?;
                map.iter().find(|(from, _)| *from == spelling)?.1.to_owned()
            }
            _ => return None,
        };
        let id = term(specs, &name)?;
        match attribute {
            "status" => core.status = Some(id),
            "detail" => core.detail = Some(id),
            "register" => core.register = Some(id),
            "form" => core.form = Some(id),
            _ => core.effort = Some(id),
        }
        return Some(());
    }
    if attribute == "blob" {
        core.blob = Some(decode_bytes(decode, unit, v)?);
        return Some(());
    }
    if attribute == "media_type" {
        core.media_type = Some(decode_media_type(decode, unit, v)?);
        return Some(());
    }
    if attribute == "structure" {
        let text = match decode {
            Decode::Json => {
                let mut text = String::new();
                json_text(unit, v, &mut text)?;
                text
            }
            Decode::JsonText => text_of(unit, v)?,
            _ => return None,
        };
        core.structure = Some(structure(&text)?);
        return Some(());
    }
    let Some(text) = decode_text(decode, unit, v)? else {
        return Some(());
    };
    match attribute {
        "payload" => core.payload = Some(text),
        "tool_name" => core.tool_name = Some(text),
        "tool_namespace" => core.tool_namespace = Some(text),
        "title" => core.title = Some(text),
        "sense" => core.sense = Some(text),
        "source_uri" => core.source_uri.push(text),
        _ => return None,
    }
    Some(())
}

/// Whether a fact holds nothing beyond its modality and direction.
fn holds_nothing(core: &FactCore) -> bool {
    core.payload.is_none()
        && core.blob.is_none()
        && core.structure.is_none()
        && core.tool_name.is_none()
        && core.tool_namespace.is_none()
        && core.status.is_none()
        && core.exit_code.is_none()
        && core.media_type.is_none()
        && core.source_uri.is_empty()
        && core.title.is_none()
        && core.sense.is_none()
        && core.detail.is_none()
        && core.register.is_none()
        && core.form.is_none()
        && core.absent.is_none()
        && core.effort.is_none()
}

fn nothing(anchor: Id) -> Fragment {
    Fragment::rooted(anchor, TribleSet::new())
}

/// The grammar of a JSON Lines field table whose records are its lines.
pub fn compile(table: &'static FieldTable) -> anyhow::Result<Grammar<Cat>> {
    if !table.jsonl || !table.record.is_empty() {
        anyhow::bail!("only JSON Lines tables whose records are the lines compile yet");
    }
    let t = Arc::new(Table::new(table));
    let mut constructions = vec![Construction {
        name: "lines",
        category: LINES.to_owned(),
        anchor: Box::new(|unit: &Unit, e, _: &Cat| kind(unit, e) == Some(json_tree::kind_array)),
        constituents: vec![Constituent::new(
            Step::Each,
            Role::Child(Cat {
                path: table.record.to_owned(),
                record: None,
                parent: None,
            }),
        )],
        emit: Box::new(|inst| nothing(inst.anchor)),
    }];
    let mut families: Vec<String> = t
        .known
        .iter()
        .filter(|path| !path.ends_with('}'))
        .cloned()
        .collect();
    families.sort();
    for family in families {
        // One construction covers a value with the entry chosen for it: the
        // walk's rule is that exactly one entry applies and accepts, so a
        // family needs no construction per entry, and its anchor asks the
        // question once.
        let entry_table = Arc::clone(&t);
        let entry_emit = Arc::clone(&t);
        constructions.push(Construction {
            name: "entry",
            category: family.clone(),
            anchor: Box::new(move |unit: &Unit, v, cat: &Cat| {
                let chosen = entry_table.chosen(unit, v, cat);
                chosen.len() == 1 && accepts(chosen[0], unit, v)
            }),
            constituents: vec![
                Constituent::new(Step::EachField, Role::Value),
                Constituent::new(Step::Each, Role::Value),
            ],
            emit: Box::new(move |inst| entry_emit.emit_point(inst)),
        });
        let anchor_table = Arc::clone(&t);
        let emit_table = Arc::clone(&t);
        let field_table = Arc::clone(&t);
        let entry_table = Arc::clone(&t);
        constructions.push(Construction {
            name: "descend",
            category: family.clone(),
            anchor: Box::new(move |unit: &Unit, v, cat: &Cat| {
                !is_leaf(unit, v) && anchor_table.chosen(unit, v, cat).is_empty()
            }),
            constituents: vec![
                Constituent::new(
                    Step::EachField,
                    Role::Within(Arc::new(
                        move |unit: &Unit, v, cat: &Cat, step: &PathStep| {
                            field_table.child(unit, v, cat, step)
                        },
                    )),
                ),
                Constituent::new(
                    Step::Each,
                    Role::Within(Arc::new(
                        move |unit: &Unit, v, cat: &Cat, step: &PathStep| {
                            entry_table.child(unit, v, cat, step)
                        },
                    )),
                ),
            ],
            emit: Box::new(move |inst| emit_table.emit_point(inst)),
        });
    }
    Grammar::new(constructions).map_err(|e| anyhow::anyhow!("{e:?}"))
}

pub mod relations;

#[cfg(test)]
mod tests;
