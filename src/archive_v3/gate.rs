//! The gate: does an importer's output hold exactly what the source held?
//!
//! Two programs that share no code read one source unit:
//!
//! - **B** ([`walk`]) is an independent tokenizer over the source bytes. It
//!   has its own minimal JSON/JSONL parser and reads the importer's
//!   [`FieldTable`] as data. It emits one [`Token`] per source value the
//!   table classes as a fact or an annotation, leaves out what the table
//!   drops or correlates, asserts every mirror and empty drop, and fails any
//!   path, discriminator or vocabulary value the table does not name.
//! - **A** ([`render`]) reads only the `TribleSet` an importer produced. It
//!   renders the window (`thread_to`) of every witnessed leaf and of every
//!   `out` fact's predecessor as tokens: each fact's values with its
//!   modality and direction, and the annotations of the nodes and witnesses
//!   on the window. It renders every attribute it finds; one it does not
//!   know becomes a token of its own, so nothing hides.
//!
//! The two meet only in the [`Token`] type and in the term names of the v3
//! vocabulary, which B reads from the table and A from the schema.
//!
//! [`check`] passes a unit when:
//!
//! 1. every token B found is in a window (else *missing*, or *changed* when
//!    the same attribute and value sit in a window under another modality or
//!    direction);
//! 2. every token A rendered is, exactly, a token B found (else *invented*:
//!    the same value under another modality or direction is invented too);
//! 3. every witnessed window holds exactly the occurrences of the source
//!    window behind its record, each as often as the source holds it and in
//!    its order (else *count* or *order*), and every source window is held
//!    by a witnessed window (else *unheld*);
//! 4. every entity A was given is on a window, or annotates one (else
//!    *unreached*);
//! 5. B found nothing the table does not name, and every drop held.
//!
//! Points 1 and 2 compare sets over the unit: copies of one item converge on
//! one fact. Point 3 is what counts them: it compares per witness, never by
//! collapsing the unit, so a repeated identical occurrence the importer lost
//! is a missing occurrence there. A window is compared as the sequence of
//! its items (the fact tokens of one node, the fact tokens of one source
//! occurrence); `json_tree` objects compare order-free, because a structure
//! token names its leaf by key path. A gap fact (`absent`) is structure: it
//! holds no value, and a node that holds no value is no item.
//!
//! The [`Report`] never holds a value: only format, path, kind and count, so
//! the gate can run over real transcripts and print its result. Path
//! segments that look like ids print as `<id>` ([`id_like`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::*;

use super::fields::FieldTable;

pub mod render;
#[cfg(test)]
pub(crate) mod tests;
pub mod walk;

/// One value as she experienced it, or one annotation of it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Token {
    /// Modality term name; empty for an annotation.
    pub modality: String,
    /// `in` or `out`; empty for an annotation.
    pub direction: String,
    /// Fact or annotation attribute name.
    pub attribute: String,
    pub value: Value,
}

/// A decoded value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    Text(String),
    /// A number, by its exact token.
    Number(String),
    Bool(bool),
    /// Bytes, by their BLAKE3 hash in hex.
    Bytes(String),
    /// A vocabulary term, by name.
    Term(String),
    /// A time, in TAI nanoseconds.
    Time(i128),
    /// A time interval in TAI nanoseconds, when its ends differ.
    Interval(i128, i128),
    /// One leaf of a JSON document, by its JSON Pointer (RFC 6901).
    Json(String, JsonLeaf),
    /// A value of an attribute A does not know, by its raw hex.
    Raw(String),
}

/// A JSON leaf; an empty container is a leaf of its own.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JsonLeaf {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    EmptyArray,
    EmptyObject,
}

/// What a report row says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A source value is in no window.
    Missing,
    /// A window value is in no source.
    Invented,
    /// A source value is in a window under another modality or direction.
    Changed,
    /// A witnessed window holds the occurrences of the source window behind
    /// its record, in another order.
    Order,
    /// A witnessed window holds an occurrence more or fewer times than the
    /// source window behind its record, or a node that is no source item.
    Count,
    /// A source window that no witnessed window holds.
    Unheld,
    /// An entity A was given that no window reaches.
    Unreached,
    /// The table names no class for a path (or for its discriminator).
    Unknown,
    /// A vocabulary value the table does not name.
    UnknownValue,
    /// A mirror drop differs from what it mirrors.
    Mirror,
    /// An empty drop is not empty.
    Empty,
    /// A field whose meaning is still open.
    Open,
    /// A value does not decode as its entry says, or the source is not JSON.
    Decode,
    /// Several entries apply to one value.
    Table,
    /// A node on a window has several predecessors.
    Ambiguous,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Kind::Missing => "missing",
            Kind::Invented => "invented",
            Kind::Changed => "changed",
            Kind::Order => "order",
            Kind::Count => "count",
            Kind::Unheld => "unheld",
            Kind::Unreached => "unreached",
            Kind::Unknown => "unknown",
            Kind::UnknownValue => "unknown-value",
            Kind::Mirror => "mirror",
            Kind::Empty => "empty",
            Kind::Open => "open",
            Kind::Decode => "decode",
            Kind::Table => "table",
            Kind::Ambiguous => "ambiguous",
        };
        f.write_str(name)
    }
}

/// One report row: a path, what is wrong there, and how many distinct
/// tokens (or occurrences, for walk rows) are affected. Never a value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Row {
    pub format: &'static str,
    /// A normalized source path for B's findings; `window:<modality>/
    /// <direction>/<attribute>` for A's.
    pub path: String,
    pub kind: Kind,
    pub count: usize,
}

/// The gate's verdict on one unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub rows: Vec<Row>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows of one kind at one path.
    pub fn count(&self, path: &str, kind: Kind) -> usize {
        self.rows
            .iter()
            .filter(|row| row.path == path && row.kind == kind)
            .map(|row| row.count)
            .sum()
    }

    /// Whether any row names `path`.
    pub fn names(&self, path: &str) -> bool {
        self.rows.iter().any(|row| row.path == path)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.rows.is_empty() {
            return writeln!(f, "pass");
        }
        for row in &self.rows {
            writeln!(
                f,
                "{}\t{}\t{}\t{}",
                row.format, row.path, row.kind, row.count
            )?;
        }
        Ok(())
    }
}

/// Where B found a token: the source path, the record it sits in and its
/// array indices within that record.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin {
    pub path: String,
    pub record: Option<usize>,
    pub index: Vec<usize>,
}

/// Gate one source unit against what an importer made of it.
///
/// `source` is the unit's bytes (a JSONL file, or a JSON document holding
/// one unit); `facts` and `blobs` are the importer's output for that unit
/// alone.
pub fn check<B: BlobStoreGet>(
    table: &FieldTable,
    source: &[u8],
    facts: &TribleSet,
    blobs: &B,
) -> Report {
    let walked = walk::walk(table, source);
    let rendered = render::render(facts, blobs);
    compare(table, &walked, &rendered)
}

/// Whether a path segment looks like an id rather than a field name: a
/// number, a uuid or other hex key of eight digits or more, a prefixed
/// provider id (`toolu_01…`), or a long mixed run of letters and digits.
pub fn id_like(segment: &str) -> bool {
    let digit = |s: &str| s.bytes().any(|b| b.is_ascii_digit());
    let letter = |s: &str| s.bytes().any(|b| b.is_ascii_alphabetic());
    let alnum = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric());
    if !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    let hex: String = segment.chars().filter(|c| *c != '-').collect();
    if hex.len() >= 8 && hex.bytes().all(|b| b.is_ascii_hexdigit()) && digit(&hex) {
        return true;
    }
    if let Some((_, rest)) = segment.split_once(['_', '-']) {
        if rest.len() >= 12 && alnum(rest) && digit(rest) && letter(rest) {
            return true;
        }
    }
    segment.len() >= 20 && alnum(segment) && digit(segment)
}

/// A path with every id-like segment as `<id>`. Segments are what lies
/// between `.`, `/`, `:`, `{`, `}`, `[`, `]`, `<` and `>`.
pub fn normalize(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut segment = String::new();
    let flush = |segment: &mut String, out: &mut String| {
        if id_like(segment) {
            out.push_str("<id>");
        } else {
            out.push_str(segment);
        }
        segment.clear();
    };
    for c in path.chars() {
        if ".:/{}[]<>".contains(c) {
            flush(&mut segment, &mut out);
            out.push(c);
        } else {
            segment.push(c);
        }
    }
    flush(&mut segment, &mut out);
    out
}

type Hash = [u8; 32];

/// The window hash of `before` extended by one item.
fn step(before: &Hash, item: u32) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(before);
    hasher.update(&item.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// Compare B's walk with A's rendering.
pub fn compare(table: &FieldTable, walked: &walk::Walked, rendered: &render::Rendered) -> Report {
    let mut rows: BTreeMap<(String, Kind), BTreeSet<String>> = BTreeMap::new();
    let mut note = |path: String, kind: Kind, what: String| {
        rows.entry((normalize(&path), kind))
            .or_default()
            .insert(what);
    };

    for (path, kind, at) in &walked.rows {
        note(path.clone(), *kind, at.clone());
    }
    for (path, kind, at) in &rendered.rows {
        note(path.clone(), *kind, at.clone());
    }

    let rendered_tokens: BTreeSet<&Token> = rendered
        .graph
        .iter()
        .flat_map(|node| node.tokens.iter())
        .collect();
    let shapes = |tokens: &mut dyn Iterator<Item = &Token>| {
        let mut shapes: BTreeMap<(String, Value), BTreeSet<(String, String)>> = BTreeMap::new();
        for token in tokens {
            shapes
                .entry((token.attribute.clone(), token.value.clone()))
                .or_default()
                .insert((token.modality.clone(), token.direction.clone()));
        }
        shapes
    };
    let rendered_shapes = shapes(&mut rendered_tokens.iter().copied());

    // A token's identity in a row is its position in the ordered set; the
    // row only counts them.
    for (ordinal, (token, origins)) in walked.tokens.iter().enumerate() {
        if rendered_tokens.contains(token) {
            continue;
        }
        let kind = if rendered_shapes.contains_key(&(token.attribute.clone(), token.value.clone()))
        {
            Kind::Changed
        } else {
            Kind::Missing
        };
        for origin in origins {
            note(origin.path.clone(), kind, ordinal.to_string());
        }
    }
    // Invented: exactly this token is in no source. The same attribute and
    // value under another modality or direction only labels B's missing
    // token as changed; it never excuses A's.
    for (ordinal, token) in rendered_tokens.iter().enumerate() {
        if walked.tokens.contains_key(*token) {
            continue;
        }
        note(window_path(token), Kind::Invented, ordinal.to_string());
    }

    for (path, kind, what) in occurrences(walked, rendered) {
        note(path, kind, what);
    }

    Report {
        rows: rows
            .into_iter()
            .map(|((path, kind), what)| Row {
                format: table.format,
                path,
                kind,
                count: what.len(),
            })
            .collect(),
    }
}

/// Per witness: every witnessed window against the source window behind
/// its record, and every source window against the witnessed windows.
fn occurrences(walked: &walk::Walked, rendered: &render::Rendered) -> Vec<(String, Kind, String)> {
    let mut rows = Vec::new();
    let facts: Vec<BTreeSet<Token>> = rendered
        .graph
        .iter()
        .map(|node| {
            node.tokens
                .iter()
                .filter(|token| !token.modality.is_empty())
                .cloned()
                .collect()
        })
        .collect();
    let mut ids: BTreeMap<&BTreeSet<Token>, u32> = BTreeMap::new();
    let mut intern = |tokens| {
        let next = ids.len() as u32;
        *ids.entry(tokens).or_insert(next)
    };
    let item_ids: Vec<u32> = walked
        .items
        .iter()
        .map(|item| intern(&item.tokens))
        .collect();
    let node_ids: Vec<Option<u32>> = facts
        .iter()
        .map(|tokens| (!tokens.is_empty()).then(|| intern(tokens)))
        .collect();

    // A's window hashes, root first; a node is placed after its previous.
    let mut node_hash: Vec<Option<Hash>> = vec![None; rendered.graph.len()];
    let mut climbing = vec![false; rendered.graph.len()];
    for start in 0..rendered.graph.len() {
        let mut stack = Vec::new();
        let mut cursor = Some(start);
        while let Some(at) = cursor {
            if node_hash[at].is_some() || climbing[at] {
                break;
            }
            climbing[at] = true;
            stack.push(at);
            cursor = rendered.graph[at].previous;
        }
        for at in stack.into_iter().rev() {
            let before = rendered.graph[at]
                .previous
                .and_then(|previous| node_hash[previous])
                .unwrap_or([0; 32]);
            node_hash[at] = Some(match node_ids[at] {
                Some(id) => step(&before, id),
                None => before,
            });
        }
    }
    let held: HashSet<Hash> = rendered
        .graph
        .iter()
        .enumerate()
        .filter(|(_, node)| node.witnessed)
        .filter_map(|(at, _)| node_hash[at])
        .collect();

    // B's source windows: one per occurrence (record, position).
    struct Occurrence {
        record: usize,
        position: usize,
        item: usize,
        hash: Hash,
    }
    let mut occurrences: Vec<Occurrence> = Vec::new();
    let id = |item: usize| item_ids[item];
    let lines = walked.lines(&id, &step);
    for record in 0..walked.record_count() {
        if walked.is_repeat(record) || walked.record_items(record).is_empty() {
            continue;
        }
        let own = walked.ordered(record);
        let Some(mut hash) = lines[record].and_then(|lines| lines.before) else {
            rows.push((
                walked.items[own[0]].path.clone(),
                Kind::Count,
                format!("window {record}"),
            ));
            continue;
        };
        for (position, &item) in own.iter().enumerate() {
            hash = step(&hash, item_ids[item]);
            occurrences.push(Occurrence {
                record,
                position,
                item,
                hash,
            });
        }
    }
    let sources: HashSet<Hash> = occurrences.iter().map(|o| o.hash).collect();

    // Each witnessed window. A window whose nearest witnessed predecessor
    // already failed is not reported again.
    let mut passed: Vec<Option<bool>> = vec![None; rendered.graph.len()];
    for (at, node) in rendered.graph.iter().enumerate() {
        if node.witnessed {
            passed[at] = Some(node_hash[at].is_some_and(|hash| sources.contains(&hash)));
        }
    }
    let multiset = |mut ids: Vec<u32>| {
        ids.sort_unstable();
        ids
    };
    for (at, node) in rendered.graph.iter().enumerate() {
        if passed[at] != Some(false) {
            continue;
        }
        let mut cursor = node.previous;
        let mut inherited = false;
        while let Some(up) = cursor {
            if let Some(ok) = passed[up] {
                inherited = !ok;
                break;
            }
            cursor = rendered.graph[up].previous;
        }
        if inherited {
            continue;
        }
        let mut window = Vec::new();
        let mut cursor = Some(at);
        while let Some(up) = cursor {
            window.extend(node_ids[up]);
            cursor = rendered.graph[up].previous;
        }
        // An order fault: the window holds what the source holds before a
        // record, and as many of that record's own items as it should, in
        // another order.
        let window = multiset(window);
        let reordered = node_ids[at].is_some_and(|id| {
            occurrences
                .iter()
                .filter(|o| item_ids[o.item] == id)
                .take(64)
                .any(|o| {
                    let Some((before, own)) = walked.window(o.record) else {
                        return false;
                    };
                    let mut rest = window.clone();
                    for &item in &before {
                        match rest.binary_search(&item_ids[item]) {
                            Ok(found) => {
                                rest.remove(found);
                            }
                            Err(_) => return false,
                        }
                    }
                    let mut own = multiset(own.iter().map(|&item| item_ids[item]).collect());
                    rest.len() == o.position + 1
                        && rest.iter().all(|id| match own.binary_search(id) {
                            Ok(found) => {
                                own.remove(found);
                                true
                            }
                            Err(_) => false,
                        })
                })
        });
        let path = match facts[at].first() {
            Some(token) => walked
                .tokens
                .get(token)
                .and_then(|origins| origins.first())
                .map(|origin| origin.path.clone())
                .unwrap_or_else(|| window_path(token)),
            None => "window:empty".to_owned(),
        };
        let kind = if reordered { Kind::Order } else { Kind::Count };
        rows.push((path, kind, format!("node {at}")));
    }

    // Each source window. One whose previous occurrence is unheld too is
    // not reported again.
    let mut last_of: HashMap<usize, bool> = HashMap::new();
    let mut previous_held: Option<bool> = None;
    let mut previous_record = None;
    for occurrence in &occurrences {
        let is_held = held.contains(&occurrence.hash);
        let before = if occurrence.position > 0 && previous_record == Some(occurrence.record) {
            previous_held
        } else {
            let mut cursor = walked.parent(occurrence.record);
            let mut found = None;
            let mut seen = BTreeSet::new();
            while let Some(up) = cursor {
                if !seen.insert(up) {
                    break;
                }
                if let Some(&ok) = last_of.get(&up) {
                    found = Some(ok);
                    break;
                }
                cursor = walked.parent(up);
            }
            found
        };
        if !is_held && before != Some(false) {
            rows.push((
                walked.items[occurrence.item].path.clone(),
                Kind::Unheld,
                format!("occurrence {} {}", occurrence.record, occurrence.position),
            ));
        }
        previous_held = Some(is_held);
        previous_record = Some(occurrence.record);
        last_of.insert(occurrence.record, is_held);
    }
    rows
}

fn window_path(token: &Token) -> String {
    if token.modality.is_empty() && token.direction.is_empty() {
        format!("window:annotation/{}", token.attribute)
    } else {
        format!(
            "window:{}/{}/{}",
            token.modality, token.direction, token.attribute
        )
    }
}
