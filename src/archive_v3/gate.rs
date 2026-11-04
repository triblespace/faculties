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
//! 1. B's token set equals A's: every token B found is in a window (else
//!    *missing*, or *changed* when the same attribute and value sit in a
//!    window under another modality or direction);
//! 2. every token A rendered traces back to B (else *invented*);
//! 3. along every window, the facts follow the source's order rule (else
//!    *order*);
//! 4. B found nothing the table does not name, and every drop held.
//!
//! Sets, not multisets: copies of one item converge on one fact, so the same
//! token may stand for several source occurrences. `json_tree` objects
//! compare order-free, because a structure token names its leaf by key
//! path. A gap fact (`absent`) is structure: it holds no value to compare.
//!
//! The [`Report`] never holds a value: only format, path, kind and count, so
//! the gate can run over real transcripts and print its result.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::*;

use super::fields::FieldTable;

pub mod render;
#[cfg(test)]
mod tests;
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
    /// A window's facts break the source's order.
    Order,
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

/// Compare B's walk with A's rendering.
pub fn compare(table: &FieldTable, walked: &walk::Walked, rendered: &render::Rendered) -> Report {
    let mut rows: BTreeMap<(String, Kind), BTreeSet<String>> = BTreeMap::new();
    let mut note = |path: String, kind: Kind, what: String| {
        rows.entry((path, kind)).or_default().insert(what);
    };

    for (path, kind, at) in &walked.rows {
        note(path.clone(), *kind, at.clone());
    }
    for (path, kind, at) in &rendered.rows {
        note(path.clone(), *kind, at.clone());
    }

    let rendered_tokens: BTreeSet<&Token> = rendered.windows.iter().flatten().flatten().collect();
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
    let walked_shapes = shapes(&mut walked.tokens.keys());

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
    for (ordinal, token) in rendered_tokens.iter().enumerate() {
        if walked.tokens.contains_key(*token)
            || walked_shapes.contains_key(&(token.attribute.clone(), token.value.clone()))
        {
            continue;
        }
        note(window_path(token), Kind::Invented, ordinal.to_string());
    }

    // Order: place each window node at the origin of its first fact token
    // that occurs exactly once in the source, and require consecutive placed
    // nodes never to run against the source's order.
    for window in &rendered.windows {
        let mut last: Option<&Origin> = None;
        for node in window {
            let placed = node
                .iter()
                .filter(|token| !token.modality.is_empty())
                .filter_map(|token| match walked.tokens.get(token) {
                    Some(origins) if origins.len() == 1 => origins.first(),
                    _ => None,
                })
                .next();
            let Some(origin) = placed else { continue };
            if let Some(before) = last {
                if walked.precedes(origin, before) {
                    note(origin.path.clone(), Kind::Order, format!("{origin:?}"));
                }
            }
            last = Some(origin);
        }
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
