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
use std::sync::{Arc, Mutex};

use triblespace::core::import::json_tree;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::Handle;
use triblespace::prelude::*;
use triblespace_cxg::{Category, Constituent, Construction, Grammar, PathStep, Role, Step, Unit};

use super::fields::{Class, Cond, Decode, DropReason, Entry, FieldTable, Structure};
use super::gate::id_like;

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
    /// Node kinds by node. A json_tree id derives from its content, kind
    /// included, so an answer holds in every unit; the cache is only ever
    /// emptied to bound its size.
    static KINDS: std::cell::RefCell<HashMap<Id, Option<Id>>> = std::cell::RefCell::new(HashMap::new());
}

/// The most node kinds [`KINDS`] keeps before it starts over.
const KIND_CACHE: usize = 1 << 22;

fn kind(unit: &Unit, e: Id) -> Option<Id> {
    if let Some(k) = KINDS.with(|kinds| kinds.borrow().get(&e).copied()) {
        return k;
    }
    let facts = unit.facts;
    let k = find!((k: Id), pattern!(facts, [{ e @ json_tree::kind: ?k }]))
        .map(|(k,)| k)
        .next();
    KINDS.with(|kinds| {
        let mut kinds = kinds.borrow_mut();
        if kinds.len() >= KIND_CACHE {
            kinds.clear();
        }
        kinds.insert(e, k);
    });
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
    /// The entries chosen for a value, by (value, category). Every entry
    /// anchor of a family asks the same question, so it is answered once.
    /// json_tree ids derive from content, and so do the record and parent
    /// ids in a category, so an answer holds in every unit; the cache is
    /// only ever emptied to bound its size.
    chosen: Mutex<HashMap<(Id, Cat), Vec<&'static Entry>>>,
}

/// The most answers the chosen-entry cache keeps before it starts over.
const CHOSEN_CACHE: usize = 1 << 20;

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
            chosen: Mutex::new(HashMap::new()),
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
        if let Some(entries) = self.chosen.lock().unwrap().get(&key) {
            return entries.clone();
        }
        let entries = self.choose(unit, v, cat);
        let mut cache = self.chosen.lock().unwrap();
        if cache.len() >= CHOSEN_CACHE {
            cache.clear();
        }
        cache.insert(key, entries.clone());
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
            emit: Box::new(|inst| nothing(inst.anchor)),
        });
        let anchor_table = Arc::clone(&t);
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
            emit: Box::new(|inst| nothing(inst.anchor)),
        });
    }
    Grammar::new(constructions).map_err(|e| anyhow::anyhow!("{e:?}"))
}

#[cfg(test)]
mod tests;
