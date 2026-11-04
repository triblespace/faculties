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
//! 4. every node holds, in place, exactly the annotations, links and
//!    occurrences the source places at its window position (else *placed*),
//!    and every link the source's correlators establish names an item in the
//!    window of the item that links (else *link*);
//! 5. every entity A was given is on a window, or annotates one (else
//!    *unreached*);
//! 6. B found nothing the table does not name, and every drop held.
//!
//! Points 1 and 2 compare sets over the unit: copies of one item converge on
//! one fact. Point 3 is what counts them: it compares per witness, never by
//! collapsing the unit, so a repeated identical occurrence the importer lost
//! is a missing occurrence there. A window is compared as the sequence of
//! its items (the fact tokens of one node, the fact tokens of one source
//! occurrence); `json_tree` objects compare order-free, because a structure
//! token names its leaf by key path. Every fact renders its shape (modality
//! and direction), so every node is an item: a gap (`absent`) the rendering
//! rule puts in her window ([`crate::archive_v3::fields::Gaps`]), and a
//! result's head that holds no value, count like any other item.
//!
//! Point 4 compares per position (G6, G7): B places each annotation where
//! its table says ([`crate::archive_v3::fields::Place`]) and A renders each
//! node's own, so `ends` on the wrong node, a gloss moved or doubled, or
//! claims swapped between records fail even when every value is in the
//! source. A witness's occurrences compare as tuples, one per record. A link
//! is named by place, never by id: `responds_to` by how many items back its
//! call stands in the window, `recalls` and `continues` by the hash of the
//! window that ends at their target. B establishes them from the source:
//! a result's correlator ([`crate::archive_v3::fields::Link`]); an item a
//! record holds re-presented at a position other than its own recalls its
//! own (an item no record holds, a header or a gap, has no original and
//! recalls nothing); the first node of a compaction's line that is not on
//! the old line continues the old line's tail.
//!
//! The [`Report`] never holds a value: only format, path, kind and count, so
//! the gate can run over real transcripts and print its result. Path
//! segments that look like ids print as `<id>` ([`id_like`]); a member name
//! or discriminator value the table does not name prints as `<key>` or
//! `<value>` ([`vocabulary`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::*;

use super::fields::FieldTable;

#[cfg(test)]
mod adversarial;
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
    /// A place in a window, named by the hash of the window that ends there
    /// (a link's target that is not on the linking node's own window).
    Place(String),
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
    /// An annotation, link or claim sits on another node than the source
    /// places it, or the source places it nowhere.
    Placed,
    /// A link the source's correlator establishes names no item, or an
    /// item outside the window of the item that links to it.
    Link,
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
            Kind::Placed => "placed",
            Kind::Link => "link",
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

/// A path as a report or a real-run program prints it: every member name
/// and discriminator value the table does not name at that place prints as
/// `<key>` or `<value>`, every id-like segment as `<id>`, and an importer's
/// own placeholders (`<record>`, `{?}`) stay. An importer's failure path
/// holds source spellings; this is how a program over real files prints
/// one.
pub fn vocabulary(table: &FieldTable, path: &str) -> String {
    let known = walk::known_paths(table);
    let mut out = String::with_capacity(path.len());
    // The path as the table spells it so far.
    let mut spelled = String::new();
    let mut segment = String::new();
    let mut opened: Option<char> = None;
    for c in path.chars().chain(std::iter::once('\0')) {
        if c != '\0' && !".:/{}[]<>".contains(c) {
            segment.push(c);
            continue;
        }
        if !segment.is_empty() {
            // A shape is named with its closing brace.
            let closing = if opened == Some('{') && c == '}' {
                "}"
            } else {
                ""
            };
            let candidate = format!("{spelled}{segment}{closing}");
            let printed = if opened == Some('<') || segment == "?" {
                segment.clone()
            } else if id_like(&segment) {
                "<id>".to_owned()
            } else if known.contains(&candidate) {
                segment.clone()
            } else if opened == Some('{') {
                "<value>".to_owned()
            } else {
                "<key>".to_owned()
            };
            spelled.push_str(if opened == Some('<') {
                &segment
            } else {
                &printed
            });
            out.push_str(&printed);
            segment.clear();
        }
        match c {
            '\0' => break,
            '{' | '<' => opened = Some(c),
            '}' | '>' => opened = None,
            _ => {}
        }
        out.push(c);
        spelled.push(c);
    }
    out
}

/// A source file's path as a real-run program prints it: every run of hex
/// digits and dashes of eight or more that holds a digit (a uuid, a date
/// and time) prints as `<id>`, and so does any other segment that looks
/// like an id.
pub fn file_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        let core = run.trim_matches('-');
        if core.len() >= 8 && core.bytes().any(|b| b.is_ascii_digit()) {
            let lead = run.len() - run.trim_start_matches('-').len();
            let trail = run.len() - run.trim_end_matches('-').len();
            out.push_str(&"-".repeat(lead));
            out.push_str("<id>");
            out.push_str(&"-".repeat(trail));
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in path.chars() {
        if c.is_ascii_hexdigit() || c == '-' {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    normalize(&out)
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

/// What the source places at one window position: whether a record holds
/// it there (its own window), its marks, its links and its occurrences.
#[derive(Debug, Default)]
struct Expect {
    witnessed: bool,
    marks: BTreeSet<Token>,
    links: BTreeSet<Token>,
    occurrences: BTreeSet<BTreeSet<Token>>,
}

fn link_token(attribute: &str, value: Value) -> Token {
    Token {
        modality: String::new(),
        direction: String::new(),
        attribute: attribute.to_owned(),
        value,
    }
}

fn place(hash: &Hash) -> Value {
    Value::Place(hash.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Per witness: every witnessed window against the source window behind
/// its record, and every source window against the witnessed windows; then
/// every node's marks, links and occurrences against what the source places
/// at its window position.
fn occurrences(walked: &walk::Walked, rendered: &render::Rendered) -> Vec<(String, Kind, String)> {
    let mut rows = Vec::new();
    let facts: Vec<&BTreeSet<Token>> = rendered.graph.iter().map(|node| &node.facts).collect();
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

    // B's window positions: hash after -> (hash before, the items there).
    let mut positions: HashMap<Hash, (Hash, Vec<usize>)> = HashMap::new();
    let mut seen = |before: Hash, item: usize, after: Hash| {
        let entry = positions
            .entry(after)
            .or_insert_with(|| (before, Vec::new()));
        if !entry.1.contains(&item) {
            entry.1.push(item);
        }
    };
    let id = |item: usize| item_ids[item];
    let lines = walked.lines(&id, &step, &mut seen);

    // B's source windows: one per occurrence (record, position) of an item
    // its record holds.
    struct Occurrence {
        record: usize,
        position: usize,
        item: usize,
        hash: Hash,
    }
    let mut occurrences: Vec<Occurrence> = Vec::new();
    let mut own_before: HashMap<usize, Hash> = HashMap::new();
    let mut own_after: HashMap<usize, Hash> = HashMap::new();
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
            let before = hash;
            hash = step(&hash, item_ids[item]);
            seen(before, item, hash);
            own_before.insert(item, before);
            own_after.insert(item, hash);
            if walked.items[item].held {
                occurrences.push(Occurrence {
                    record,
                    position,
                    item,
                    hash,
                });
            }
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
    let path_of = |at: usize| -> String {
        let valued = || facts[at].iter().filter(|token| !walk::positional(token));
        valued()
            .find_map(|token| {
                walked
                    .tokens
                    .get(token)
                    .and_then(|origins| origins.first())
                    .map(|origin| origin.path.clone())
            })
            .or_else(|| valued().next().map(window_path))
            .or_else(|| {
                facts[at]
                    .iter()
                    .next()
                    .map(|token| format!("window:{}/{}/absent", token.modality, token.direction))
            })
            .unwrap_or_else(|| "window:empty".to_owned())
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
        let kind = if reordered { Kind::Order } else { Kind::Count };
        rows.push((path_of(at), kind, format!("node {at}")));
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

    // What the source places at each position.
    let mut expect: HashMap<Hash, Expect> = HashMap::new();
    for occurrence in &occurrences {
        let item = &walked.items[occurrence.item];
        let at = expect.entry(occurrence.hash).or_default();
        at.witnessed = true;
        at.marks.extend(item.marks.iter().cloned());
        at.marks.extend(item.carried.iter().cloned());
        let claims = walked.records_claims(occurrence.record);
        if !claims.is_empty() {
            at.occurrences.insert(claims.clone());
        }
        at.occurrences.extend(item.claims.iter().cloned());
        // A link: the latest candidate before the item in its window, by
        // how many places back it stands.
        for pair in &item.pairs {
            let mut cursor = own_before[&occurrence.item];
            let mut back = 1usize;
            let found = loop {
                let Some((previous, there)) = positions.get(&cursor) else {
                    break None;
                };
                if there.iter().any(|item| pair.candidates.contains(item)) {
                    break Some(back);
                }
                cursor = *previous;
                back += 1;
            };
            match found {
                Some(back) => {
                    at.links
                        .insert(link_token(&pair.attribute, Value::Number(back.to_string())));
                }
                None => rows.push((
                    pair.path.clone(),
                    Kind::Link,
                    format!("occurrence {} {}", occurrence.record, occurrence.position),
                )),
            }
        }
    }
    // A re-presented copy: an item a record holds, at a position other than
    // its own, recalls its own.
    for (after, (before, there)) in &positions {
        for &item in there {
            if !walked.items[item].held || own_before.get(&item) == Some(before) {
                continue;
            }
            let Some(original) = own_after.get(&item) else {
                continue;
            };
            let at = expect.entry(*after).or_default();
            at.marks.extend(walked.items[item].carried.iter().cloned());
            at.links.insert(link_token("recalls", place(original)));
        }
    }
    // The unit's own annotations, at the end of each line: an own
    // occurrence no position continues.
    if !walked.unit_marks.is_empty() || !walked.unit_claims.is_empty() {
        let continued: HashSet<Hash> = positions.values().map(|(before, _)| *before).collect();
        for occurrence in &occurrences {
            if continued.contains(&occurrence.hash) {
                continue;
            }
            let at = expect.entry(occurrence.hash).or_default();
            at.marks.extend(walked.unit_marks.iter().cloned());
            if !walked.unit_claims.is_empty() {
                let tuples = std::mem::take(&mut at.occurrences);
                at.occurrences = tuples
                    .into_iter()
                    .map(|mut tuple| {
                        tuple.extend(walked.unit_claims.iter().cloned());
                        tuple
                    })
                    .collect();
                if at.occurrences.is_empty() {
                    at.occurrences.insert(walked.unit_claims.clone());
                }
            }
        }
    }
    // Marks at the end of a record's line.
    for (record, token) in &walked.line_marks {
        match lines[*record].and_then(|lines| lines.line) {
            Some(hash) => {
                expect.entry(hash).or_default().marks.insert(token.clone());
            }
            None => rows.push((window_path(token), Kind::Placed, format!("record {record}"))),
        }
    }
    // A compaction: the first node of the new line that is not on the old
    // line continues the old line's tail.
    let line_back = |from: Hash| -> Vec<Hash> {
        let mut out = vec![from];
        let mut cursor = from;
        while let Some((previous, _)) = positions.get(&cursor) {
            out.push(*previous);
            cursor = *previous;
        }
        out
    };
    for (record, old_tip, tail) in walked.continuations(&lines) {
        let target = match tail {
            Some((hash, item)) => match own_after.get(&item) {
                Some(original) if *original != hash => *original,
                _ => hash,
            },
            None => old_tip,
        };
        let old: HashSet<Hash> = line_back(old_tip).into_iter().collect();
        let own = walked.ordered(record);
        let Some(end) = own.last().and_then(|item| own_after.get(item)) else {
            continue;
        };
        let mut new = line_back(*end);
        new.reverse();
        match new
            .into_iter()
            .find(|hash| !old.contains(hash) && *hash != [0; 32])
        {
            Some(first) => {
                expect
                    .entry(first)
                    .or_default()
                    .links
                    .insert(link_token("continues", place(&target)));
            }
            None => rows.push((
                "window:link/continues".to_owned(),
                Kind::Placed,
                format!("record {record}"),
            )),
        }
    }

    // Every node against what the source places at its position.
    let none = Expect::default();
    for (at, node) in rendered.graph.iter().enumerate() {
        let Some(hash) = node_hash[at] else { continue };
        if !positions.contains_key(&hash) {
            continue;
        }
        let expected = expect.get(&hash).unwrap_or(&none);
        let label = format!("node {at}");
        for token in node.marks.symmetric_difference(&expected.marks) {
            let path = walked
                .tokens
                .get(token)
                .and_then(|origins| origins.first())
                .map_or_else(|| window_path(token), |origin| origin.path.clone());
            rows.push((path, Kind::Placed, label.clone()));
        }
        let links: BTreeSet<Token> = node
            .links
            .iter()
            .map(|(attribute, target)| {
                let value = if attribute == "responds_to" {
                    let mut cursor = node.previous;
                    let mut back = 1usize;
                    loop {
                        match cursor {
                            None => break Value::Term("outside".to_owned()),
                            Some(up) if Some(up) == *target => {
                                break Value::Number(back.to_string())
                            }
                            Some(up) => {
                                cursor = rendered.graph[up].previous;
                                back += 1;
                            }
                        }
                    }
                } else {
                    match target.and_then(|target| node_hash[target]) {
                        Some(hash) => place(&hash),
                        None => Value::Term("outside".to_owned()),
                    }
                };
                link_token(attribute, value)
            })
            .collect();
        // Which node: its fact's shape; and whether the window holds the
        // link where the source places none (extra) or the reverse.
        let shape = node
            .facts
            .iter()
            .find(|token| token.attribute.is_empty())
            .map_or_else(String::new, |token| {
                format!("{}/{}", token.modality, token.direction)
            });
        for token in links.difference(&expected.links) {
            rows.push((
                format!("window:link/{}/extra/{shape}", token.attribute),
                Kind::Placed,
                label.clone(),
            ));
        }
        for token in expected.links.difference(&links) {
            rows.push((
                format!("window:link/{}/missing/{shape}", token.attribute),
                Kind::Placed,
                label.clone(),
            ));
        }
        if node.witnessed && expected.witnessed && node.occurrences != expected.occurrences {
            rows.push(("window:occurrence".to_owned(), Kind::Placed, label.clone()));
        }
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
