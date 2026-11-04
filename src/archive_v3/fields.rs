//! The importer field table: what every source field path means.
//!
//! Each importer has one [`FieldTable`]. It names every field path a source
//! format may hold and gives it exactly one [`Class`]:
//!
//! - [`Class::Fact`]: the decoded value is an attribute of a fact her window
//!   rendered, with the fact's modality and direction;
//! - [`Class::Structure`]: the value shapes the graph (order, discriminated
//!   shape, a vocabulary choice) and is held by no attribute of its own;
//! - [`Class::Annotation`]: the value annotates a witness or a node;
//! - [`Class::Correlator`]: a provider id read only to pair things (a call
//!   with its result) while parsing;
//! - [`Class::Drop`]: the value is left out, with the reason.
//!
//! [`Class::Open`] marks a field whose meaning is not yet verified. It fails
//! like an unknown path until the question is answered.
//!
//! Anything the table does not name fails: a path, a discriminator value, a
//! vocabulary value. There is no default class and no catch-all.
//!
//! # Paths
//!
//! A path is normalized from the source's key path:
//!
//! - `.name` is an object member;
//! - `[]` is any element of an array (array order is the source's order);
//! - `.<id>` is a member of an object whose member names are provider ids
//!   ([`FieldTable::id_keyed`]);
//! - `{value}` follows an object that carries a discriminator member
//!   ([`FieldTable::discriminators`]), naming its shape: in Claude Code,
//!   `{user}.message.content[]{tool_result}.content[]{text}.text`.
//!
//! The root of a JSONL line, and of a JSON document, is the empty path.
//!
//! # Conditions
//!
//! An entry may carry [`Cond`]itions on fields of the enclosing record,
//! given as record-relative key paths (`message.author.role`). For one value
//! at one path exactly one entry must apply. An entry applies only to values
//! its class can take (a text decode takes a string, a structure a leaf).
//! An empty drop competes with other entries only for an empty value: there
//! a fact, annotation or structure entry wins (`""` is text that holds no
//! value) and the empty drop wins over the rest (an open field that is
//! `null` or `[]` held nothing).
//! Shaped entries (`path{shape}`) are tried first; when none applies, the
//! unshaped path's entries are.
//!
//! The table is shared by an importer and the gate's source walker ([`super::gate`]).
//! It is declared data: the walker reads it, and so will the importer, but
//! the two never share code that interprets the source.

/// One importer's field table.
#[derive(Clone, Copy, Debug)]
pub struct FieldTable {
    /// Format name used in gate reports, e.g. `claude-code`.
    pub format: &'static str,
    /// A unit is JSON Lines (one record value per line) rather than one
    /// JSON document.
    pub jsonl: bool,
    /// Object members whose string value names the object's shape.
    pub discriminators: &'static [&'static str],
    /// Normalized paths of objects whose member names are provider ids.
    pub id_keyed: &'static [&'static str],
    /// Normalized path at which one record starts (`""`: each JSONL line).
    pub record: &'static str,
    /// The source's order rule over records.
    pub order: Order,
    pub entries: &'static [Entry],
}

/// How a source orders its records.
#[derive(Clone, Copy, Debug)]
pub enum Order {
    /// Each record names itself and its parent by record-relative fields;
    /// a record follows its parent. Within a record, array order holds.
    Parents {
        id: &'static str,
        parent: &'static str,
    },
}

/// One field path, under its conditions, and its class.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub path: &'static str,
    pub when: &'static [Cond],
    pub class: Class,
}

/// A condition on a field of the enclosing record. A scalar is compared by
/// its spelling: a string as decoded, a number by its exact token, `true`,
/// `false` or `null`. Fields are record-relative member paths; arrays are not
/// entered.
#[derive(Clone, Copy, Debug)]
pub enum Cond {
    /// The field is a scalar with this value.
    Is(&'static str, &'static str),
    /// The field is absent or has another value.
    IsNot(&'static str, &'static str),
    /// The field is a string that starts with this prefix.
    Prefix(&'static str, &'static str),
    /// The field is present with any value (an array or object included).
    Present(&'static str),
    /// The field is absent.
    Absent(&'static str),
    /// A member of the object that holds the value is a scalar with this
    /// value (for conditions inside arrays, where record fields cannot
    /// reach).
    Sibling(&'static str, &'static str),
    /// A member of the object that holds the value is absent or has another
    /// value.
    SiblingNot(&'static str, &'static str),
}

#[derive(Clone, Copy, Debug)]
pub enum Class {
    /// An attribute of a fact her window rendered.
    Fact {
        /// Modality term name, as in [`crate::schemas::archive_v3::fact::modality::SPECS`].
        modality: &'static str,
        /// `in` or `out`: who produced the fact.
        direction: &'static str,
        /// Fact attribute name: `payload`, `blob`, `structure`, `tool_name`,
        /// `tool_namespace`, `status`, `exit_code`, `media_type`,
        /// `source_uri`, `title`, `sense`, `detail`, `register` or `form`.
        attribute: &'static str,
        decode: Decode,
        /// Further attributes of the same fact that a rendering rule fixes
        /// for this path, each `(attribute, constant decode)`: a sealed
        /// blob's format media type, `form: voiced` on harness text in her
        /// slot. They are tokens of the value's own origin.
        with: &'static [(&'static str, Decode)],
    },
    Structure(Structure),
    /// Annotations of the witness or node, each `(attribute, decode)`:
    /// `raw_model`, `raw_role`, `raw_author`, `generated_by`,
    /// `source_locator`, `timestamp`, `pointer`, `claimed_size`,
    /// `claimed_media_type`, `claimed_title`, `compaction_summary`, `ends`,
    /// or `gloss:<category>`.
    Annotation(&'static [(&'static str, Decode)]),
    /// A provider id read only while parsing (pairing a call and its result,
    /// grouping a message). It never reaches the pile.
    Correlator,
    Drop(DropReason),
    /// Meaning not yet verified; fails until answered.
    Open(&'static str),
}

/// What a structure field does.
#[derive(Clone, Copy, Debug)]
pub enum Structure {
    /// A discriminator member: its value is already part of the path.
    Discriminator,
    /// One of a closed vocabulary; any other value fails.
    Vocabulary(&'static [&'static str]),
    /// The record id or parent id of [`Order::Parents`].
    Order,
    /// A value whose presence alone shapes the graph (a mapping node with
    /// no message), with the reason. Only a leaf may be shape.
    Shape(&'static str),
}

/// How a value decodes into a gate token.
#[derive(Clone, Copy, Debug)]
pub enum Decode {
    /// A string, exactly as decoded.
    Text,
    /// A base64 string: the decoded bytes, compared by hash.
    Base64,
    /// A number, by its exact token.
    Number,
    Bool,
    /// Tool-defined JSON, compared as a `json_tree`: objects order-free,
    /// arrays ordered, numbers by token.
    Json,
    /// A media type, in canonical spelling (lowercased names, sorted
    /// parameters, values exact).
    MediaType,
    /// An RFC 3339 / ISO 8601 time.
    Timestamp,
    /// Seconds since the Unix epoch, as an exact decimal number.
    UnixSeconds,
    /// A closed vocabulary: source spelling to term name. Any other value
    /// fails.
    Term(&'static [(&'static str, &'static str)]),
    /// A closed vocabulary: source spelling to an exact text. Any other
    /// value fails.
    TextMap(&'static [(&'static str, &'static str)]),
    /// A constant text the rendering rule fixes, whatever the value.
    ConstText(&'static str),
    /// A constant vocabulary term the rendering rule fixes, whatever the
    /// value.
    ConstTerm(&'static str),
}

/// Why a field is left out.
#[derive(Clone, Copy, Debug)]
pub enum DropReason {
    /// Machinery she never saw (usage, envelope, client state), or content
    /// she never saw such as originals behind a preview.
    Machinery(&'static str),
    /// A copy of what is kept; it must equal it, or the import fails.
    Mirror(Mirror),
    /// Always empty; a non-empty value fails.
    Empty,
    /// A provider id with no role in her window.
    ProviderId,
}

/// What a mirror copies.
#[derive(Clone, Copy, Debug)]
pub enum Mirror {
    /// The member of the same object with this name.
    Sibling(&'static str),
    /// The ids of the records whose parent is this record ([`Order::Parents`]).
    Children,
    /// Members of an object keyed by provider id, each mirroring `member`
    /// of the element of the record-relative array `array` whose `key`
    /// member equals the member name: Claude Code's `wireToolInputs`.
    /// JSON values compare order-free, numbers by token.
    Keyed {
        array: &'static str,
        key: &'static str,
        member: &'static str,
    },
}

impl Entry {
    pub const fn fact(
        path: &'static str,
        modality: &'static str,
        direction: &'static str,
        attribute: &'static str,
        decode: Decode,
    ) -> Self {
        Self::fact_with(path, modality, direction, attribute, decode, &[])
    }

    pub const fn fact_with(
        path: &'static str,
        modality: &'static str,
        direction: &'static str,
        attribute: &'static str,
        decode: Decode,
        with: &'static [(&'static str, Decode)],
    ) -> Self {
        Entry {
            path,
            when: &[],
            class: Class::Fact {
                modality,
                direction,
                attribute,
                decode,
                with,
            },
        }
    }

    pub const fn new(path: &'static str, class: Class) -> Self {
        Entry {
            path,
            when: &[],
            class,
        }
    }

    pub const fn when(self, when: &'static [Cond]) -> Self {
        Entry {
            path: self.path,
            when,
            class: self.class,
        }
    }
}

impl FieldTable {
    /// The entries for exactly `path`, in table order.
    pub fn at<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a Entry> + 'a {
        self.entries.iter().filter(move |entry| entry.path == path)
    }
}

pub mod chatgpt;
pub mod claude_code;
