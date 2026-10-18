//! Shared zero-copy source-adapter substrate for Archive transcript importers.
//!
//! Source adapters own only format knowledge: they scan immutable bytes into
//! [`SourceRecord`]s whose strings remain views of the source allocation
//! while projecting; no source bytes reach the pile. This module owns source-DAG
//! validation, contentless-record contraction, node chaining, tool-call
//! linking, content construction, source receipts, and callback emission. No adapter needs a dynamic JSON tree,
//! repository head, branch, or cursor.
//!
//! Direction is `in` or `out` only: everything the agent perceives (system
//! prompts, compaction summaries, tool results, unknown senders) is `in`, and
//! everything it produces (text, tool calls, thinking, sealed reasoning) is
//! `out`. Every part of every record is one node in the main chain, in the
//! order the agent experienced it, so what it perceived before acting is an
//! ancestor of the act. Acts and perceptions are derived from that chain
//! ([`crate::blockdag::turns_along`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anybytes::{ByteArea, Bytes, View};
use anyhow::{bail, Context, Result};
use triblespace::core::import::scanner as sc;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

use crate::blockdag::{self, ProjectionAnnotations};
use crate::schemas::blockdag as schema;

/// One immutable, private copy of a potentially live source prefix.
///
/// `digest` is ephemeral mutation-detection evidence for a multi-pass adapter.
/// The bytes are only parsed; no source byte is ever written to the pile.
#[derive(Clone, Debug)]
pub struct FrozenSource {
    pub bytes: Bytes,
    pub digest: [u8; 32],
}

/// Freeze the file size observed from one open handle.
pub fn freeze_file(path: &Path) -> Result<FrozenSource> {
    let mut source =
        File::open(path).with_context(|| format!("open {} for freezing", path.display()))?;
    let len = source
        .metadata()
        .with_context(|| format!("stat {} for freezing", path.display()))?
        .len();
    freeze_open_file(&mut source, path, len)
}

/// Freeze exactly `len` leading bytes from a potentially growing file.
pub fn freeze_prefix(path: &Path, len: u64) -> Result<FrozenSource> {
    let mut source =
        File::open(path).with_context(|| format!("open {} for freezing", path.display()))?;
    freeze_open_file(&mut source, path, len)
}

fn freeze_open_file(source: &mut File, path: &Path, len: u64) -> Result<FrozenSource> {
    let len = usize::try_from(len).with_context(|| {
        format!(
            "source prefix for {} does not fit this platform's address space",
            path.display()
        )
    })?;
    if len == 0 {
        return Ok(FrozenSource {
            bytes: Bytes::empty(),
            digest: *blake3::hash(&[]).as_bytes(),
        });
    }
    let mut area = ByteArea::new().context("create private source freeze area")?;
    let mut sections = area.sections();
    let mut section = sections
        .reserve::<u8>(len)
        .context("reserve source freeze area")?;
    source
        .read_exact(&mut section)
        .with_context(|| format!("freeze {len} bytes from {}", path.display()))?;
    let digest = *blake3::hash(&section).as_bytes();
    let bytes = section
        .freeze()
        .with_context(|| format!("make frozen source {} immutable", path.display()))?;
    Ok(FrozenSource { bytes, digest })
}

/// One ordered semantic datum extracted from a source occurrence.
#[derive(Clone, Debug)]
pub enum SourcePart {
    /// UTF-8 text, thinking, event, or textual tool payload.
    Text {
        modality: Id,
        direction: Id,
        value: View<str>,
    },
    /// Resident binary payload.
    Blob {
        modality: Id,
        direction: Id,
        bytes: Bytes,
        media_type: View<str>,
    },
    /// External asset, optionally accompanied by recovered bytes. The media
    /// type and size are what the export itself says; recovered bytes only
    /// annotate the fact and never decide either.
    Pointer {
        modality: Id,
        direction: Id,
        namespace: Id,
        pointer: View<str>,
        media_type: Option<View<str>>,
        size: Option<u128>,
        resolved: Option<Bytes>,
    },
}

impl SourcePart {
    /// Construct a text part without copying its backing source allocation.
    pub fn text(modality: Id, direction: Id, value: View<str>) -> Self {
        Self::Text {
            modality,
            direction,
            value,
        }
    }

    /// Whether the agent perceived (`in`) or produced (`out`) this datum.
    pub fn direction(&self) -> Id {
        match self {
            Self::Text { direction, .. }
            | Self::Blob { direction, .. }
            | Self::Pointer { direction, .. } => *direction,
        }
    }
}

/// A vendor correlator between a tool call and the result that answers it.
///
/// It threads `responds_to` while projecting and is never stored: the vendor's
/// call ids stay out of every hashed text, and structure carries the link.
#[derive(Clone, Debug)]
pub enum ToolLink {
    /// The part is the tool call this key names.
    Call(View<str>),
    /// The part answers the call this key names.
    Result(View<str>),
}

/// Parsed claims that remain occurrence-scoped rather than entering block ID.
#[derive(Clone, Debug, Default)]
pub struct SourceClaims {
    pub timestamp: Option<Inline<NsTAIInterval>>,
    pub author: Option<Id>,
    pub experiencer: Option<Id>,
    pub raw_author: Option<View<str>>,
    pub raw_role: Option<View<str>>,
    pub raw_model: Option<View<str>>,
}

/// One source occurrence plus its semantic interpretation.
#[derive(Clone, Debug)]
pub struct SourceRecord {
    /// The record's key inside one projection call. It only threads the
    /// source graph while projecting and is never stored: identity comes
    /// from content and structure alone.
    pub locator: View<str>,
    /// Keys of records that precede this one.
    pub predecessors: Vec<View<str>>,
    /// Genuine event time, attached to the block as a nonidentity annotation.
    pub block_timestamp: Option<Inline<NsTAIInterval>>,
    /// Ordered content: what was perceived (`in`) and produced (`out`), in
    /// the order the agent experienced it. A record without content projects
    /// nothing and is contracted out of its children's predecessor paths.
    pub parts: Vec<SourcePart>,
    /// Tool correlators of individual parts, by index into `parts`.
    pub links: Vec<(usize, ToolLink)>,
    /// Nonidentity source claims.
    pub claims: SourceClaims,
}

impl SourceRecord {
    /// A record with no act key, tool links or claims.
    pub fn new(locator: View<str>, predecessors: Vec<View<str>>, parts: Vec<SourcePart>) -> Self {
        Self {
            locator,
            predecessors,
            block_timestamp: None,
            parts,
            links: Vec::new(),
            claims: SourceClaims::default(),
        }
    }
}

/// One source-only fragment emitted to an Archive import writer.
pub struct ProjectedSource {
    pub source_path: PathBuf,
    pub fragment: Fragment,
    /// Key of the last record whose content this block holds, for
    /// diagnostics and tests. `None` on a leaf annotation. It is never stored.
    pub record: Option<View<str>>,
}

/// Common accounting across source adapters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectionStats {
    pub records_seen: usize,
    /// Nodes emitted: one per part.
    pub projections_emitted: usize,
    pub content_parts: usize,
    pub contentless_records: usize,
    pub missing_predecessors: usize,
    /// Tool results whose call the source does not hold.
    pub unresolved_tool_results: usize,
}

impl ProjectionStats {
    /// Add another unit's accounting to this one.
    pub fn absorb(&mut self, other: Self) {
        self.records_seen += other.records_seen;
        self.projections_emitted += other.projections_emitted;
        self.content_parts += other.content_parts;
        self.contentless_records += other.contentless_records;
        self.missing_predecessors += other.missing_predecessors;
        self.unresolved_tool_results += other.unresolved_tool_results;
    }
}

/// Map a user-supplied immutable export into one shareable byte owner.
///
/// `anybytes` keeps the mapping alive for every derived `Bytes` and `View`, but
/// the file itself must not be concurrently modified. Live logs and editor
/// state must use [`read_file`] or a line-buffered scanner instead.
pub fn map_immutable_file(path: &Path) -> Result<Bytes> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if len == 0 {
        return Ok(Bytes::empty());
    }
    // SAFETY: this API is intentionally restricted to immutable data exports.
    // The returned Bytes owns the mmap, so derived views cannot outlive it.
    unsafe { Bytes::map_file(&file) }.with_context(|| format!("mmap {}", path.display()))
}

/// Read a potentially mutable source into one immutable shareable allocation.
pub fn read_file(path: &Path) -> Result<Bytes> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(Bytes::from_source(bytes))
}

/// Parse a JSON string directly into a source-backed UTF-8 view.
pub fn string(bytes: &mut Bytes) -> std::result::Result<View<str>, sc::ScanError> {
    sc::parse_string(bytes)?
        .view::<str>()
        .map_err(|_| sc::ScanError::Syntax("JSON string is not UTF-8".to_owned()))
}

/// Capture one exact JSON value as a zero-copy slice while advancing input.
pub fn raw_value(bytes: &mut Bytes) -> std::result::Result<Bytes, sc::ScanError> {
    sc::take_value(bytes)
}

/// Canonicalize one JSON value without constructing a dynamic value tree.
///
/// Arrays stream in source order. Objects alone require temporary storage so
/// decoded keys can be sorted; duplicate keys retain their final value. This
/// deliberately freezes the historical dynamic-tree spelling used by Archive
/// tool payloads while keeping parsing source-backed and scanner-native.
pub fn canonical_json(mut bytes: Bytes) -> std::result::Result<String, sc::ScanError> {
    sc::skip_ws(&mut bytes);
    let canonical = canonical_json_value(&mut bytes)?;
    sc::skip_ws(&mut bytes);
    if !bytes.is_empty() {
        return Err(sc::ScanError::Syntax(
            "trailing bytes after canonical JSON value".to_owned(),
        ));
    }
    Ok(canonical)
}

/// Canonicalize one JSON object without the named top-level members.
///
/// Used where a record's envelope (ids, times, client state) must not enter
/// the content it carries. A non-object value is canonicalized whole.
pub fn canonical_json_without(
    mut bytes: Bytes,
    without: &[&str],
) -> std::result::Result<String, sc::ScanError> {
    sc::skip_ws(&mut bytes);
    if bytes.first().copied() != Some(b'{') {
        return canonical_json(bytes);
    }
    let members = sc::object(
        &mut bytes,
        BTreeMap::<String, String>::new(),
        |mut members, key, value| {
            let key = key
                .view::<str>()
                .map_err(|_| sc::ScanError::Syntax("JSON object key is not UTF-8".to_owned()))?;
            if without.contains(&key.as_ref()) {
                sc::skip_value(value)?;
            } else {
                members.insert(key.as_ref().to_owned(), canonical_json_value(value)?);
            }
            Ok(members)
        },
    )?;
    sc::skip_ws(&mut bytes);
    if !bytes.is_empty() {
        return Err(sc::ScanError::Syntax(
            "trailing bytes after canonical JSON value".to_owned(),
        ));
    }
    let mut out = String::from("{");
    for (ordinal, (key, value)) in members.into_iter().enumerate() {
        if ordinal != 0 {
            out.push(',');
        }
        out.push_str(&canonical_json_string(&key));
        out.push(':');
        out.push_str(&value);
    }
    out.push('}');
    Ok(out)
}

fn canonical_json_value(bytes: &mut Bytes) -> std::result::Result<String, sc::ScanError> {
    match bytes.first().copied() {
        Some(b'"') => Ok(canonical_json_string(string(bytes)?.as_ref())),
        Some(b'{') => {
            let members = sc::object(
                bytes,
                BTreeMap::<String, String>::new(),
                |mut members, key, value| {
                    let key = key.view::<str>().map_err(|_| {
                        sc::ScanError::Syntax("JSON object key is not UTF-8".to_owned())
                    })?;
                    members.insert(key.as_ref().to_owned(), canonical_json_value(value)?);
                    Ok(members)
                },
            )?;
            let mut out = String::from("{");
            for (ordinal, (key, value)) in members.into_iter().enumerate() {
                if ordinal != 0 {
                    out.push(',');
                }
                out.push_str(&canonical_json_string(&key));
                out.push(':');
                out.push_str(&value);
            }
            out.push('}');
            Ok(out)
        }
        Some(b'[') => {
            let values = sc::array(bytes, String::new(), |mut out, value| {
                if !out.is_empty() {
                    out.push(',');
                }
                out.push_str(&canonical_json_value(value)?);
                Ok(out)
            })?;
            Ok(format!("[{values}]"))
        }
        Some(b't') => {
            sc::expect_literal(bytes, b"true")?;
            Ok("true".to_owned())
        }
        Some(b'f') => {
            sc::expect_literal(bytes, b"false")?;
            Ok("false".to_owned())
        }
        Some(b'n') => {
            sc::expect_literal(bytes, b"null")?;
            Ok("null".to_owned())
        }
        Some(_) => canonical_json_number(sc::parse_number(bytes)?),
        None => Err(sc::ScanError::Syntax("expected JSON value".to_owned())),
    }
}

fn canonical_json_number(raw: Bytes) -> std::result::Result<String, sc::ScanError> {
    let raw = raw
        .view::<str>()
        .map_err(|_| sc::ScanError::Syntax("JSON number is not UTF-8".to_owned()))?;
    let raw = raw.as_ref();
    if !raw.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
        if raw.starts_with('-') {
            if raw != "-0" {
                if let Ok(value) = raw.parse::<i64>() {
                    return Ok(value.to_string());
                }
            }
        } else if let Ok(value) = raw.parse::<u64>() {
            return Ok(value.to_string());
        }
    }
    let value = raw
        .parse::<f64>()
        .map_err(|_| sc::ScanError::Syntax("invalid JSON number".to_owned()))?;
    if !value.is_finite() {
        return Err(sc::ScanError::Syntax(
            "JSON number is out of range".to_owned(),
        ));
    }
    Ok(zmij::Buffer::new().format_finite(value).to_owned())
}

/// Return the canonical JSON spelling of one decoded string.
pub fn canonical_json_string(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            control if control <= '\u{1f}' => {
                let byte = control as u8;
                out.push_str("\\u00");
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

/// Convert computed UTF-8 into the same view type used by source slices.
pub fn owned_text(value: impl Into<String>) -> View<str> {
    Bytes::from_source(value.into())
        .view::<str>()
        .expect("String is valid UTF-8")
}

/// Wire alphabet of a sealed reasoning token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SealedAlphabet {
    /// RFC 4648 base64 with padding (Anthropic signatures).
    Standard,
    /// RFC 4648 URL-safe base64 with padding (OpenAI Fernet tokens).
    UrlSafe,
}

/// The bytes of one sealed reasoning token and the media type that says what
/// they are. A token is decoded when its base64 spelling round-trips exactly,
/// so the provider's own spelling is recoverable from the bytes and the type.
/// Otherwise the token's exact text is kept under `<media_type>+text`.
pub fn sealed_bytes(token: &str, media_type: &str, alphabet: SealedAlphabet) -> (Vec<u8>, String) {
    use base64::Engine as _;
    let engine = match alphabet {
        SealedAlphabet::Standard => &base64::engine::general_purpose::STANDARD,
        SealedAlphabet::UrlSafe => &base64::engine::general_purpose::URL_SAFE,
    };
    match engine.decode(token.as_bytes()) {
        Ok(bytes) if engine.encode(&bytes) == token => (bytes, media_type.to_owned()),
        _ => (token.as_bytes().to_vec(), format!("{media_type}+text")),
    }
}

/// One sealed reasoning token as a THINKING blob fact.
pub fn sealed_fact(token: &str, media_type: &str, alphabet: SealedAlphabet) -> Result<Fragment> {
    let (bytes, media_type) = sealed_bytes(token, media_type, alphabet);
    blockdag::blob_fact(
        schema::content_fact::modality::THINKING,
        schema::content_fact::direction::OUT,
        bytes,
        &media_type,
    )
}

/// One sealed reasoning token as a source part.
pub fn sealed_part(token: &str, media_type: &str, alphabet: SealedAlphabet) -> SourcePart {
    let (bytes, media_type) = sealed_bytes(token, media_type, alphabet);
    SourcePart::Blob {
        modality: schema::content_fact::modality::THINKING,
        direction: schema::content_fact::direction::OUT,
        bytes: Bytes::from_source(bytes),
        media_type: owned_text(media_type),
    }
}

/// Pick a canonical binary modality from a MIME spelling.
pub fn modality_for_media_type(media_type: &str) -> Id {
    if media_type.starts_with("image/") {
        schema::content_fact::modality::IMAGE
    } else if media_type.starts_with("audio/") {
        schema::content_fact::modality::AUDIO
    } else if media_type.starts_with("video/") {
        schema::content_fact::modality::VIDEO
    } else {
        schema::content_fact::modality::FILE
    }
}

/// Build one content fact, and separately the recovered bytes of a pointer
/// fact. The bytes annotate the fact and never enter the part's or block's
/// identity: whether an export happened to include an asset file is not part
/// of the message.
fn build_fact(part: SourcePart) -> Result<(Fragment, Option<Fragment>)> {
    let fact = match part {
        SourcePart::Text {
            modality,
            direction,
            value,
        } => blockdag::text_fact_view(modality, direction, value)?,
        SourcePart::Blob {
            modality,
            direction,
            bytes,
            media_type,
        } => blockdag::blob_fact(modality, direction, bytes, media_type.as_ref())?,
        SourcePart::Pointer {
            modality,
            direction,
            namespace,
            pointer,
            media_type,
            size,
            resolved,
        } => {
            let fact = blockdag::asset_pointer_fact_view(
                modality,
                direction,
                namespace,
                pointer,
                media_type.as_ref().map(AsRef::as_ref),
                size,
            )?;
            let resolution = match resolved {
                Some(bytes) => {
                    let root = fact.root().expect("content fact is rooted");
                    Some(blockdag::resolve_pointer_fact(
                        Fragment::rooted(root, TribleSet::new()),
                        bytes,
                    )?)
                }
                None => None,
            };
            return Ok((fact, resolution));
        }
    };
    Ok((fact, None))
}

/// Conversation-level annotations of one projected source unit.
///
/// They land on the unit's leaves only (the blocks no other block of the unit
/// follows), never on every message, and never in an identity.
#[derive(Clone, Debug, Default)]
pub struct UnitAnnotations {
    /// The conversation's own id, as its content spells it (conversation or
    /// session id).
    pub source_locator: Option<String>,
}

/// The smallest interval covering every given one.
pub fn interval_hull(
    intervals: impl IntoIterator<Item = Inline<NsTAIInterval>>,
) -> Option<Inline<NsTAIInterval>> {
    let mut hull: Option<(i128, i128)> = None;
    for interval in intervals {
        let Ok((lower, upper)) = interval.try_from_inline() else {
            continue;
        };
        hull = Some(match hull {
            Some((low, high)) => (low.min(lower), high.max(upper)),
            None => (lower, upper),
        });
    }
    let (lower, upper) = hull?;
    let epoch = |nanos: i128| {
        hifitime::Epoch::from_tai_duration(hifitime::Duration::from_total_nanoseconds(nanos))
    };
    (epoch(lower), epoch(upper)).try_to_inline().ok()
}

/// Project a complete source graph through the canonical node DAG.
///
/// Contentless records are contracted. Every other record joins the main
/// chain in its source position as one node per part, in the order the
/// record lists them: each node follows the node before it, and a record's
/// first node follows the last nodes of the records before it. A tool
/// result's node names the node of the call it answers through
/// `responds_to`. Records are emitted in topological order, ties broken by
/// the id of each record's first node, so the order is a function of the
/// nodes: independent of source iteration order and of provider keys or raw
/// bytes. Every emitted fragment is
/// one whole node (the node and its fact) with its
/// [`blockdag::block_source_projection`] receipt, annotated with the claims of
/// its record; provider keys thread the graph while projecting and are never
/// stored. A longer version of the same source only appends nodes.
pub fn project_records<F>(
    source_namespace: Id,
    source_path: &Path,
    records: Vec<SourceRecord>,
    unit: UnitAnnotations,
    mut emit: F,
) -> Result<ProjectionStats>
where
    F: FnMut(ProjectedSource) -> Result<()>,
{
    let mut by_locator = HashMap::<View<str>, usize>::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        if by_locator.insert(record.locator.clone(), index).is_some() {
            bail!("duplicate source locator {:?}", record.locator.as_ref());
        }
    }

    let mut stats = ProjectionStats {
        records_seen: records.len(),
        ..ProjectionStats::default()
    };
    let mut indegree = vec![0usize; records.len()];
    let mut children = vec![Vec::<usize>::new(); records.len()];
    for (index, record) in records.iter().enumerate() {
        let mut unique = BTreeSet::new();
        for predecessor in &record.predecessors {
            if let Some(&parent) = by_locator.get(predecessor) {
                if unique.insert(parent) {
                    indegree[index] += 1;
                    children[parent].push(index);
                }
            } else {
                stats.missing_predecessors += 1;
            }
        }
    }

    // Kahn's order with ties broken by content. A record is built as soon as
    // every predecessor has been emitted, which fixes its frontier, and it
    // then waits in `ready` under the id of its first node. The smallest id
    // is emitted next, so wherever several records are ready (Gemini cards,
    // the roots and branches of a conversation tree) the order, and with it
    // every piece the import writer cuts, is a function of the nodes alone,
    // never of provider ids or raw bytes. A contentless record emits nothing
    // and releases its children at once.
    let mut frontiers = vec![BTreeSet::<Id>::new(); records.len()];
    let mut call_nodes = HashMap::<View<str>, Id>::new();
    let mut followed = BTreeSet::<Id>::new();
    let mut receipts = Vec::<(Id, Id)>::new();
    let mut staged: HashMap<usize, Vec<ProjectedSource>> = HashMap::new();
    let mut ready = BTreeSet::<(Id, usize)>::new();
    let mut releasable: Vec<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect();
    let mut emitted = 0usize;
    loop {
        while let Some(index) = releasable.pop() {
            let record = &records[index];
            let mut frontier = BTreeSet::new();
            for predecessor in &record.predecessors {
                if let Some(&parent) = by_locator.get(predecessor) {
                    frontier.extend(frontiers[parent].iter().copied());
                }
            }
            if record.parts.is_empty() {
                // Nothing to store: the record only forwards its ancestors.
                stats.contentless_records += 1;
                frontiers[index] = frontier;
                emitted += 1;
                for &child in &children[index] {
                    indegree[child] -= 1;
                    if indegree[child] == 0 {
                        releasable.push(child);
                    }
                }
                continue;
            }
            let claims = &record.claims;
            let annotations = ProjectionAnnotations {
                source_timestamp: claims.timestamp,
                author: claims.author,
                experiencer: claims.experiencer,
                raw_author: claims
                    .raw_author
                    .as_ref()
                    .map(|value| value.as_ref().to_owned()),
                raw_role: claims
                    .raw_role
                    .as_ref()
                    .map(|value| value.as_ref().to_owned()),
                raw_model: claims
                    .raw_model
                    .as_ref()
                    .map(|value| value.as_ref().to_owned()),
                ..ProjectionAnnotations::default()
            };
            let mut first = None;
            let mut projected = Vec::with_capacity(record.parts.len());
            for (position, part) in record.parts.iter().cloned().enumerate() {
                let (fact, resolution) = build_fact(part)?;
                let mut node =
                    blockdag::block(frontier.iter().copied(), record.block_timestamp, fact)?;
                let node_id = node.root().expect("canonical node is rooted");
                first.get_or_insert(node_id);
                if let Some(resolution) = resolution {
                    // Facts and bytes only: the fact is not exported again.
                    let (_, facts, metafacts, blobs) = resolution.into_parts();
                    node += Fragment::from_parts(facts, metafacts, blobs);
                }
                let link = record
                    .links
                    .iter()
                    .find_map(|(part, link)| (*part == position).then_some(link));
                match link {
                    Some(ToolLink::Call(key)) => {
                        call_nodes.entry(key.clone()).or_insert(node_id);
                    }
                    Some(ToolLink::Result(key)) => match call_nodes.get(key) {
                        Some(call) => node += blockdag::responds_to(node_id, *call),
                        None => stats.unresolved_tool_results += 1,
                    },
                    None => {}
                }
                followed.extend(frontier.iter().copied());
                let receipt = blockdag::block_source_projection(source_namespace, node)?;
                let receipt = blockdag::annotate_source_projection(receipt, annotations.clone())?;
                receipts.push((
                    node_id,
                    receipt.root().expect("canonical receipt is rooted"),
                ));
                projected.push(ProjectedSource {
                    source_path: source_path.to_path_buf(),
                    fragment: receipt,
                    record: Some(record.locator.clone()),
                });
                stats.projections_emitted += 1;
                stats.content_parts += 1;
                frontier = BTreeSet::from([node_id]);
            }
            frontiers[index] = frontier;
            staged.insert(index, projected);
            ready.insert((first.expect("a record with parts has a node"), index));
        }
        let Some((_, index)) = ready.pop_first() else {
            break;
        };
        for projected in staged.remove(&index).unwrap_or_default() {
            emit(projected)?;
        }
        emitted += 1;
        for &child in &children[index] {
            indegree[child] -= 1;
            if indegree[child] == 0 {
                releasable.push(child);
            }
        }
    }
    if emitted != records.len() {
        bail!("source predecessor graph contains a cycle");
    }
    // The conversation's id lands on the unit's leaves only.
    let mut annotated = BTreeSet::new();
    for (node, receipt) in receipts {
        let Some(locator) = unit.source_locator.clone() else {
            break;
        };
        if followed.contains(&node) || !annotated.insert(receipt) {
            continue;
        }
        emit(ProjectedSource {
            source_path: source_path.to_path_buf(),
            fragment: blockdag::source_occurrence(
                receipt,
                ProjectionAnnotations {
                    source_locator: Some(locator),
                    ..ProjectionAnnotations::default()
                },
            )?,
            record: None,
        })?;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IN: Id = schema::content_fact::direction::IN;
    const OUT: Id = schema::content_fact::direction::OUT;

    fn part(direction: Id, text: &str) -> SourcePart {
        SourcePart::text(
            schema::content_fact::modality::TEXT,
            direction,
            owned_text(text),
        )
    }

    fn record(locator: &str, predecessors: &[&str], parts: Vec<SourcePart>) -> SourceRecord {
        SourceRecord::new(
            owned_text(locator),
            predecessors.iter().copied().map(owned_text).collect(),
            parts,
        )
    }

    fn said(
        locator: &str,
        predecessors: &[&str],
        direction: Id,
        text: Option<&str>,
    ) -> SourceRecord {
        record(
            locator,
            predecessors,
            text.map(|text| vec![part(direction, text)])
                .unwrap_or_default(),
        )
    }

    fn project(records: Vec<SourceRecord>, unit: UnitAnnotations) -> (ProjectionStats, Fragment) {
        let mut union = Fragment::empty();
        let stats = project_records(
            schema::source_projection::SOURCE_AGY,
            Path::new("fixture.json"),
            records,
            unit,
            |projected| {
                union += projected.fragment;
                Ok(())
            },
        )
        .unwrap();
        (stats, union)
    }

    fn blocks(fragment: &Fragment) -> BTreeSet<Id> {
        find!(
            block: Id,
            pattern!(fragment, [{ _?receipt @ schema::source_projection::projects_to: ?block }])
        )
        .collect()
    }

    fn block_with_text(fragment: &mut Fragment, text: &str) -> Id {
        let handle =
            fragment.put::<triblespace::prelude::blobencodings::UTF8String, _>(text.to_owned());
        find!(
            block: Id,
            pattern!(&*fragment, [
                { ?block @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::payload: handle },
            ])
        )
        .next()
        .expect("a block carries the text")
    }

    fn follows(fragment: &Fragment, child: Id, parent: Id) -> bool {
        exists!(pattern!(fragment, [{ child @ schema::block::previous: parent }]))
    }

    #[test]
    fn contentless_nodes_store_nothing_and_contract_to_the_same_semantic_future() {
        let records = vec![
            said("child", &["envelope"], OUT, Some("b")),
            said("envelope", &["root"], IN, None),
            said("root", &[], IN, Some("a")),
        ];
        let (stats, mut union) = project(records, UnitAnnotations::default());
        assert_eq!(stats.projections_emitted, 2);
        assert_eq!(stats.contentless_records, 1);
        assert_eq!(stats.content_parts, 2);
        assert_eq!(blocks(&union).len(), 2);
        let root = block_with_text(&mut union, "a");
        let child = block_with_text(&mut union, "b");
        assert!(follows(&union, child, root));
    }

    #[test]
    fn tool_exhaust_sits_in_the_main_chain_and_the_next_act_descends_from_it() {
        let call = SourcePart::text(
            schema::content_fact::modality::TOOL_CALL,
            OUT,
            owned_text(r#"{"name":"ls"}"#),
        );
        let output = SourcePart::text(
            schema::content_fact::modality::TOOL_RESULT,
            IN,
            owned_text("a.txt"),
        );
        let mut act = record("act", &["root"], vec![call]);
        act.links.push((0, ToolLink::Call(owned_text("call-1"))));
        let mut result = record("result", &["act"], vec![output]);
        result
            .links
            .push((0, ToolLink::Result(owned_text("call-1"))));
        let records = vec![
            said("root", &[], IN, Some("question")),
            act,
            result,
            said("answer", &["result"], OUT, Some("answer")),
        ];
        let (stats, mut union) = project(records, UnitAnnotations::default());
        assert_eq!(stats.unresolved_tool_results, 0);
        let call_block = block_with_text(&mut union, r#"{"name":"ls"}"#);
        let result_block = block_with_text(&mut union, "a.txt");
        let answer = block_with_text(&mut union, "answer");
        assert!(follows(&union, result_block, call_block));
        assert!(follows(&union, answer, result_block));
        assert!(
            exists!(pattern!(&union, [{
                result_block @ schema::block::responds_to: call_block
            }])),
            "the result's node names the call's node"
        );
    }

    #[test]
    fn every_part_is_one_node_in_source_order_and_turns_are_derived() {
        let records = vec![
            said("prompt", &[], IN, Some("prompt")),
            record(
                "message",
                &["prompt"],
                vec![
                    part(OUT, "thinking"),
                    part(OUT, "call a"),
                    part(OUT, "call b"),
                ],
            ),
            record(
                "results",
                &["message"],
                vec![part(IN, "result a"), part(IN, "result b")],
            ),
            said("note", &["results"], IN, Some("system note")),
            said("user", &["note"], IN, Some("next prompt")),
            said("next", &["user"], OUT, Some("next act")),
            record(
                "mixed",
                &["next"],
                vec![
                    part(OUT, "searching"),
                    part(IN, "search result"),
                    part(OUT, "found it"),
                ],
            ),
        ];
        let (stats, mut union) = project(records, UnitAnnotations::default());
        assert_eq!(stats.projections_emitted, 12, "one node per part");
        let texts = [
            "prompt",
            "thinking",
            "call a",
            "call b",
            "result a",
            "result b",
            "system note",
            "next prompt",
            "next act",
            "searching",
            "search result",
            "found it",
        ];
        let nodes: Vec<Id> = texts
            .iter()
            .map(|text| block_with_text(&mut union, text))
            .collect();
        // Order is the chain alone.
        assert_eq!(blockdag::thread_to(union.facts(), nodes[11]), nodes);
        let turns = blockdag::turns_along(union.facts(), nodes[11]);
        assert_eq!(
            turns,
            vec![
                vec![nodes[0]],
                nodes[1..4].to_vec(),
                nodes[4..8].to_vec(),
                vec![nodes[8], nodes[9]],
                vec![nodes[10]],
                vec![nodes[11]],
            ],
            "a run of one direction is a turn, whatever records it came from"
        );
    }

    #[test]
    fn a_longer_source_only_appends_nodes() {
        let full = vec![
            said("q", &[], IN, Some("question")),
            record(
                "a",
                &["q"],
                vec![part(OUT, "thinking"), part(OUT, "answer")],
            ),
            said("q2", &["a"], IN, Some("follow-up")),
            said("a2", &["q2"], OUT, Some("second answer")),
        ];
        let truncated = full[..2].to_vec();
        let (_, short) = project(truncated, UnitAnnotations::default());
        let (_, long) = project(full, UnitAnnotations::default());
        let short_nodes = blocks(&short);
        let long_nodes = blocks(&long);
        assert!(short_nodes.is_subset(&long_nodes), "no earlier id changed");
        assert_eq!(long_nodes.len() - short_nodes.len(), 2, "only the tail");
        let receipts = |fragment: &Fragment| -> BTreeSet<Id> {
            find!(
                receipt: Id,
                pattern!(fragment, [{ ?receipt @ schema::source_projection::projects_to: _?node }])
            )
            .collect()
        };
        assert!(receipts(&short).is_subset(&receipts(&long)));
    }

    #[test]
    fn a_continuing_branch_yields_the_same_blocks_as_a_source_without_the_branch() {
        let with_branch = vec![
            said("q", &[], IN, Some("question")),
            said("a1", &["q"], OUT, Some("first answer")),
            said("a2", &["q"], OUT, Some("regenerated answer")),
            said("tool", &["a2"], IN, Some("exhaust")),
            said("more", &["tool"], IN, Some("more input")),
        ];
        let without_branch = vec![
            said("q", &[], IN, Some("question")),
            said("a2", &["q"], OUT, Some("regenerated answer")),
            said("tool", &["a2"], IN, Some("exhaust")),
            said("more", &["tool"], IN, Some("more input")),
        ];
        let (_, full) = project(with_branch, UnitAnnotations::default());
        let (_, lean) = project(without_branch, UnitAnnotations::default());
        assert!(blocks(&lean).is_subset(&blocks(&full)));
        assert_eq!(blocks(&full).len(), blocks(&lean).len() + 1);
    }

    #[test]
    fn provider_keys_never_reach_an_identity_or_the_pile() {
        let first = vec![
            said("conv-1/node-a", &[], IN, Some("hello")),
            said("conv-1/node-b", &["conv-1/node-a"], OUT, Some("hi")),
        ];
        let renamed = vec![
            said("other-id/x", &[], IN, Some("hello")),
            said("other-id/y", &["other-id/x"], OUT, Some("hi")),
        ];
        let unit = |id: &str| UnitAnnotations {
            source_locator: Some(id.to_owned()),
        };
        let (_, a) = project(first, unit("conv-1"));
        let (_, b) = project(renamed, unit("other-id"));
        let receipts = |fragment: &Fragment| -> BTreeSet<Id> {
            find!(
                receipt: Id,
                pattern!(fragment, [{ ?receipt @ schema::source_projection::projects_to: _?block }])
            )
            .collect()
        };
        assert_eq!(blocks(&a), blocks(&b));
        assert_eq!(receipts(&a), receipts(&b));
        // Only the conversation-level locator differs, as an annotation.
        let difference = a.facts().difference(b.facts());
        assert!(difference
            .iter()
            .all(|trible| *trible.a() == schema::source_projection::source_locator.id()));
        assert!(!exists!(
            pattern!(&a, [{ _?r @ schema::source_projection::raw_record: _?raw }])
        ));
    }

    #[test]
    fn unit_annotations_land_on_leaves_only() {
        let records = vec![
            said("root", &[], IN, Some("q")),
            said("a1", &["root"], OUT, Some("first answer")),
            said("a2", &["root"], OUT, Some("regenerated answer")),
            said("tool", &["a2"], IN, Some("exhaust")),
        ];
        let (_, mut union) = project(
            records,
            UnitAnnotations {
                source_locator: Some("conv".to_owned()),
            },
        );
        let annotated: BTreeSet<Id> = find!(
            block: Id,
            pattern!(&union, [{
                _?receipt @ schema::source_projection::source_locator: _?locator,
                schema::source_projection::projects_to: ?block
            }])
        )
        .collect();
        let expected = BTreeSet::from([
            block_with_text(&mut union, "first answer"),
            block_with_text(&mut union, "exhaust"),
        ]);
        assert_eq!(annotated, expected);
    }

    #[test]
    fn cycles_fail_before_the_first_fragment_is_emitted() {
        let records = vec![
            said("a", &["b"], IN, Some("a")),
            said("b", &["a"], IN, Some("b")),
        ];
        let mut emitted = 0;
        let error = project_records(
            schema::source_projection::SOURCE_AGY,
            Path::new("cycle.json"),
            records,
            UnitAnnotations::default(),
            |_| {
                emitted += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(emitted, 0);
        assert!(error.to_string().contains("cycle"));
    }

    #[test]
    fn shared_canonical_json_sorts_objects_and_keeps_last_duplicate() {
        let raw = Bytes::from_source(br#"{ "b": [true, null], "a": 1, "a": 2 }"#.to_vec());
        assert_eq!(canonical_json(raw).unwrap(), r#"{"a":2,"b":[true,null]}"#);
        assert_eq!(
            canonical_json_string("quote: \"; slash: \\; line:\n"),
            r#""quote: \"; slash: \\; line:\n""#
        );
    }
}
