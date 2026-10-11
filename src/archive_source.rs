//! Shared zero-copy source-adapter substrate for Archive transcript importers.
//!
//! Source adapters own only format knowledge: they scan immutable bytes into
//! [`SourceRecord`]s whose strings remain views of the source allocation
//! while projecting; no source bytes reach the pile. This module owns source-DAG validation, transparent-node
//! contraction, content construction, source receipts, and callback emission.
//! No adapter needs a dynamic JSON tree, repository head, branch, or cursor.

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

/// Whether an occurrence participates in the canonical dialogue DAG.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Threading {
    /// The projected block is a semantic predecessor for source children.
    #[default]
    Semantic,
    /// A side branch: the record's block follows its nearest semantic
    /// ancestors, but the record is contracted out of its children's
    /// predecessor paths. Tool exhaust, hidden harness context, and envelope
    /// nodes use it, so a source that omits them (a newer export format, a
    /// replay that skips tool records) still threads the same dialogue blocks.
    Transparent,
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
    /// External asset, optionally accompanied by recovered bytes.
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
    /// Whether children see this record in their semantic predecessor frontier.
    pub threading: Threading,
    /// Ordered semantic content. A record without content projects nothing
    /// and is contracted out of its children's predecessor paths.
    pub parts: Vec<SourcePart>,
    /// Nonidentity source claims.
    pub claims: SourceClaims,
}

/// One source-only fragment emitted to an Archive import writer.
pub struct ProjectedSource {
    pub source_path: PathBuf,
    pub fragment: Fragment,
    /// Key of the record this receipt projects, for diagnostics and tests.
    /// `None` on a leaf annotation. It is never stored.
    pub record: Option<View<str>>,
}

/// Common accounting across source adapters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectionStats {
    pub records_seen: usize,
    pub projections_emitted: usize,
    pub content_parts: usize,
    pub transparent_records: usize,
    pub contentless_records: usize,
    pub missing_predecessors: usize,
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
/// They land on the unit's leaves only (the blocks no other record of the unit
/// follows), never on every message, and never in an identity.
#[derive(Clone, Debug, Default)]
pub struct UnitAnnotations {
    /// Provider name of the conversation (conversation, session or file id).
    pub source_locator: Option<String>,
    /// Provider title of the conversation.
    pub name: Option<String>,
}

/// Project a complete source graph through the canonical block-DAG model.
///
/// The graph is planned before fragments are emitted. Output is independent of
/// source iteration order, and transparent nodes are contracted by the set of
/// nearest semantic ancestors rather than historical path. Every receipt is a
/// [`blockdag::block_source_projection`]: provider keys thread the graph while
/// projecting and are never stored.
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

    let mut ready: Vec<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect();
    sort_ready(&mut ready, &records);
    let mut order = Vec::with_capacity(records.len());
    while let Some(index) = ready.pop() {
        order.push(index);
        for &child in &children[index] {
            indegree[child] -= 1;
            if indegree[child] == 0 {
                ready.push(child);
                sort_ready(&mut ready, &records);
            }
        }
    }
    if order.len() != records.len() {
        bail!("source predecessor graph contains a cycle");
    }

    let mut semantic_frontiers = vec![BTreeSet::<usize>::new(); records.len()];
    let mut block_ids = vec![None::<Id>; records.len()];
    let mut receipt_ids = vec![None::<Id>; records.len()];
    let mut followed = BTreeSet::<Id>::new();

    for index in order {
        let record = &records[index];
        let mut predecessor_indices = BTreeSet::new();
        for predecessor in &record.predecessors {
            if let Some(&parent) = by_locator.get(predecessor) {
                predecessor_indices.extend(semantic_frontiers[parent].iter().copied());
            }
        }
        if record.parts.is_empty() {
            // Nothing to store: the record only forwards its ancestors.
            stats.contentless_records += 1;
            stats.transparent_records += 1;
            semantic_frontiers[index] = predecessor_indices;
            continue;
        }

        let mut parts = Fragment::empty();
        for (ordinal, part) in record.parts.iter().cloned().enumerate() {
            let ordinal = u64::try_from(ordinal).context("content-part ordinal exceeds u64")?;
            let (fact, resolution) = build_fact(part)?;
            parts += blockdag::content_part(ordinal, fact, None)?;
            if let Some(resolution) = resolution {
                // Facts and bytes only: the fact must not become an export
                // of the parts the block contains.
                let (_, facts, metafacts, blobs) = resolution.into_parts();
                parts += Fragment::from_parts(facts, metafacts, blobs);
            }
            stats.content_parts += 1;
        }
        let predecessors: BTreeSet<Id> = predecessor_indices
            .iter()
            .map(|parent| block_ids[*parent].expect("semantic frontier contains a projected block"))
            .collect();
        if record.threading == Threading::Semantic {
            followed.extend(predecessors.iter().copied());
        }
        let block = blockdag::block(predecessors, record.block_timestamp, parts)?;
        let block_id = block.root().expect("canonical block is rooted");
        let receipt = blockdag::block_source_projection(source_namespace, block)?;
        let receipt = blockdag::annotate_source_projection(
            receipt,
            ProjectionAnnotations {
                source_timestamp: record.claims.timestamp,
                author: record.claims.author,
                experiencer: record.claims.experiencer,
                raw_author: record
                    .claims
                    .raw_author
                    .as_ref()
                    .map(|value| value.as_ref().to_owned()),
                raw_role: record
                    .claims
                    .raw_role
                    .as_ref()
                    .map(|value| value.as_ref().to_owned()),
                raw_model: record
                    .claims
                    .raw_model
                    .as_ref()
                    .map(|value| value.as_ref().to_owned()),
                ..ProjectionAnnotations::default()
            },
        )?;
        receipt_ids[index] = Some(receipt.root().expect("canonical receipt is rooted"));
        emit(ProjectedSource {
            source_path: source_path.to_path_buf(),
            fragment: receipt,
            record: Some(record.locator.clone()),
        })?;

        block_ids[index] = Some(block_id);
        if record.threading == Threading::Semantic {
            semantic_frontiers[index].insert(index);
        } else {
            stats.transparent_records += 1;
            semantic_frontiers[index] = predecessor_indices;
        }
        stats.projections_emitted += 1;
    }

    // Conversation-level provenance lands on the unit's leaves only.
    let mut annotated = BTreeSet::new();
    for ((record, block), receipt) in records.iter().zip(&block_ids).zip(&receipt_ids) {
        let (Some(block), Some(receipt)) = (block, receipt) else {
            continue;
        };
        if record.threading != Threading::Semantic {
            continue;
        }
        if followed.contains(block) || !annotated.insert(*receipt) {
            continue;
        }
        emit(ProjectedSource {
            source_path: source_path.to_path_buf(),
            fragment: blockdag::source_occurrence(
                *receipt,
                ProjectionAnnotations {
                    source_path: Some(source_path.display().to_string()),
                    source_locator: unit.source_locator.clone(),
                    name: unit.name.clone(),
                    ..ProjectionAnnotations::default()
                },
            )?,
            record: None,
        })?;
    }

    Ok(stats)
}

fn sort_ready(ready: &mut [usize], records: &[SourceRecord]) {
    ready.sort_by(|left, right| {
        records[*right]
            .locator
            .as_ref()
            .cmp(records[*left].locator.as_ref())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        locator: &str,
        predecessors: &[&str],
        threading: Threading,
        text: Option<&str>,
    ) -> SourceRecord {
        SourceRecord {
            locator: owned_text(locator),
            predecessors: predecessors.iter().copied().map(owned_text).collect(),
            block_timestamp: None,
            threading,
            parts: text
                .map(|value| {
                    vec![SourcePart::text(
                        schema::content_fact::modality::TEXT,
                        schema::content_fact::direction::AMBIENT,
                        owned_text(value),
                    )]
                })
                .unwrap_or_default(),
            claims: SourceClaims::default(),
        }
    }

    fn projected_block(fragment: &Fragment) -> Id {
        let projection = fragment.root().expect("projection has one root");
        find!(
            (block: Id),
            pattern!(fragment, [{
                projection @ schema::source_projection::projects_to: ?block
            }])
        )
        .next()
        .map(|(block,)| block)
        .expect("projection names one block")
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
                { ?block @ schema::block::contains: _?part },
                { _?part @ schema::content_part::fact: _?fact },
                { _?fact @ schema::content_fact::payload: handle },
            ])
        )
        .next()
        .expect("a block carries the text")
    }

    #[test]
    fn contentless_nodes_store_nothing_and_contract_to_the_same_semantic_future() {
        let records = vec![
            record("child", &["envelope"], Threading::Semantic, Some("b")),
            record("envelope", &["root"], Threading::Transparent, None),
            record("root", &[], Threading::Semantic, Some("a")),
        ];
        let (stats, mut union) = project(records, UnitAnnotations::default());
        assert_eq!(stats.projections_emitted, 2);
        assert_eq!(stats.contentless_records, 1);
        assert_eq!(stats.content_parts, 2);
        assert_eq!(blocks(&union).len(), 2);
        let root = block_with_text(&mut union, "a");
        let child = block_with_text(&mut union, "b");
        assert!(exists!(
            pattern!(&union, [{ child @ schema::block::previous: root }])
        ));
    }

    #[test]
    fn side_branches_keep_their_place_without_moving_the_dialogue() {
        let with_tool = vec![
            record("root", &[], Threading::Semantic, Some("question")),
            record(
                "tool",
                &["root"],
                Threading::Transparent,
                Some("tool output"),
            ),
            record("answer", &["tool"], Threading::Semantic, Some("answer")),
        ];
        let without_tool = vec![
            record("root", &[], Threading::Semantic, Some("question")),
            record("answer", &["root"], Threading::Semantic, Some("answer")),
        ];
        let (_, mut full) = project(with_tool, UnitAnnotations::default());
        let (_, mut lean) = project(without_tool, UnitAnnotations::default());
        let root = block_with_text(&mut full, "question");
        let tool = block_with_text(&mut full, "tool output");
        assert!(exists!(
            pattern!(&full, [{ tool @ schema::block::previous: root }])
        ));
        // The answer is the same block whether or not the source kept the
        // tool exhaust between question and answer.
        assert_eq!(
            block_with_text(&mut full, "answer"),
            block_with_text(&mut lean, "answer")
        );
        assert!(blocks(&lean).is_subset(&blocks(&full)));
    }

    #[test]
    fn provider_keys_never_reach_an_identity_or_the_pile() {
        let first = vec![
            record("conv-1/node-a", &[], Threading::Semantic, Some("hello")),
            record(
                "conv-1/node-b",
                &["conv-1/node-a"],
                Threading::Semantic,
                Some("hi"),
            ),
        ];
        let renamed = vec![
            record("other-id/x", &[], Threading::Semantic, Some("hello")),
            record(
                "other-id/y",
                &["other-id/x"],
                Threading::Semantic,
                Some("hi"),
            ),
        ];
        let unit = |id: &str| UnitAnnotations {
            source_locator: Some(id.to_owned()),
            name: Some("Greeting".to_owned()),
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
    fn unit_annotations_land_on_semantic_leaves_only() {
        let records = vec![
            record("root", &[], Threading::Semantic, Some("q")),
            record("a1", &["root"], Threading::Semantic, Some("first answer")),
            record(
                "a2",
                &["root"],
                Threading::Semantic,
                Some("regenerated answer"),
            ),
            record("tool", &["a2"], Threading::Transparent, Some("exhaust")),
        ];
        let (_, union) = project(
            records,
            UnitAnnotations {
                source_locator: Some("conv".to_owned()),
                name: None,
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
        assert_eq!(annotated.len(), 2, "the two answers are the leaves");
    }

    #[test]
    fn cycles_fail_before_the_first_fragment_is_emitted() {
        let records = vec![
            record("a", &["b"], Threading::Semantic, Some("a")),
            record("b", &["a"], Threading::Semantic, Some("b")),
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
