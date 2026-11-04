//! The importer's check of one record against the field table: every value
//! must have exactly one class, every vocabulary value must be named, every
//! empty drop must be empty and every mirror must equal what it mirrors. An
//! open field fails. Errors name a path and a reason, never a value.
//!
//! It returns the values whose entry makes them members of an assembled
//! structure fact ([`Decode::Member`]), each with the array levels it sits
//! under, for the importer to build.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use base64::Engine;

use super::json::Json;
use super::Failure;
use crate::archive_v3::fields::{
    Class, Cond, Decode, DropReason, Entry, FieldTable, Mirror, Structure,
};
use crate::archive_v3::MediaType;

/// A name safe to put in a path: field names and vocabulary only.
pub(crate) fn printable(name: &str) -> bool {
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
pub(crate) struct Member<'a> {
    /// `(array path, position)` of each array it sits in, outermost first.
    pub levels: Vec<(String, usize)>,
    /// Its JSON Pointer in the assembled structure.
    pub pointer: &'static str,
    pub modality: &'static str,
    pub direction: &'static str,
    pub value: &'a Json,
}

/// Check one record (one JSONL line) against `table`.
pub(crate) fn record<'a>(
    table: &'static FieldTable,
    root: &'a Json,
) -> Result<Vec<Member<'a>>, Failure> {
    let mut unit = UnitMirrors::new(table);
    let members = record_in(table, root, &mut unit)?;
    unit.verify()?;
    Ok(members)
}

/// Check one record of a unit whose mirrors may reach across records: the
/// unit mirrors are verified by [`UnitMirrors::verify`] once every record is
/// checked.
pub(crate) fn record_in<'a>(
    table: &'static FieldTable,
    root: &'a Json,
    unit: &mut UnitMirrors,
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
        unit: RefCell::new(unit),
    };
    checker.value(root, "", None, &mut Vec::new())?;
    Ok(checker.members.into_inner())
}

/// The values a unit's mirrors compare against, and the mirrors still to
/// check.
pub(crate) struct UnitMirrors {
    /// The table's entries by path, in table order.
    by_path: HashMap<&'static str, Vec<&'static Entry>>,
    watched: HashMap<&'static str, (HashSet<[u8; 32]>, Vec<String>)>,
    /// (path, mirror, the value's hash, the string when it is one)
    pending: Vec<(String, Mirror, [u8; 32], Option<String>)>,
}

impl UnitMirrors {
    pub(crate) fn new(table: &'static FieldTable) -> Self {
        let mut watched = HashMap::new();
        let mut by_path: HashMap<&'static str, Vec<&'static Entry>> = HashMap::new();
        for entry in table.entries {
            by_path.entry(entry.path).or_default().push(entry);
        }
        for entry in table.entries {
            if let Class::Drop(DropReason::Mirror(Mirror::Unit(paths) | Mirror::Within(paths))) =
                entry.class
            {
                for path in paths {
                    watched.entry(*path).or_insert_with(Default::default);
                }
            }
        }
        UnitMirrors {
            by_path,
            watched,
            pending: Vec::new(),
        }
    }

    fn see(&mut self, path: &str, value: &Json) {
        if let Some((hashes, strings)) = self.watched.get_mut(path) {
            hashes.insert(value.hash());
            if let Json::Str(text) = value {
                strings.push(text.clone());
            }
        }
    }

    /// Every unit mirror equals (or sits inside) what it mirrors.
    pub(crate) fn verify(&self) -> Result<(), Failure> {
        let mut done: HashSet<([u8; 32], usize)> = HashSet::new();
        for (path, mirror, hash, text) in &self.pending {
            let key = match mirror {
                Mirror::Unit(paths) | Mirror::Within(paths) => (*hash, paths.as_ptr() as usize),
                _ => (*hash, 0),
            };
            if !done.insert(key) {
                continue;
            }
            let equal = |paths: &[&str]| {
                paths.iter().any(|at| {
                    self.watched
                        .get(at)
                        .is_some_and(|(hashes, _)| hashes.contains(hash))
                })
            };
            let held = match mirror {
                Mirror::Unit(paths) => equal(paths),
                Mirror::Within(paths) => {
                    equal(paths)
                        || text.as_deref().is_some_and(|text| {
                            paths.iter().any(|at| {
                                self.watched.get(at).is_some_and(|(_, strings)| {
                                    strings.iter().any(|haystack| haystack.contains(text))
                                })
                            })
                        })
                }
                _ => true,
            };
            if !held {
                return Err(Failure::new(
                    path,
                    "a copy the unit does not hold what it copies",
                ));
            }
        }
        Ok(())
    }
}

struct Checker<'a, 'u> {
    table: &'static FieldTable,
    record: Record<'a>,
    members: RefCell<Vec<Member<'a>>>,
    unit: RefCell<&'u mut UnitMirrors>,
}

fn takes(decode: Decode, value: &Json) -> bool {
    match decode {
        Decode::Json | Decode::Member(_) | Decode::ConstText(_) | Decode::ConstTerm(_) => true,
        Decode::Text
        | Decode::Base64
        | Decode::Base64Url
        | Decode::DataUri
        | Decode::DataUriType
        | Decode::JsonText
        | Decode::MediaType
        | Decode::Timestamp => matches!(value, Json::Str(_)),
        Decode::Number | Decode::UnixSeconds => matches!(value, Json::Num(_)),
        Decode::Bool => matches!(value, Json::Bool(_)),
        Decode::Term(_) | Decode::TextMap(_) => value.is_scalar(),
    }
}

impl<'a, 'u> Checker<'a, 'u> {
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
            Cond::SiblingAbsent(name) => sibling(name).is_none_or(|v| v == "null"),
        })
    }

    fn takes(entry: &Entry, value: &Json) -> bool {
        match entry.class {
            Class::Fact { decode, .. } => takes(decode, value),
            Class::Annotation(list) => list.iter().all(|(_, decode)| takes(*decode, value)),
            // A correlator names one id: an object or array under its path
            // is walked member by member and fails where the table names
            // nothing, as the gate's walker does (C1).
            Class::Structure(_) | Class::Correlator => value.is_scalar() || value.is_empty(),
            Class::Drop(DropReason::Empty) => value.is_scalar() || value.is_empty(),
            Class::Rewrite(_) => matches!(value, Json::Obj(_)),
            Class::Supply(supply) => value.is_scalar() && takes(supply.decode, value),
            Class::Drop(_) | Class::Open(_) => true,
        }
    }

    fn entries(&self, at: &str, value: &Json, parent: Option<&Json>) -> Vec<&'static Entry> {
        let mut found: Vec<&'static Entry> = self
            .unit
            .borrow()
            .by_path
            .get(at)
            .into_iter()
            .flatten()
            .copied()
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

    /// The object's shape: each discriminator member it carries, in table
    /// order, joined by `}{`.
    fn shape(&self, value: &Json) -> Option<String> {
        let shapes: Vec<String> = self
            .table
            .discriminators
            .iter()
            .filter_map(|key| match value.get(key) {
                Some(Json::Str(shape)) if printable(shape) => Some(shape.clone()),
                Some(Json::Str(_)) => Some("?".to_owned()),
                _ => None,
            })
            .collect();
        (!shapes.is_empty()).then(|| shapes.join("}{"))
    }

    fn value(
        &self,
        value: &'a Json,
        path: &str,
        parent: Option<&'a Json>,
        levels: &mut Vec<(String, usize)>,
    ) -> Result<(), Failure> {
        self.unit.borrow_mut().see(path, value);
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
            (Decode::Base64Url, Json::Str(text)) => base64_url(text).is_some(),
            (Decode::DataUri, Json::Str(text)) => data_uri(text).is_some(),
            (Decode::DataUriType, Json::Str(text)) => data_uri(text).is_some(),
            (Decode::JsonText, Json::Str(text)) => super::json::parse(text.as_bytes()).is_some(),
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
            Class::Drop(DropReason::Mirror(mirror @ (Mirror::Unit(_) | Mirror::Within(_)))) => {
                let text = value.str().map(str::to_owned);
                if matches!(mirror, Mirror::Within(_)) && text.is_none() {
                    return Err(Failure::new(path, "a copy inside text is not text"));
                }
                self.unit
                    .borrow_mut()
                    .pending
                    .push((path.to_owned(), mirror, value.hash(), text));
                Ok(())
            }
            Class::Drop(DropReason::Mirror(Mirror::Field(field))) => {
                if self
                    .record
                    .root
                    .at(field)
                    .is_some_and(|original| original.same(value))
                {
                    Ok(())
                } else {
                    Err(Failure::new(path, "a mirror differs from what it mirrors"))
                }
            }
            Class::Drop(DropReason::Mirror(Mirror::Children)) => Err(Failure::new(
                path,
                "a children mirror is not checked by this importer",
            )),
            // The importer builds the changed window; here each member must
            // name a call of the record.
            Class::Rewrite(rewrite) => {
                let Json::Obj(sent) = value else {
                    return Err(Failure::new(path, "a rewrite is not an object"));
                };
                let elements = self
                    .record
                    .root
                    .at(rewrite.array)
                    .map(Json::arr)
                    .unwrap_or(&[]);
                for (name, _) in sent {
                    let named = elements
                        .iter()
                        .filter(|element| element.str_at(rewrite.key) == Some(name.as_str()))
                        .count();
                    if named != 1 {
                        return Err(Failure::new(
                            path,
                            "a rewritten member names no single call of its record",
                        ));
                    }
                }
                Ok(())
            }
            // The importer pairs a supply with its item itself; here its
            // value must be one the table names.
            Class::Supply(supply) => Self::decodes(supply.decode, value, path),
            Class::Open(why) => Err(Failure::new(path, why)),
        }
    }
}

/// The bytes of a URL-safe base64 string, padding optional.
pub(crate) fn base64_url(text: &str) -> Option<Vec<u8>> {
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    let engine = GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    engine.decode(text).ok()
}

/// The media type and bytes of a base64 `data:` URI.
pub(crate) fn data_uri(text: &str) -> Option<(MediaType, Vec<u8>)> {
    let rest = text.strip_prefix("data:")?;
    let (head, data) = rest.split_once(',')?;
    let media_type = MediaType::parse(head.strip_suffix(";base64")?).ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    Some((media_type, bytes))
}
