//! B: the gate's independent walk over a source unit.
//!
//! It parses the bytes with its own minimal RFC 8259 parser (not the
//! TribleSpace scanner, not an importer's parser), walks every value under
//! its normalized path, and lets the importer's [`FieldTable`] say what the
//! value is. It emits a [`Token`] for each fact and annotation value, keeps
//! the source's order (record parent links, array order within a record),
//! and asserts drops: a mirror must equal what it mirrors, an empty drop must
//! be empty. Anything the table does not name is a row, never a skip.
//!
//! Object members are visited in name order, never in the order the source
//! spelled them: member order carries no information.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::str::FromStr;

use base64::Engine;
use hifitime::{Duration, Epoch};

use super::{JsonLeaf, Kind, Origin, Token, Value};
use crate::archive_v3::fields::{
    Class, Cond, Decode, DropReason, Entry, FieldTable, Mirror, Order, Structure,
};

/// What B found in one unit.
#[derive(Debug, Default)]
pub struct Walked {
    /// Every token, with every place it occurs.
    pub tokens: BTreeMap<Token, Vec<Origin>>,
    /// Findings: (path, kind, occurrence label). Labels only count rows.
    pub rows: Vec<(String, Kind, String)>,
    records: Vec<RecordLinks>,
    ids: HashMap<String, Vec<usize>>,
}

#[derive(Debug, Default)]
struct RecordLinks {
    id: Option<String>,
    parent: Option<String>,
    /// A `Mirror::Children` drop: its path and the ids it lists.
    children: Option<(String, Vec<String>)>,
}

impl Walked {
    /// Whether the source places `a` strictly before `b`: in the same
    /// record at an earlier array position, or in an ancestor record.
    pub fn precedes(&self, a: &Origin, b: &Origin) -> bool {
        let (Some(ra), Some(rb)) = (a.record, b.record) else {
            return false;
        };
        if ra == rb {
            return a
                .index
                .iter()
                .zip(&b.index)
                .find(|(x, y)| x != y)
                .is_some_and(|(x, y)| x < y);
        }
        self.is_ancestor(ra, rb)
    }

    fn is_ancestor(&self, ancestor: usize, of: usize) -> bool {
        let mut seen = BTreeSet::new();
        let mut frontier = vec![of];
        while let Some(record) = frontier.pop() {
            if !seen.insert(record) {
                continue;
            }
            let Some(parent) = &self.records[record].parent else {
                continue;
            };
            for &up in self.ids.get(parent).into_iter().flatten() {
                if up == ancestor {
                    return true;
                }
                frontier.push(up);
            }
        }
        false
    }
}

/// Walk one unit's bytes under `table`.
pub fn walk(table: &FieldTable, source: &[u8]) -> Walked {
    let mut walker = Walker {
        table,
        out: Walked::default(),
    };
    if table.jsonl {
        for (line, bytes) in source.split(|byte| *byte == b'\n').enumerate() {
            if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            walker.document(bytes, line + 1);
        }
    } else {
        walker.document(source, 0);
    }
    walker.finish();
    walker.out
}

// ---------------------------------------------------------------------------
// The parser.

#[derive(Clone, Debug)]
enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    /// Members in name order; names are unique.
    Obj(BTreeMap<String, J>),
}

const MAX_DEPTH: usize = 512;

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn document(bytes: &'a [u8]) -> Result<J, String> {
        let mut parser = Parser {
            bytes,
            at: 0,
            depth: 0,
        };
        let value = parser.value()?;
        parser.whitespace();
        if parser.at != bytes.len() {
            return Err(format!("trailing bytes at {}", parser.at));
        }
        Ok(value)
    }

    fn whitespace(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected {:?} at {}", byte as char, self.at))
        }
    }

    fn value(&mut self) -> Result<J, String> {
        self.whitespace();
        match self.peek() {
            Some(b'{') => self.nested(Self::object),
            Some(b'[') => self.nested(Self::array),
            Some(b'"') => self.string().map(J::Str),
            Some(b't') => self.literal(b"true", J::Bool(true)),
            Some(b'f') => self.literal(b"false", J::Bool(false)),
            Some(b'n') => self.literal(b"null", J::Null),
            Some(b'-' | b'0'..=b'9') => self.number().map(J::Num),
            _ => Err(format!("expected a value at {}", self.at)),
        }
    }

    fn nested(&mut self, parse: fn(&mut Self) -> Result<J, String>) -> Result<J, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("nesting too deep".to_owned());
        }
        let value = parse(self);
        self.depth -= 1;
        value
    }

    fn literal(&mut self, word: &[u8], value: J) -> Result<J, String> {
        if self.bytes[self.at..].starts_with(word) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(format!("bad literal at {}", self.at))
        }
    }

    fn object(&mut self) -> Result<J, String> {
        self.expect(b'{')?;
        let mut members = BTreeMap::new();
        self.whitespace();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(J::Obj(members));
        }
        loop {
            self.whitespace();
            let name = self.string()?;
            self.whitespace();
            self.expect(b':')?;
            let value = self.value()?;
            if members.insert(name, value).is_some() {
                return Err(format!("duplicate member name before {}", self.at));
            }
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(J::Obj(members));
                }
                _ => return Err(format!("expected , or }} at {}", self.at)),
            }
        }
    }

    fn array(&mut self) -> Result<J, String> {
        self.expect(b'[')?;
        let mut elements = Vec::new();
        self.whitespace();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(J::Arr(elements));
        }
        loop {
            elements.push(self.value()?);
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(J::Arr(elements));
                }
                _ => return Err(format!("expected , or ] at {}", self.at)),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| "short \\u escape".to_owned())?;
        let text = std::str::from_utf8(digits).map_err(|_| "bad \\u escape".to_owned())?;
        if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("bad \\u escape".to_owned());
        }
        self.at += 4;
        Ok(u32::from_str_radix(text, 16).expect("four hex digits"))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err("unterminated string".to_owned());
            };
            self.at += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(escape) = self.peek() else {
                        return Err("unterminated escape".to_owned());
                    };
                    self.at += 1;
                    let decoded = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let high = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&high) {
                                if !self.bytes[self.at..].starts_with(b"\\u") {
                                    return Err("lone high surrogate".to_owned());
                                }
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err("bad low surrogate".to_owned());
                                }
                                0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&high) {
                                return Err("lone low surrogate".to_owned());
                            } else {
                                high
                            };
                            char::from_u32(code).ok_or_else(|| "bad code point".to_owned())?
                        }
                        _ => return Err("bad escape".to_owned()),
                    };
                    let mut buffer = [0u8; 4];
                    out.extend_from_slice(decoded.encode_utf8(&mut buffer).as_bytes());
                }
                0x00..=0x1F => return Err("control character in string".to_owned()),
                _ => out.push(byte),
            }
        }
        String::from_utf8(out).map_err(|_| "invalid UTF-8 in string".to_owned())
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<String, String> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err("bad number".to_owned()),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if self.digits() == 0 {
                return Err("bad fraction".to_owned());
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err("bad exponent".to_owned());
            }
        }
        Ok(String::from_utf8(self.bytes[start..self.at].to_vec()).expect("ASCII number"))
    }
}

// ---------------------------------------------------------------------------
// The walk.

struct Walker<'t> {
    table: &'t FieldTable,
    out: Walked,
}

/// Where the walk is: the record, its scalar fields, array indices within
/// it, and the line (for occurrence labels).
#[derive(Clone)]
struct Ctx {
    record: Option<usize>,
    scalars: std::rc::Rc<BTreeMap<String, String>>,
    index: Vec<usize>,
    line: usize,
}

/// A scalar's spelling, as conditions and vocabularies compare it.
fn spelling(value: &J) -> Option<String> {
    match value {
        J::Null => Some("null".to_owned()),
        J::Bool(b) => Some(b.to_string()),
        J::Num(token) => Some(token.clone()),
        J::Str(text) => Some(text.clone()),
        J::Arr(_) | J::Obj(_) => None,
    }
}

fn is_scalar(value: &J) -> bool {
    !matches!(value, J::Arr(_) | J::Obj(_))
}

fn is_empty(value: &J) -> bool {
    match value {
        J::Null => true,
        J::Str(text) => text.is_empty(),
        J::Arr(elements) => elements.is_empty(),
        J::Obj(members) => members.is_empty(),
        J::Bool(_) | J::Num(_) => false,
    }
}

/// Whether a value with nothing to look inside is a leaf.
fn is_leaf(value: &J) -> bool {
    is_scalar(value) || is_empty(value)
}

/// The record's scalar fields by record-relative key path. Arrays are not
/// entered: a condition names one field.
fn scalars(value: &J, prefix: &str, out: &mut BTreeMap<String, String>) {
    if let J::Obj(members) = value {
        for (name, member) in members {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            match member {
                J::Obj(_) => scalars(member, &path, out),
                J::Arr(_) => {}
                scalar => {
                    out.insert(path, spelling(scalar).expect("scalar"));
                }
            }
        }
    }
}

fn holds(when: &[Cond], scalars: &BTreeMap<String, String>) -> bool {
    when.iter().all(|cond| match cond {
        Cond::Is(field, value) => scalars.get(*field).is_some_and(|v| v == value),
        Cond::IsNot(field, value) => scalars.get(*field).is_none_or(|v| v != value),
    })
}

/// Whether an entry's class can take this value: a scalar decode or a
/// structure takes only a leaf; anything else takes any value.
fn applies(entry: &Entry, value: &J) -> bool {
    match entry.class {
        Class::Fact { decode, .. } => decode_applies(decode, value),
        Class::Annotation(list) => list
            .iter()
            .all(|(_, decode)| decode_applies(*decode, value)),
        Class::Structure(_) => is_leaf(value),
        Class::Correlator | Class::Drop(_) | Class::Open(_) => true,
    }
}

fn decode_applies(decode: Decode, value: &J) -> bool {
    matches!(decode, Decode::Json) || is_leaf(value)
}

/// A name safe to print in a path: field names and vocabulary, never
/// content. Anything else prints as a placeholder.
fn printable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:$@".contains(&b))
}

enum Failure {
    Decode,
    UnknownValue,
}

impl<'t> Walker<'t> {
    fn row(&mut self, path: &str, kind: Kind, ctx: &Ctx) {
        self.out.rows.push((
            path.to_owned(),
            kind,
            format!("{}:{:?}", ctx.line, ctx.index),
        ));
    }

    fn document(&mut self, bytes: &[u8], line: usize) {
        let ctx = Ctx {
            record: None,
            scalars: Default::default(),
            index: Vec::new(),
            line,
        };
        match Parser::document(bytes) {
            Ok(value) => self.value(&value, "", None, ctx),
            Err(_) => self.row("<document>", Kind::Decode, &ctx),
        }
    }

    fn discriminator(&self, value: &J) -> Option<String> {
        let J::Obj(members) = value else { return None };
        self.table
            .discriminators
            .iter()
            .find_map(|key| match members.get(*key) {
                Some(J::Str(shape)) if printable(shape) => Some(shape.clone()),
                Some(J::Str(_)) => Some("?".to_owned()),
                _ => None,
            })
    }

    fn value(&mut self, value: &J, path: &str, parent: Option<&J>, mut ctx: Ctx) {
        if path == self.table.record && ctx.record.is_none() {
            ctx = self.start_record(value, ctx);
        }
        let shape = self.discriminator(value);
        let shaped = shape.as_ref().map(|shape| format!("{path}{{{shape}}}"));

        let mut entries: Vec<&Entry> = shaped
            .as_deref()
            .map(|shaped| self.table.at(shaped).collect())
            .unwrap_or_default();
        if entries.is_empty() {
            entries = self.table.at(path).collect();
        }
        entries.retain(|entry| applies(entry, value) && holds(entry.when, &ctx.scalars));

        match entries.as_slice() {
            [entry] => {
                let entry = *entry;
                self.apply(entry, value, path, parent, &ctx);
                return;
            }
            [] => {}
            _ => {
                self.row(path, Kind::Table, &ctx);
                return;
            }
        }
        if is_leaf(value) {
            self.row(path, Kind::Unknown, &ctx);
            return;
        }
        let inner = shaped.unwrap_or_else(|| path.to_owned());
        match value {
            J::Obj(members) => {
                let id_keyed = self.table.id_keyed.contains(&path);
                for (name, member) in members {
                    let child = if id_keyed {
                        format!("{inner}.<id>")
                    } else if printable(name) {
                        format!("{inner}.{name}")
                    } else {
                        format!("{inner}.<key>")
                    };
                    self.value(member, &child, Some(value), ctx.clone());
                }
            }
            J::Arr(elements) => {
                let child = format!("{inner}[]");
                for (at, element) in elements.iter().enumerate() {
                    let mut ctx = ctx.clone();
                    ctx.index.push(at);
                    self.value(element, &child, Some(value), ctx);
                }
            }
            _ => unreachable!("leaves are handled above"),
        }
    }

    fn start_record(&mut self, value: &J, mut ctx: Ctx) -> Ctx {
        let mut fields = BTreeMap::new();
        scalars(value, "", &mut fields);
        let Order::Parents { id, parent } = self.table.order;
        let field = |name: &str| fields.get(name).filter(|v| *v != "null").cloned();
        let links = RecordLinks {
            id: field(id),
            parent: field(parent),
            children: None,
        };
        let record = self.out.records.len();
        if let Some(id) = &links.id {
            self.out.ids.entry(id.clone()).or_default().push(record);
        }
        self.out.records.push(links);
        ctx.record = Some(record);
        ctx.scalars = std::rc::Rc::new(fields);
        ctx.index = Vec::new();
        ctx
    }

    fn emit(&mut self, token: Token, path: &str, ctx: &Ctx) {
        self.out.tokens.entry(token).or_default().push(Origin {
            path: path.to_owned(),
            record: ctx.record,
            index: ctx.index.clone(),
        });
    }

    fn apply(&mut self, entry: &Entry, value: &J, path: &str, parent: Option<&J>, ctx: &Ctx) {
        match entry.class {
            Class::Fact {
                modality,
                direction,
                attribute,
                decode,
            } => match decode_values(decode, value) {
                Ok(values) => {
                    for value in values {
                        let token = Token {
                            modality: modality.to_owned(),
                            direction: direction.to_owned(),
                            attribute: attribute.to_owned(),
                            value,
                        };
                        self.emit(token, path, ctx);
                    }
                }
                Err(failure) => self.fail(failure, path, ctx),
            },
            Class::Annotation(list) => {
                for (attribute, decode) in list {
                    match decode_values(*decode, value) {
                        Ok(values) => {
                            for value in values {
                                let token = Token {
                                    modality: String::new(),
                                    direction: String::new(),
                                    attribute: (*attribute).to_owned(),
                                    value,
                                };
                                self.emit(token, path, ctx);
                            }
                        }
                        Err(failure) => self.fail(failure, path, ctx),
                    }
                }
            }
            Class::Structure(structure) => match structure {
                Structure::Discriminator => {
                    if !matches!(value, J::Str(_)) {
                        self.row(path, Kind::Decode, ctx);
                    }
                }
                Structure::Vocabulary(terms) => {
                    if !spelling(value).is_some_and(|v| terms.contains(&v.as_str())) {
                        self.row(path, Kind::UnknownValue, ctx);
                    }
                }
                Structure::Order => {
                    if !matches!(value, J::Str(_) | J::Null) {
                        self.row(path, Kind::Decode, ctx);
                    }
                }
                Structure::Shape(_) => {}
            },
            Class::Correlator => {}
            Class::Drop(reason) => match reason {
                DropReason::Machinery(_) | DropReason::ProviderId => {}
                DropReason::Empty => {
                    if !is_empty(value) {
                        self.row(path, Kind::Empty, ctx);
                    }
                }
                DropReason::Mirror(Mirror::Sibling(name)) => {
                    let mirrored = match parent {
                        Some(J::Obj(members)) => members.get(name),
                        _ => None,
                    };
                    if !mirrored.is_some_and(|mirrored| same(mirrored, value)) {
                        self.row(path, Kind::Mirror, ctx);
                    }
                }
                DropReason::Mirror(Mirror::Children) => {
                    let ids: Option<Vec<String>> = match value {
                        J::Arr(elements) => elements
                            .iter()
                            .map(|element| match element {
                                J::Str(id) => Some(id.clone()),
                                _ => None,
                            })
                            .collect(),
                        _ => None,
                    };
                    match (ids, ctx.record) {
                        (Some(ids), Some(record)) => {
                            self.out.records[record].children = Some((path.to_owned(), ids));
                        }
                        _ => self.row(path, Kind::Decode, ctx),
                    }
                }
            },
            Class::Open(_) => self.row(path, Kind::Open, ctx),
        }
    }

    fn fail(&mut self, failure: Failure, path: &str, ctx: &Ctx) {
        let kind = match failure {
            Failure::Decode => Kind::Decode,
            Failure::UnknownValue => Kind::UnknownValue,
        };
        self.row(path, kind, ctx);
    }

    /// Check the children mirrors against the parent links.
    fn finish(&mut self) {
        let mut rows = Vec::new();
        for (at, record) in self.out.records.iter().enumerate() {
            let Some((path, listed)) = &record.children else {
                continue;
            };
            let listed: BTreeSet<&str> = listed.iter().map(String::as_str).collect();
            let linked: BTreeSet<&str> = self
                .out
                .records
                .iter()
                .filter(|child| child.parent.is_some() && child.parent == record.id)
                .filter_map(|child| child.id.as_deref())
                .collect();
            if listed != linked {
                rows.push((path.clone(), Kind::Mirror, format!("record {at}")));
            }
        }
        self.out.rows.extend(rows);
    }
}

/// Order-free equality of two JSON values; numbers by token.
fn same(a: &J, b: &J) -> bool {
    match (a, b) {
        (J::Null, J::Null) => true,
        (J::Bool(x), J::Bool(y)) => x == y,
        (J::Num(x), J::Num(y)) | (J::Str(x), J::Str(y)) => x == y,
        (J::Arr(x), J::Arr(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
        (J::Obj(x), J::Obj(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((kx, vx), (ky, vy))| kx == ky && same(vx, vy))
        }
        _ => false,
    }
}

fn decode_values(decode: Decode, value: &J) -> Result<Vec<Value>, Failure> {
    let one = |value: Value| Ok(vec![value]);
    match (decode, value) {
        (Decode::Text, J::Str(text)) => one(Value::Text(text.clone())),
        (Decode::Base64, J::Str(text)) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(text)
                .map_err(|_| Failure::Decode)?;
            one(Value::Bytes(blake3::hash(&bytes).to_hex().to_string()))
        }
        (Decode::Number, J::Num(token)) => one(Value::Number(token.clone())),
        (Decode::Bool, J::Bool(b)) => one(Value::Bool(*b)),
        (Decode::Json, value) => {
            let mut leaves = Vec::new();
            json_leaves(value, &mut String::new(), &mut leaves);
            Ok(leaves)
        }
        (Decode::MediaType, J::Str(text)) => one(Value::Text(
            canonical_media_type(text).ok_or(Failure::Decode)?,
        )),
        (Decode::Timestamp, J::Str(text)) => {
            let epoch = Epoch::from_str(text.trim()).map_err(|_| Failure::Decode)?;
            one(Value::Time(epoch.to_tai_duration().total_nanoseconds()))
        }
        (Decode::UnixSeconds, J::Num(token)) => {
            let nanos = exact_nanoseconds(token).ok_or(Failure::Decode)?;
            let epoch = Epoch::from_unix_duration(Duration::from_total_nanoseconds(nanos));
            one(Value::Time(epoch.to_tai_duration().total_nanoseconds()))
        }
        (Decode::Term(map), value) => {
            let spelled = spelling(value).ok_or(Failure::Decode)?;
            match map.iter().find(|(source, _)| *source == spelled) {
                Some((_, term)) => one(Value::Term((*term).to_owned())),
                None => Err(Failure::UnknownValue),
            }
        }
        _ => Err(Failure::Decode),
    }
}

/// Every leaf of a JSON value under its JSON Pointer.
fn json_leaves(value: &J, pointer: &mut String, out: &mut Vec<Value>) {
    let leaf = match value {
        J::Null => JsonLeaf::Null,
        J::Bool(b) => JsonLeaf::Bool(*b),
        J::Num(token) => JsonLeaf::Number(token.clone()),
        J::Str(text) => JsonLeaf::String(text.clone()),
        J::Arr(elements) if elements.is_empty() => JsonLeaf::EmptyArray,
        J::Obj(members) if members.is_empty() => JsonLeaf::EmptyObject,
        J::Arr(elements) => {
            for (at, element) in elements.iter().enumerate() {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&at.to_string());
                json_leaves(element, pointer, out);
                pointer.truncate(len);
            }
            return;
        }
        J::Obj(members) => {
            for (name, member) in members {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&name.replace('~', "~0").replace('/', "~1"));
                json_leaves(member, pointer, out);
                pointer.truncate(len);
            }
            return;
        }
    };
    out.push(Value::Json(pointer.clone(), leaf));
}

fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn is_token(text: &str) -> bool {
    !text.is_empty() && text.chars().all(is_tchar)
}

/// The canonical spelling of a media type: type, subtype and parameter
/// names lowercased, parameters sorted by name, values exact, a value
/// quoted only when it is not a token. A malformed spelling or a repeated
/// parameter is `None`.
fn canonical_media_type(text: &str) -> Option<String> {
    let mut rest = text.trim_matches(|c| c == ' ' || c == '\t');
    let end = rest.find(';').unwrap_or(rest.len());
    let essence = rest[..end].trim_end_matches([' ', '\t']);
    rest = &rest[end..];
    let (kind, sub) = essence.split_once('/')?;
    if !is_token(kind) || !is_token(sub) {
        return None;
    }
    let mut parameters: BTreeMap<String, String> = BTreeMap::new();
    while let Some(after) = rest.strip_prefix(';') {
        let after = after.trim_start_matches([' ', '\t']);
        let eq = after.find('=')?;
        let name = &after[..eq];
        if !is_token(name) {
            return None;
        }
        let mut value_text = &after[eq + 1..];
        let value = if let Some(quoted) = value_text.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut close = None;
            while let Some((at, c)) = chars.next() {
                match c {
                    '"' => {
                        close = Some(at);
                        break;
                    }
                    '\\' => value.push(chars.next()?.1),
                    c => value.push(c),
                }
            }
            value_text = &quoted[close? + 1..];
            value
        } else {
            let end = value_text.find(';').unwrap_or(value_text.len());
            let value = value_text[..end].trim_end_matches([' ', '\t']);
            if !is_token(value) {
                return None;
            }
            value_text = &value_text[end..];
            value.to_owned()
        };
        rest = value_text.trim_start_matches([' ', '\t']);
        if parameters
            .insert(name.to_ascii_lowercase(), value)
            .is_some()
        {
            return None;
        }
    }
    if !rest.is_empty() {
        return None;
    }
    let mut out = format!("{}/{}", kind.to_ascii_lowercase(), sub.to_ascii_lowercase());
    for (name, value) in parameters {
        out.push(';');
        out.push_str(&name);
        out.push('=');
        if is_token(&value) {
            out.push_str(&value);
        } else {
            out.push('"');
            for c in value.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        }
    }
    Some(out)
}

/// An exact decimal number of seconds as nanoseconds, or `None` when it is
/// not a whole number of nanoseconds or does not fit.
fn exact_nanoseconds(token: &str) -> Option<i128> {
    let (negative, unsigned) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], unsigned[at + 1..].parse::<i32>().ok()?),
        None => (unsigned, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    // value = digits * 10^(exponent - fraction.len()); nanoseconds add 9.
    let shift = exponent - fraction.len() as i32 + 9;
    let mut digits = digits.trim_start_matches('0').to_owned();
    if digits.is_empty() {
        return Some(0);
    }
    if shift >= 0 {
        digits.push_str(&"0".repeat(shift as usize));
    } else {
        let cut = digits.len().checked_sub((-shift) as usize)?;
        if !digits[cut..].bytes().all(|b| b == b'0') {
            return None;
        }
        digits.truncate(cut);
        if digits.is_empty() {
            return Some(0);
        }
    }
    let magnitude: i128 = digits.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
pub(super) mod probe {
    //! Test access to the walker's internals.
    pub fn canonical_media_type(text: &str) -> Option<String> {
        super::canonical_media_type(text)
    }
    pub fn exact_nanoseconds(token: &str) -> Option<i128> {
        super::exact_nanoseconds(token)
    }
}
