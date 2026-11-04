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
//! - [`Class::Correlator`]: a provider id the importer reads only to pair
//!   things (a call with its result) while parsing; always one leaf;
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
//!   `{user}.message.content[]{tool_result}.content[]{text}.text`. An
//!   object that carries several of them names each, in table order: in
//!   Codex, `{response_item}.payload{message}{user}.content[]{input_text}.text`.
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
    /// The term name of the source namespace every witness of the format
    /// names ([`crate::schemas::archive_v3::witness::NAMESPACES`]).
    pub namespace: &'static str,
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
    /// How the source's own markers shape a window beyond its record
    /// chain. The gate's walker builds the window behind every record from
    /// this and the order rule alone.
    pub windows: Windows,
    /// The gaps the rendering rule puts in her window, which the source
    /// holds no value for: each is an item of its window.
    pub gaps: Gaps,
    /// The links between items that the source's correlators establish.
    pub links: &'static [Link],
    /// Where an annotation sits when its attribute's default place
    /// ([`Place::of`]) does not hold: `(entry path, attribute, place)`.
    pub places: &'static [(&'static str, &'static str, Place)],
    pub entries: &'static [Entry],
}

/// One gap fact: a modality and direction (both empty for a span of
/// unknown kind) and its `absent` term (`body` or `span`). A [`GapRule`]
/// may name the empty term: a fact the source shows only by its place and
/// that holds no value at all, such as a result's head with no status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Gap {
    pub modality: &'static str,
    pub direction: &'static str,
    pub kind: &'static str,
}

/// The gaps a window holds where the source holds no value.
///
/// A window is `header ++ prefix ++ after_prefix ++ line`; each rule here
/// fills one of those places, or stands beside a value.
#[derive(Clone, Copy, Debug)]
pub struct Gaps {
    /// The settings header of a window whose request (or header record)
    /// records none.
    pub header: Option<Gap>,
    /// The system section of a window that no prefix record (or a prefix
    /// record holding nothing) opens.
    pub prefix: &'static [Gap],
    /// Items after the system section of every window.
    pub after_prefix: &'static [Gap],
    /// After the system section of a line whose first record names a
    /// parent the unit does not hold: what that parent's line held.
    pub orphan: Option<Gap>,
    /// Right after the head of each result a request clears, in place of
    /// its parts.
    pub cleared: Option<Gap>,
    /// Gaps beside a value.
    pub beside: &'static [GapRule],
}

impl Gaps {
    pub const NONE: Gaps = Gaps {
        header: None,
        prefix: &[],
        after_prefix: &[],
        orphan: None,
        cleared: None,
        beside: &[],
    };
}

/// A gap the rendering rule puts beside the value at `path` whenever the
/// walk reaches it and `when` holds (record conditions, or conditions on
/// the object that holds the value).
#[derive(Clone, Copy, Debug)]
pub struct GapRule {
    pub path: &'static str,
    pub when: &'static [Cond],
    pub at: Beside,
    pub gap: Gap,
    /// Whether the value's record holds the item ([`Beside::Before`] and
    /// [`Beside::After`]): a result's head that holds no value is held, a
    /// span the server read is not.
    pub held: bool,
}

/// Where a [`GapRule`]'s gap stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beside {
    /// On the value's own fact (the item of the gap's modality at the
    /// value's place): a fact whose body the source does not hold.
    Same,
    /// An item of its own right before the value's item. No record holds
    /// it, so it is no occurrence of its own.
    Before,
    /// An item of its own right after the value's item, held by no record.
    After,
}

/// A link an importer makes from one item to another, which the source
/// establishes by a correlator: the item at the place of the `from`
/// correlator (of modality `from_modality`) links to the latest item
/// before it in its window whose `to` correlator has the same value (of
/// modality `to_modality`).
#[derive(Clone, Copy, Debug)]
pub struct Link {
    /// `responds_to` or `spawned_by`.
    pub attribute: &'static str,
    pub from: &'static str,
    pub from_modality: &'static str,
    pub to: &'static [&'static str],
    pub to_modality: &'static str,
}

/// Where the gate places an annotation value of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// A claim of its record: one occurrence (the record's claims as one
    /// tuple) of the witness of each node the record holds. Under
    /// [`Order::File`] a record that holds no node gives its claims to the
    /// next node the file places, or, after the file's last node, to that
    /// last node.
    Claim,
    /// The record's last item.
    Last,
    /// Every item of the record, and every copy of it.
    Every,
    /// The item at the value's own place (its array element) whose
    /// modality is the place's only one.
    Here,
    /// The last item of the request whose [`Requests::id`] the value names.
    Request,
    /// The end of the line of the latest record before this one in the
    /// file that names itself ([`Order::Parents`]'s id). Before every such
    /// record there is no line: the value is a restatement and places
    /// nothing when a later record of the file, with a line before it,
    /// carries the same token; otherwise it fails.
    FileTail,
    /// The end of the line of the record's parent.
    ParentTail,
    /// The last `out` item of the line since the latest turn started
    /// ([`Order::File`]); nothing when the turn holds none.
    LastOut,
}

impl Place {
    /// The default place of an annotation attribute: claims of the record,
    /// `ends` on its last item, `rendering` on every item, a gloss on the
    /// item at its place.
    pub fn of(attribute: &str) -> Place {
        match attribute {
            "ends" => Place::Last,
            "rendering" => Place::Every,
            attribute if attribute.starts_with("gloss:") => Place::Here,
            _ => Place::Claim,
        }
    }
}

/// The window rules a source marks. Each names record-relative fields;
/// a record's shape is its discriminator value.
#[derive(Clone, Copy, Debug)]
pub struct Windows {
    /// Records of these shapes open every window that follows them in the
    /// file: the latest such record before a record is its window's prefix.
    pub prefix: &'static [&'static str],
    /// The normalized path of a request record's field whose fact is the
    /// request's settings header: it opens the window of the request and of
    /// every record after it on the chain, before the prefix, and is none of
    /// the record's own items. A request that records none opens with no
    /// item there.
    pub header: Option<&'static str>,
    pub compaction: Option<Compaction>,
    pub requests: Option<Requests>,
}

impl Windows {
    /// A source whose windows are exactly its record chains.
    pub const NONE: Windows = Windows {
        prefix: &[],
        header: None,
        compaction: None,
        requests: None,
    };
}

/// A record that replaces the context: its children start a new chain, and
/// the segment `head..=tail` of the old chain (by the order rule) is
/// re-presented right after the record `anchor` names.
#[derive(Clone, Copy, Debug)]
pub struct Compaction {
    pub shape: &'static str,
    /// The record-relative field naming the last record of the context the
    /// compaction replaces. A compaction record that names one the unit
    /// does not hold opens its line on [`Gaps::orphan`].
    pub parent: &'static str,
    pub head: &'static str,
    pub tail: &'static str,
    pub anchor: &'static str,
}

/// Sampled requests and the window edits they mark.
///
/// A request is a run of consecutive chain records of `shape` that share
/// the value of `id`. The window a request was sampled from is the whole
/// history before its first record, with only that request's own edits
/// applied (never an earlier request's).
#[derive(Clone, Copy, Debug)]
pub struct Requests {
    pub shape: &'static str,
    pub id: &'static str,
    /// Elements of the record-relative array `.0` whose member `type` is
    /// `.1` each drop one block, named by member `.2` as
    /// `messages.<i>.content.<j>`: block `j` of message `i`, where messages
    /// are the runs of one role in the window after its prefix.
    pub dropped: (&'static str, &'static str, &'static str),
    /// Elements of the record-relative array `.0` whose member `type` is
    /// `.1` clear the parts of the oldest results, as many as member `.2`
    /// counts.
    pub cleared: (&'static str, &'static str, &'static str),
    /// A result is a value at the normalized path `.0`. Its head is the
    /// item of modality `.1` it holds directly (it comes first, and counts
    /// even when it holds no value); its parts are every other item inside.
    pub result: (&'static str, &'static str),
    /// The modality of the blocks a drop may name.
    pub thinking: &'static str,
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
    /// Records follow each other in file order: a record's items continue
    /// the window the records before it left, shaped by the record kinds
    /// [`FileWindows`] names. Within a record, array order holds.
    File(FileWindows),
}

/// The window rules of a file-ordered source. Each kind is the records
/// whose fields meet all its conditions.
///
/// A window is `header ++ prefix ++ line`:
///
/// - a `header` record's items become the header (none: no header), the
///   settings she was sampled under; a later window opens with the latest;
/// - a `prefix` record's items become the prefix (the system section);
/// - a `replace` record starts a new line with its own items (a compaction
///   installs what it holds);
/// - a `turn` record marks where a turn starts on the line;
/// - a `rollback` record cuts the line back to the start of its last
///   `rollback_count` turns;
/// - any other record's items extend the line.
///
/// When the header or the prefix changes, the line is the same items
/// under the new one: a window that differs branches at the first
/// difference.
#[derive(Clone, Copy, Debug)]
pub struct FileWindows {
    pub header: &'static [Cond],
    pub prefix: &'static [Cond],
    pub replace: &'static [Cond],
    pub turn: &'static [Cond],
    pub rollback: &'static [Cond],
    /// The record-relative field of a rollback that counts its turns.
    pub rollback_count: &'static str,
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
    /// The field is a scalar (a string, number, `true`, `false` or `null`).
    Scalar(&'static str),
    /// A member of the object that holds the value is a scalar with this
    /// value (for conditions inside arrays, where record fields cannot
    /// reach).
    Sibling(&'static str, &'static str),
    /// A member of the object that holds the value is absent or has another
    /// value.
    SiblingNot(&'static str, &'static str),
    /// A member of the object that holds the value is absent or `null`.
    SiblingAbsent(&'static str),
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
    /// `claimed_media_type`, `claimed_title`, `compaction_summary`,
    /// `harness_written`, `ends`,
    /// or `gloss:<category>`.
    Annotation(&'static [(&'static str, Decode)]),
    /// A provider id read only while parsing (pairing a call and its result,
    /// grouping a message). It never reaches the pile. It names one leaf: an
    /// object or array at its path is walked member by member, and an entry
    /// the importer never reads is no correlator.
    Correlator,
    /// A marked window change: what the client sent in later requests in
    /// place of a call's member ([`Rewrite`]).
    Rewrite(Rewrite),
    /// A client copy of a member of a kept item, which is the member's
    /// source where the item holds none ([`Supply`]).
    Supply(Supply),
    Drop(DropReason),
    /// Meaning not yet verified; fails until answered.
    Open(&'static str),
}

/// A client copy of one member of a kept item: Codex's
/// `agent_message.phase` (ruling X1).
///
/// The copy names its item by text: the record-relative field `text` of the
/// copy's record equals a value at one of the normalized paths `of.0`; the
/// nearest such value before the copy in the file (else the nearest after
/// it) is its item's. Where that value's record holds the member, at its
/// record-relative field `of.1`, the copy must spell the same, or it is a
/// mirror fault. Where it holds none, the copy is the source of attribute
/// `attribute`, decoded by `decode`, of every fact of `modality` and
/// `direction` that record holds. A copy whose text names no value holds
/// nothing of its own (its text's own mirror says whether that is a fault).
#[derive(Clone, Copy, Debug)]
pub struct Supply {
    pub text: &'static str,
    pub of: &'static [(&'static str, &'static str)],
    pub attribute: &'static str,
    pub decode: Decode,
    pub modality: &'static str,
    pub direction: &'static str,
}

/// An object whose members, keyed by a call's id, hold what the client
/// sent in later requests in place of member `member` of the element of the
/// record-relative array `array` whose `key` member names it: Claude Code's
/// `wireToolInputs` (ruling S2).
///
/// Where a member equals the call's (JSON order-free, numbers by token) it
/// is a copy and holds nothing. Where it differs, the window changes: the
/// record's own window and the rest of its request keep the call as she
/// wrote it, and every later request's window holds, in the call's place,
/// a fact of `modality` and `direction`, with the fixed attributes `with`,
/// the call's member `name` as attribute `.1` of `name`, and the sent value
/// as attribute `value.0` decoded by `value.1`. It is held by no record and
/// recalls nothing: it is not her writing. A member that names no element
/// fails. The array holds the calls directly, under no other array.
#[derive(Clone, Copy, Debug)]
pub struct Rewrite {
    pub array: &'static str,
    pub key: &'static str,
    pub member: &'static str,
    pub name: (&'static str, &'static str),
    pub value: (&'static str, Decode),
    pub modality: &'static str,
    pub direction: &'static str,
    pub with: &'static [(&'static str, Decode)],
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
    /// A string followed by a constant text the rendering rule appends (a
    /// rendering her window held with a trailing newline).
    TextThen(&'static str),
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
    /// A URL-safe base64 string (padding optional): the decoded bytes,
    /// compared by hash.
    Base64Url,
    /// A `data:` URI with base64 data: the decoded bytes, compared by hash.
    DataUri,
    /// The media type of a `data:` URI, in canonical spelling.
    DataUriType,
    /// A string that holds tool-defined JSON (call arguments she wrote), as
    /// a `json_tree` like [`Decode::Json`].
    JsonText,
    /// One named member of a structure fact the importer assembles from
    /// several source fields: the value's JSON leaves under this JSON
    /// Pointer (`/origin/kind`). The members of one object (one array
    /// element, or the record outside every array) form one fact.
    Member(&'static str),
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
    /// Equal (JSON values order-free) to a value at one of these
    /// normalized paths somewhere in the unit: a client log line that
    /// copies a kept item.
    Unit(&'static [&'static str]),
    /// A string held inside a string at one of these normalized paths
    /// somewhere in the unit: a setting the client rendered into a kept
    /// message.
    Within(&'static [&'static str]),
    /// Equal to the record-relative field.
    Field(&'static str),
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
    /// Where the annotation `attribute` of the entry at `path` sits.
    pub fn place(&self, path: &str, attribute: &str) -> Place {
        self.places
            .iter()
            .find(|(at, name, _)| *at == path && *name == attribute)
            .map_or_else(|| Place::of(attribute), |(_, _, place)| *place)
    }

    /// The entries for exactly `path`, in table order.
    pub fn at<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a Entry> + 'a {
        self.entries.iter().filter(move |entry| entry.path == path)
    }
}

pub mod chatgpt;
pub mod claude_code;
pub mod codex;
