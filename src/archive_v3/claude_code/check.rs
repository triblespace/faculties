//! The importer's check of one record against the field table: every value
//! must have exactly one class, every vocabulary value must be named, every
//! empty drop must be empty and every mirror must equal what it mirrors. An
//! open field fails. Errors name a path and a reason, never a value.
//!
//! It returns the values whose entry makes them members of an assembled
//! structure fact ([`Decode::Member`]), each with the array levels it sits
//! under, for the importer to build.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;

use super::json::Json;
use super::Failure;
use crate::archive_v3::fields::{
    Class, Cond, Decode, DropReason, Entry, FieldTable, Mirror, Structure,
};
use crate::archive_v3::MediaType;

/// A name safe to put in a path: field names and vocabulary only.
pub(super) fn printable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:$@".contains(&b))
}

struct Record<'a> {
    root: &'a Json,
    scalars: BTreeMap<String, String>,
    present: BTreeSet<String>,
}

fn fields(value: &Json, prefix: &str, record: &mut Record) {
    if let Json::Obj(members) = value {
        for (name, member) in members {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            record.present.insert(path.clone());
            match member {
                Json::Obj(_) => fields(member, &path, record),
                Json::Arr(_) => {}
                scalar => {
                    record
                        .scalars
                        .insert(path, scalar.spelling().expect("a scalar"));
                }
            }
        }
    }
}

/// One value that is a member of an assembled structure fact.
#[derive(Debug)]
pub(super) struct Member<'a> {
    /// `(array path, position)` of each array it sits in, outermost first.
    pub levels: Vec<(String, usize)>,
    /// Its JSON Pointer in the assembled structure.
    pub pointer: &'static str,
    pub modality: &'static str,
    pub direction: &'static str,
    pub value: &'a Json,
}

/// Check one record (one JSONL line) against `table`.
pub(super) fn record<'a>(
    table: &'static FieldTable,
    root: &'a Json,
) -> Result<Vec<Member<'a>>, Failure> {
    let mut record = Record {
        root,
        scalars: BTreeMap::new(),
        present: BTreeSet::new(),
    };
    fields(root, "", &mut record);
    let checker = Checker {
        table,
        record,
        members: RefCell::new(Vec::new()),
    };
    checker.value(root, "", None, &mut Vec::new())?;
    Ok(checker.members.into_inner())
}

struct Checker<'a> {
    table: &'static FieldTable,
    record: Record<'a>,
    members: RefCell<Vec<Member<'a>>>,
}

fn takes(decode: Decode, value: &Json) -> bool {
    match decode {
        Decode::Json | Decode::Member(_) | Decode::ConstText(_) | Decode::ConstTerm(_) => true,
        Decode::Text | Decode::Base64 | Decode::MediaType | Decode::Timestamp => {
            matches!(value, Json::Str(_))
        }
        Decode::Number | Decode::UnixSeconds => matches!(value, Json::Num(_)),
        Decode::Bool => matches!(value, Json::Bool(_)),
        Decode::Term(_) | Decode::TextMap(_) => value.is_scalar(),
    }
}

impl<'a> Checker<'a> {
    fn holds(&self, when: &[Cond], parent: Option<&Json>) -> bool {
        let sibling = |name: &str| parent.and_then(|p| p.get(name)).and_then(Json::spelling);
        let scalar = |field: &str| self.record.scalars.get(field);
        when.iter().all(|cond| match *cond {
            Cond::Is(field, value) => scalar(field).is_some_and(|v| v == value),
            Cond::IsNot(field, value) => scalar(field).is_none_or(|v| v != value),
            Cond::Prefix(field, prefix) => scalar(field).is_some_and(|v| v.starts_with(prefix)),
            Cond::Present(field) => self.record.present.contains(field),
            Cond::Absent(field) => !self.record.present.contains(field),
            Cond::Scalar(field) => self.record.scalars.contains_key(field),
            Cond::Sibling(name, value) => sibling(name).is_some_and(|v| v == value),
            Cond::SiblingNot(name, value) => sibling(name).is_none_or(|v| v != value),
        })
    }

    fn takes(entry: &Entry, value: &Json) -> bool {
        match entry.class {
            Class::Fact { decode, .. } => takes(decode, value),
            Class::Annotation(list) => list.iter().all(|(_, decode)| takes(*decode, value)),
            Class::Structure(_) => value.is_scalar() || value.is_empty(),
            Class::Drop(DropReason::Empty) => value.is_scalar() || value.is_empty(),
            Class::Correlator | Class::Drop(_) | Class::Open(_) => true,
        }
    }

    fn entries(&self, at: &str, value: &Json, parent: Option<&Json>) -> Vec<&Entry> {
        let mut found: Vec<&Entry> = self
            .table
            .entries
            .iter()
            .filter(|entry| entry.path == at)
            .filter(|entry| Self::takes(entry, value) && self.holds(entry.when, parent))
            .collect();
        let empty = |entry: &&Entry| matches!(entry.class, Class::Drop(DropReason::Empty));
        if found.len() > 1 && found.iter().any(empty) {
            let valued = found.iter().any(|entry| {
                matches!(
                    entry.class,
                    Class::Fact { .. } | Class::Annotation(_) | Class::Structure(_)
                )
            });
            let keep_empty = !valued && value.is_empty();
            found.retain(|entry| empty(entry) == keep_empty);
        }
        found
    }

    fn shape(&self, value: &Json) -> Option<String> {
        self.table
            .discriminators
            .iter()
            .find_map(|key| match value.get(key) {
                Some(Json::Str(shape)) if printable(shape) => Some(shape.clone()),
                Some(Json::Str(_)) => Some("?".to_owned()),
                _ => None,
            })
    }

    fn value(
        &self,
        value: &'a Json,
        path: &str,
        parent: Option<&'a Json>,
        levels: &mut Vec<(String, usize)>,
    ) -> Result<(), Failure> {
        let shaped = self.shape(value).map(|shape| format!("{path}{{{shape}}}"));
        let mut entries = match &shaped {
            Some(shaped) => self.entries(shaped, value, parent),
            None => Vec::new(),
        };
        if entries.is_empty() {
            entries = self.entries(path, value, parent);
        }
        match entries.as_slice() {
            [entry] => {
                if let Class::Fact {
                    decode: Decode::Member(pointer),
                    modality,
                    direction,
                    ..
                } = entry.class
                {
                    self.members.borrow_mut().push(Member {
                        levels: levels.clone(),
                        pointer,
                        modality,
                        direction,
                        value,
                    });
                }
                return self.apply(entry, value, entry.path, parent);
            }
            [] => {}
            _ => return Err(Failure::new(path, "several table entries apply")),
        }
        let inner = shaped.unwrap_or_else(|| path.to_owned());
        match value {
            Json::Obj(members) if !members.is_empty() => {
                for (name, member) in members {
                    let child = if self.table.id_keyed.contains(&path) {
                        format!("{inner}.<id>")
                    } else if printable(name) {
                        format!("{inner}.{name}")
                    } else {
                        format!("{inner}.<key>")
                    };
                    self.value(member, &child, Some(value), levels)?;
                }
                Ok(())
            }
            Json::Arr(elements) if !elements.is_empty() => {
                let child = format!("{inner}[]");
                for (at, element) in elements.iter().enumerate() {
                    levels.push((inner.clone(), at));
                    self.value(element, &child, Some(value), levels)?;
                    levels.pop();
                }
                Ok(())
            }
            _ => Err(Failure::new(path, "the table names no class for this path")),
        }
    }

    fn decodes(decode: Decode, value: &Json, path: &str) -> Result<(), Failure> {
        let ok = match (decode, value) {
            (Decode::Base64, Json::Str(text)) => base64::engine::general_purpose::STANDARD
                .decode(text)
                .is_ok(),
            (Decode::MediaType, Json::Str(text)) => MediaType::parse(text).is_ok(),
            (Decode::Timestamp, Json::Str(text)) => super::time(text).is_some(),
            (Decode::Term(map) | Decode::TextMap(map), value) => {
                let spelled = value.spelling();
                if !map
                    .iter()
                    .any(|(source, _)| Some(*source) == spelled.as_deref())
                {
                    return Err(Failure::new(
                        path,
                        "a vocabulary value the table does not name",
                    ));
                }
                true
            }
            _ => true,
        };
        if ok {
            Ok(())
        } else {
            Err(Failure::new(
                path,
                "a value does not decode as its entry says",
            ))
        }
    }

    fn apply(
        &self,
        entry: &Entry,
        value: &Json,
        path: &str,
        parent: Option<&Json>,
    ) -> Result<(), Failure> {
        match entry.class {
            Class::Fact { decode, .. } => Self::decodes(decode, value, path),
            Class::Annotation(list) => list
                .iter()
                .try_for_each(|(_, decode)| Self::decodes(*decode, value, path)),
            Class::Structure(Structure::Discriminator) => match value {
                Json::Str(_) => Ok(()),
                _ => Err(Failure::new(path, "a discriminator is not a string")),
            },
            Class::Structure(Structure::Order) => match value {
                Json::Str(_) | Json::Null => Ok(()),
                _ => Err(Failure::new(path, "an order link is not a string")),
            },
            Class::Structure(Structure::Vocabulary(terms)) => {
                if value
                    .spelling()
                    .is_some_and(|v| terms.contains(&v.as_str()))
                {
                    Ok(())
                } else {
                    Err(Failure::new(
                        path,
                        "a vocabulary value the table does not name",
                    ))
                }
            }
            Class::Structure(Structure::Shape(_)) | Class::Correlator => Ok(()),
            Class::Drop(DropReason::Machinery(_) | DropReason::ProviderId) => Ok(()),
            Class::Drop(DropReason::Empty) => {
                if value.is_empty() {
                    Ok(())
                } else {
                    Err(Failure::new(path, "an empty drop is not empty"))
                }
            }
            Class::Drop(DropReason::Mirror(Mirror::Sibling(name))) => {
                if parent
                    .and_then(|p| p.get(name))
                    .is_some_and(|mirrored| mirrored.same(value))
                {
                    Ok(())
                } else {
                    Err(Failure::new(path, "a mirror differs from what it mirrors"))
                }
            }
            Class::Drop(DropReason::Mirror(Mirror::Keyed { array, key, member })) => {
                let Json::Obj(mirrors) = value else {
                    return Err(Failure::new(path, "a keyed mirror is not an object"));
                };
                let elements = self.record.root.at(array).map(Json::arr).unwrap_or(&[]);
                for (name, mirror) in mirrors {
                    let original = elements
                        .iter()
                        .find(|element| element.str_at(key) == Some(name.as_str()))
                        .and_then(|element| element.get(member));
                    if !original.is_some_and(|original| original.same(mirror)) {
                        return Err(Failure::new(path, "a mirror differs from what it mirrors"));
                    }
                }
                Ok(())
            }
            Class::Drop(DropReason::Mirror(Mirror::Children)) => Err(Failure::new(
                path,
                "a children mirror is not checked by this importer",
            )),
            Class::Open(why) => Err(Failure::new(path, why)),
        }
    }
}
