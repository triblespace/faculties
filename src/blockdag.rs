//! Canonical constructors for the Archive block DAG.
//!
//! Each constructor returns a self-contained [`Fragment`]. Child fragments
//! are spread into their parent, consuming the child's exports while retaining
//! all facts and blobs. The resulting root therefore follows the data model:
//!
//! ```text
//! content fact(s) -> node (`block`) -> source projection
//! ```
//!
//! A node holds one fact, or the unordered set of facts that happened at one
//! moment, and follows its previous node(s); order is that chain alone.
//!
//! Kind markers and source annotations are attached only after intrinsic
//! construction. They remain queryable without becoming hidden inputs to a
//! content-derived id.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Result};
use triblespace::core::blob::encodings::UnknownBlob;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::{Handle, NsTAIInterval};
use triblespace::prelude::*;

use crate::files;
use crate::schemas::blockdag as schema;

/// Source-occurrence evidence that must not affect the projected block or
/// projection-receipt identity.
///
/// Every field is additive. `None` means that the source supplied no usable
/// evidence; it never inserts a sentinel value.
///
/// Every field comes from the session's own content, so it is the same in
/// every copy of the session, read from any folder, on any host, by any
/// build. Nothing that only describes the machinery around the session (the
/// file's path, a provider's title for the conversation, the importer's
/// revision) is an annotation.
#[derive(Clone, Debug, Default)]
pub struct ProjectionAnnotations {
    /// Source receipts that additively support the projected block's semantic
    /// predecessor classes. Importers leave it empty: their receipts are the
    /// namespace and the block, so the support is derivable from the block's
    /// predecessors.
    pub semantic_predecessor_support: Vec<Id>,
    /// Genuine source timestamp claim, if independently decodable.
    pub source_timestamp: Option<Inline<NsTAIInterval>>,
    /// Stable Relations entity that produced this occurrence.
    pub author: Option<Id>,
    /// Stable Relations entity whose stream observed or produced it.
    pub experiencer: Option<Id>,
    /// Exact source spelling of the author field.
    pub raw_author: Option<String>,
    /// Exact source spelling of the role field.
    pub raw_role: Option<String>,
    /// Exact source spelling of the model field.
    pub raw_model: Option<String>,
    /// Models that wrote this occurrence ([`vendor_model`] roots for
    /// imported records, the producer's binding for native ones). Leave it
    /// empty for anything the model perceived rather than produced.
    pub generated_by: Vec<Id>,
    /// Provider name of the conversation this receipt belongs to (a session,
    /// rollout or conversation id). Importers attach it to the leaves of one
    /// source unit only; it never enters an identity.
    pub source_locator: Option<String>,
    /// Canonical JSON of provider metadata about that conversation.
    pub source_detail: Option<String>,
    /// The agent name the source's own session metadata gives that
    /// conversation, exactly as spelled there (Claude Code `agent-name`,
    /// Codex `agent_nickname`). Leaves only; never from a persona.
    pub raw_agent_name: Option<String>,
    /// The provider thread the leaf ends, as `thread/<id>`, where one session
    /// holds several threads (Codex subagents share their root's session).
    pub source_thread: Option<String>,
    /// Exact source label of who started that thread (Codex `thread_source`).
    pub raw_thread_source: Option<String>,
    /// Exact source agent path of that thread (Codex `agent_path`).
    pub raw_agent_path: Option<String>,
}

fn rooted(fragment: &Fragment, what: &str) -> Result<Id> {
    fragment
        .root()
        .ok_or_else(|| anyhow!("{what} must export exactly one root"))
}

fn attach_kind(mut fragment: Fragment, kind: Id, what: &str) -> Result<Fragment> {
    let root = rooted(&fragment, what)?;
    fragment += entity! { ExclusiveId::force_ref(&root) @ metadata::tag: &kind };
    Ok(fragment)
}

/// Queryable names for the reified Archive modality and direction tags.
///
/// These facts are ordinary collection data rather than renderer knowledge.
/// Repeating them in authored imports is harmless set duplication, and lets a
/// reader display a newly introduced tag without adding another match table.
pub fn vocabulary_fragment() -> Fragment {
    let mut fragment = Fragment::empty();
    for &(id, name) in schema::content_fact::modality::SPECS
        .iter()
        .chain(schema::content_fact::direction::SPECS)
    {
        let name = fragment.put::<UTF8String, _>(name.to_owned());
        fragment += entity! { ExclusiveId::force_ref(&id) @
            metadata::tag: &metadata::KIND_TAG,
            metadata::name: name,
        };
    }
    fragment
}

/// The model a source names, as an intrinsic entity rooted in the returned
/// fragment: `{ metadata::tag: vendor_model::KIND, metadata::name: label }`,
/// the label exactly as the source spelled it. Equal labels converge on one
/// entity; that records equal spelling, not shown equal weights (see
/// [`schema::vendor_model`]).
pub fn vendor_model(label: &str) -> Fragment {
    let mut fragment = Fragment::empty();
    let name = fragment.put::<UTF8String, _>(label.to_owned());
    fragment += entity! {
        metadata::tag: &schema::vendor_model::KIND,
        metadata::name: name,
    };
    fragment
}

/// Build-independent descriptions of the Archive's own attributes, with the
/// blobs they name: each attribute's identity and value encoding, without
/// the attribute-usage annotations (their ids derive from the Rust module
/// path, and they carry the doc comment as text).
///
/// The import writer publishes them as the vocabulary COMMIT's metadata and
/// gives every piece empty metadata, so a rebuilt cohort, whatever its doc
/// comments and module layout, signs the same pieces again. No reader takes
/// attribute descriptions from a commit's metadata.
pub fn attribute_descriptions() -> (TribleSet, Vec<triblespace::core::blob::Blob<UnknownBlob>>) {
    let mut described = schema::block::describe();
    described += schema::content_fact::describe();
    described += schema::source_projection::describe();
    described += schema::library_file::describe();
    let (_, facts, metafacts, mut blobs) = described.into_parts();
    let mut all = facts;
    all += metafacts;
    let module = metadata::source_module.id();
    let description = metadata::description.id();
    let usages: BTreeSet<Id> = all
        .iter()
        .filter(|fact| *fact.a() == module)
        .map(|fact| *fact.e())
        .collect();
    let mut kept = TribleSet::new();
    let mut named = BTreeSet::new();
    for fact in all.iter() {
        if usages.contains(fact.e()) || *fact.a() == description {
            continue;
        }
        named.insert(fact.v::<Handle<UnknownBlob>>().raw);
        kept.insert(fact);
    }
    let reader = blobs
        .snapshot()
        .expect("MemoryBlobStore reader creation is infallible");
    let mut kept_blobs: Vec<_> = reader
        .iter()
        .filter(|(handle, _)| named.contains(&handle.raw))
        .collect();
    kept_blobs.sort_unstable_by_key(|(handle, _)| handle.raw);
    (kept, kept_blobs.into_iter().map(|(_, blob)| blob).collect())
}

/// Construct one intrinsic textual content fact.
pub fn text_fact(modality: Id, direction: Id, text: impl Into<String>) -> Result<Fragment> {
    text_fact_blob(modality, direction, text.into())
}

/// Construct one intrinsic textual fact while retaining a source-backed view.
pub fn text_fact_view(modality: Id, direction: Id, text: anybytes::View<str>) -> Result<Fragment> {
    text_fact_blob(modality, direction, text)
}

fn text_fact_blob<T>(modality: Id, direction: Id, text: T) -> Result<Fragment>
where
    T: triblespace::core::blob::IntoBlob<UTF8String>,
{
    let mut fragment = Fragment::empty();
    let payload = fragment.put::<UTF8String, _>(text);
    fragment += entity! { _ @
        schema::content_fact::modality: &modality,
        schema::content_fact::direction: &direction,
        schema::content_fact::payload: payload,
    };
    attach_kind(fragment, schema::content_fact::KIND, "content fact")
}

/// Construct one intrinsic resident binary content fact.
///
/// The media type is normalized by Files and represented by the same
/// intrinsic media-type entity used by canonical file records.
pub fn blob_fact<T>(modality: Id, direction: Id, bytes: T, media_type: &str) -> Result<Fragment>
where
    T: triblespace::core::blob::IntoBlob<RawBytes>,
{
    let mut fragment = Fragment::empty();
    let blob = fragment.put::<RawBytes, _>(bytes);
    let media_type = files::media_type_fragment(media_type)?;
    fragment += entity! { _ @
        schema::content_fact::modality: &modality,
        schema::content_fact::direction: &direction,
        schema::content_fact::blob: blob,
        schema::content_fact::media_type*: media_type,
    };
    attach_kind(fragment, schema::content_fact::KIND, "content fact")
}

/// Construct one intrinsic unresolved external-asset fact.
///
/// A pointer is always scoped by its source namespace. Media type and claimed
/// byte size participate only when genuinely known.
pub fn asset_pointer_fact(
    modality: Id,
    direction: Id,
    asset_namespace: Id,
    pointer: impl Into<String>,
    media_type: Option<&str>,
    size: Option<u128>,
) -> Result<Fragment> {
    asset_pointer_fact_blob(
        modality,
        direction,
        asset_namespace,
        pointer.into(),
        media_type,
        size,
    )
}

/// Construct an unresolved asset fact from a source-backed pointer view.
pub fn asset_pointer_fact_view(
    modality: Id,
    direction: Id,
    asset_namespace: Id,
    pointer: anybytes::View<str>,
    media_type: Option<&str>,
    size: Option<u128>,
) -> Result<Fragment> {
    asset_pointer_fact_blob(
        modality,
        direction,
        asset_namespace,
        pointer,
        media_type,
        size,
    )
}

fn asset_pointer_fact_blob<P>(
    modality: Id,
    direction: Id,
    asset_namespace: Id,
    pointer: P,
    media_type: Option<&str>,
    size: Option<u128>,
) -> Result<Fragment>
where
    P: triblespace::core::blob::IntoBlob<UTF8String>,
{
    let mut fragment = Fragment::empty();
    let pointer = fragment.put::<UTF8String, _>(pointer);
    let media_type = media_type
        .map(files::media_type_fragment)
        .transpose()?
        .unwrap_or_else(Fragment::empty);
    fragment += entity! { _ @
        schema::content_fact::modality: &modality,
        schema::content_fact::direction: &direction,
        schema::content_fact::asset_namespace: &asset_namespace,
        schema::content_fact::asset_pointer: pointer,
        schema::content_fact::media_type*: media_type,
        schema::content_fact::asset_size?: size,
    };
    attach_kind(fragment, schema::content_fact::KIND, "content fact")
}

/// Add one recovered byte representation to a pointer-identified content fact.
///
/// The intrinsic fact root remains unchanged. Multiple distinct resolutions
/// are all retained, for a reader to choose among, rather than resolved by
/// last-write-wins.
pub fn resolve_pointer_fact<T>(mut fact: Fragment, bytes: T) -> Result<Fragment>
where
    T: triblespace::core::blob::IntoBlob<RawBytes>,
{
    let root = rooted(&fact, "content fact")?;
    let resolved = fact.put::<RawBytes, _>(bytes);
    fact += entity! { ExclusiveId::force_ref(&root) @
        schema::content_fact::resolved_to: resolved,
    };
    Ok(fact)
}

/// Add one recovered body the source itself labels as its sanitized copy.
///
/// The body is a resolution like any other (`resolved_to`), and also
/// `sanitized_to`, so [`preferred_resolution`] ranks it below an unlabelled
/// body. The intrinsic fact root remains unchanged.
pub fn resolve_sanitized_pointer_fact<T>(mut fact: Fragment, bytes: T) -> Result<Fragment>
where
    T: triblespace::core::blob::IntoBlob<RawBytes>,
{
    let root = rooted(&fact, "content fact")?;
    let resolved = fact.put::<RawBytes, _>(bytes);
    fact += entity! { ExclusiveId::force_ref(&root) @
        schema::content_fact::resolved_to: resolved,
        schema::content_fact::sanitized_to: resolved,
    };
    Ok(fact)
}

/// Annotate a pointer fact as a rendered page of another asset.
///
/// `document_pointer` is the document's own pointer in the same namespace, so
/// it is stored as the same text blob the document's fact uses, and a reader
/// joins on it. `page` is the number as the source spells it. Annotation
/// only: the fact keeps its id, and no bytes are claimed for the page.
pub fn page_of_pointer_fact<P>(
    mut fact: Fragment,
    document_pointer: P,
    page: u128,
) -> Result<Fragment>
where
    P: triblespace::core::blob::IntoBlob<UTF8String>,
{
    let root = rooted(&fact, "content fact")?;
    let document = fact.put::<UTF8String, _>(document_pointer);
    fact += entity! { ExclusiveId::force_ref(&root) @
        schema::content_fact::page_of: document,
        schema::content_fact::page: page,
    };
    Ok(fact)
}

/// The resolution a reader shows when a pointer fact has several.
///
/// Every `resolved_to` body stays a resolution; this only orders them, by a
/// fixed rule, so every reader of the same facts picks the same body:
///
/// 1. a body the source did not label as sanitized (`sanitized_to`);
/// 2. a body that `body` can read;
/// 3. a body that still carries embedded EXIF metadata ([`carries_exif`]),
///    because a provider's sanitized copy is one with that metadata
///    stripped (in the ChatGPT exports the older, stripped copy is often not
///    labelled);
/// 4. the larger body;
/// 5. the smallest handle.
///
/// `None` when the fact has no resolution.
pub fn preferred_resolution<P, F>(
    facts: &P,
    fact: Id,
    mut body: F,
) -> Option<Inline<Handle<RawBytes>>>
where
    P: TriblePattern,
    F: FnMut(Inline<Handle<RawBytes>>) -> Option<anybytes::Bytes>,
{
    let sanitized: BTreeSet<[u8; 32]> = find!(
        handle: Inline<Handle<RawBytes>>,
        pattern!(facts, [{ fact @ schema::content_fact::sanitized_to: ?handle }])
    )
    .map(|handle| handle.raw)
    .collect();
    let candidates: BTreeSet<[u8; 32]> = find!(
        handle: Inline<Handle<RawBytes>>,
        pattern!(facts, [{ fact @ schema::content_fact::resolved_to: ?handle }])
    )
    .map(|handle| handle.raw)
    .collect();
    candidates
        .into_iter()
        .map(|raw| {
            let handle = Inline::<Handle<RawBytes>>::new(raw);
            let bytes = body(handle);
            let rank = (
                !sanitized.contains(&raw),
                bytes.is_some(),
                bytes.as_deref().is_some_and(carries_exif),
                bytes.as_ref().map_or(0, |bytes| bytes.len()),
                std::cmp::Reverse(raw),
            );
            (rank, handle)
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, handle)| handle)
}

/// Whether an image carries an embedded EXIF block: a JPEG `APP1` segment
/// starting `Exif\0\0`, a PNG `eXIf` chunk, or a WebP `EXIF` chunk. The
/// container is walked by its own segment lengths; anything else, or a
/// truncated container, is `false`.
pub fn carries_exif(bytes: &[u8]) -> bool {
    if let Some(mut rest) = bytes.strip_prefix(&[0xFF, 0xD8]) {
        while let [0xFF, marker, tail @ ..] = rest {
            match *marker {
                // Fill bytes before a marker.
                0xFF => rest = &rest[1..],
                // Start of scan or end of image: no more metadata segments.
                0xDA | 0xD9 => return false,
                // Markers without a length.
                0x01 | 0xD0..=0xD7 => rest = tail,
                marker => {
                    let [high, low, ..] = tail else { return false };
                    let length = usize::from(u16::from_be_bytes([*high, *low]));
                    let Some(segment) = tail.get(2..length) else {
                        return false;
                    };
                    if marker == 0xE1 && segment.starts_with(b"Exif\0\0") {
                        return true;
                    }
                    rest = &tail[length..];
                }
            }
        }
        return false;
    }
    if let Some(mut rest) = bytes.strip_prefix(b"\x89PNG\r\n\x1a\n".as_slice()) {
        while rest.len() >= 12 {
            let length = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            let kind = &rest[4..8];
            if kind == b"eXIf" {
                return true;
            }
            if kind == b"IEND" {
                return false;
            }
            let Some(next) = length.checked_add(12).and_then(|end| rest.get(end..)) else {
                return false;
            };
            rest = next;
        }
        return false;
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        let mut rest = &bytes[12..];
        while rest.len() >= 8 {
            let length = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
            if &rest[..4] == b"EXIF" {
                return true;
            }
            let padded = length + (length & 1);
            let Some(next) = padded.checked_add(8).and_then(|end| rest.get(end..)) else {
                return false;
            };
            rest = next;
        }
    }
    false
}

/// Construct one intrinsic node from structural predecessors, an optional
/// genuine event interval, and the exported content facts it holds.
///
/// A node holds one fact, or an unordered set of facts the source says
/// happened at the same moment; `facts` must export them. Order is the
/// `previous` chain alone. The unique predecessor-free, timeless, content-free
/// node is the bottom projection used by exact source receipts that yielded no
/// semantic content.
pub fn block(
    predecessors: impl IntoIterator<Item = Id>,
    timestamp: Option<Inline<NsTAIInterval>>,
    facts: Fragment,
) -> Result<Fragment> {
    block_parts(predecessors, timestamp, facts).map(|(fragment, _)| fragment)
}

/// The identity half of what an importer stages, named where it is built:
/// the nodes, and the facts their ids are derived from with their kind tags.
/// Everything else in the staged fragment annotates them.
///
/// Only [`block_parts`] makes a node's identity; [`Identity::combine`] joins
/// the identities of several nodes one fragment holds, and
/// [`Identity::none`] is the identity of a fragment that holds no node.
#[derive(Clone, Debug)]
pub struct Identity {
    /// The nodes whose identity this is; the import writer ends a piece on
    /// them.
    nodes: Vec<Id>,
    /// The facts of the nodes' intrinsic cores, the content facts they hold
    /// (with the entities those name, such as a media type), and the kind
    /// tags of all of them.
    facts: TribleSet,
}

impl Identity {
    /// The identity of a fragment that holds no node: only annotations.
    pub fn none() -> Self {
        Self {
            nodes: Vec::new(),
            facts: TribleSet::new(),
        }
    }

    /// Join another node's identity: one fragment that holds several nodes.
    pub fn combine(&mut self, other: Identity) {
        self.nodes.extend(other.nodes);
        self.facts += other.facts;
    }

    /// The nodes this identity names.
    pub fn nodes(&self) -> &[Id] {
        &self.nodes
    }

    /// The facts of their identities.
    pub fn facts(&self) -> &TribleSet {
        &self.facts
    }

    /// The facts, consumed.
    pub fn into_facts(self) -> TribleSet {
        self.facts
    }
}

/// What an export's file library states about one file, all of it annotation.
#[derive(Clone, Debug, Default)]
pub struct LibraryFileAnnotations {
    pub name: Option<anybytes::View<str>>,
    pub media_type: Option<String>,
    /// The pointer a message would name the file by, in the same namespace.
    pub asset_pointer: Option<anybytes::View<str>>,
    pub recorded_at: Option<Inline<NsTAIInterval>>,
    pub source_locator: Option<String>,
    pub generation: Option<anybytes::View<str>>,
    pub origin_message: Option<anybytes::View<str>>,
}

/// A file a source's library holds ([`schema::library_file`]), with its
/// [`Identity`]: the namespace and the bytes. It names no node.
pub fn library_file<T>(
    namespace: Id,
    bytes: T,
    annotations: LibraryFileAnnotations,
) -> Result<(Fragment, Identity)>
where
    T: triblespace::core::blob::IntoBlob<RawBytes>,
{
    let mut fragment = Fragment::empty();
    let blob = fragment.put::<RawBytes, _>(bytes);
    fragment += entity! { _ @
        metadata::tag: &schema::library_file::KIND,
        schema::source_projection::source_namespace: &namespace,
        schema::content_fact::blob: blob,
    };
    let root = rooted(&fragment, "library file")?;
    let identity = Identity {
        nodes: Vec::new(),
        facts: fragment.facts().clone(),
    };
    let pointer = annotations
        .asset_pointer
        .map(|pointer| fragment.put::<UTF8String, _>(pointer));
    let pointer_namespace = pointer.map(|_| namespace);
    let name = annotations
        .name
        .map(|name| fragment.put::<UTF8String, _>(name));
    let locator = annotations
        .source_locator
        .map(|locator| fragment.put::<UTF8String, _>(locator));
    let generation = annotations
        .generation
        .map(|generation| fragment.put::<UTF8String, _>(generation));
    let origin_message = annotations
        .origin_message
        .map(|message| fragment.put::<UTF8String, _>(message));
    let media_type = annotations
        .media_type
        .as_deref()
        .map(files::media_type_fragment)
        .transpose()?
        .unwrap_or_else(Fragment::empty);
    fragment += entity! { ExclusiveId::force_ref(&root) @
        metadata::name?: name,
        schema::content_fact::media_type*: media_type,
        schema::content_fact::asset_namespace?: pointer_namespace.as_ref(),
        schema::content_fact::asset_pointer?: pointer,
        schema::source_projection::source_timestamp?: annotations.recorded_at,
        schema::source_projection::source_locator?: locator,
        schema::library_file::generation?: generation,
        schema::library_file::origin_message?: origin_message,
    };
    Ok((fragment, identity))
}

/// [`block`], returning the node's fragment with its [`Identity`]: the
/// intrinsic core (`previous`, `holds` and the held content facts as their
/// builders made them) and the node's kind tag. The timestamp is the only
/// annotation `block` adds.
///
/// `facts` must be content facts as [`text_fact`], [`blob_fact`] and the
/// pointer builders return them: their whole fragment is identity. An
/// annotation of a content fact, such as [`resolve_pointer_fact`]'s
/// resolution of a pointer built from `Fragment::rooted`, is added to the
/// node afterwards and so stays outside the identity.
pub fn block_parts(
    predecessors: impl IntoIterator<Item = Id>,
    timestamp: Option<Inline<NsTAIInterval>>,
    facts: Fragment,
) -> Result<(Fragment, Identity)> {
    let predecessors: Vec<_> = predecessors.into_iter().collect();
    if facts.exports().next().is_none() && (!predecessors.is_empty() || timestamp.is_some()) {
        bail!("a content-free node must be the predecessor-free, timeless canonical bottom");
    }
    // IDENTITY IS `previous` + CONTENT. The timestamp is an ANNOTATION on that
    // identity, not part of it.
    //
    // It used to be in the core, for a good reason: first blocks have no
    // `previous`, so without a timestamp two conversations opening with the
    // same content collide at the root. But that assumed the timestamp is a
    // property of the EVENT. For Codex it is a property of WHEN THE FILE WAS
    // WRITTEN, and it changes on every reload.
    //
    // MEASURED 2026-09-01, two consecutive rollouts of one resumed session:
    // three byte-identical payloads, ZERO with the same timestamp — each
    // restamped to the replay moment (13:12:16.185Z -> 14:03:56.153Z), and two
    // events originally 1 ms apart collapsed onto the SAME new value, so the
    // original timing is destroyed rather than shifted. Identical text
    // therefore hashed to different blocks, and the replayed prefix was
    // re-stored: 872 KB of source added 1238 KB of pile. With ONE 2026-08
    // conversation carrying 622 rollouts, that is one conversation stored
    // several hundred times.
    //
    // Collapsing is also the TRUER statement. A replayed block is the same
    // block observed again; storing it 622 times asserts 622 distinct events.
    // Nothing is lost by collapsing, because multiplicity lives where it is
    // actually true: every occurrence adds its own `source_timestamp` to the
    // receipt, so a query joining on time enumerates them all. "The
    // timestamps unfold them."
    //
    // Root collisions become correct rather than hazardous: two conversations
    // that genuinely open with identical content DO share that opening, and
    // diverge at the first block that differs. This mirrors what Memory
    // already does — `observed_at` is a BTreeSet in `annotate_chunk`, never in
    // `chunk_core` ("genuine creation/import observations, outside intrinsic
    // state").
    let core = entity! { _ @
        schema::block::previous*: predecessors.iter(),
        schema::block::holds*: facts,
    };
    let root = rooted(&core, "block")?;
    let mut fragment = attach_kind(core, schema::block::KIND, "block")?;
    let identity = Identity {
        nodes: vec![root],
        facts: fragment.facts().clone(),
    };
    fragment += entity! { ExclusiveId::force_ref(&root) @
        schema::block::timestamp?: timestamp,
    };
    debug_assert_eq!(fragment.root(), Some(root));
    Ok((fragment, identity))
}

/// Annotate a result node with the node of the tool call it answers.
pub fn responds_to(node: Id, call: Id) -> Fragment {
    Fragment::from_facts_and_blobs(
        entity! { ExclusiveId::force_ref(&node) @ schema::block::responds_to: &call }.into_facts(),
        Default::default(),
    )
}

/// Annotate a source receipt with the exact body its source selected for the
/// pointer fact its node holds.
///
/// Annotation only: the receipt, its node and the fact keep their ids. The
/// body must be among the fact's own `resolved_to` values, which the reader
/// checks; the bytes travel with the fact.
pub fn select_resolution(receipt: Id, body: Inline<Handle<RawBytes>>) -> Fragment {
    Fragment::from_facts_and_blobs(
        entity! { ExclusiveId::force_ref(&receipt) @
            schema::source_projection::resolution: body,
        }
        .into_facts(),
        Default::default(),
    )
}

/// The directions of the facts a node holds.
pub fn node_directions<P: TriblePattern>(facts: &P, node: Id) -> BTreeSet<Id> {
    find!(
        direction: Id,
        pattern!(facts, [
            { node @ schema::block::holds: _?fact },
            { _?fact @ schema::content_fact::direction: ?direction },
        ])
    )
    .collect()
}

/// The thread that ends at `leaf`, root first. Where a node follows several
/// predecessors, the thread continues through the smallest id.
pub fn thread_to<P: TriblePattern>(facts: &P, leaf: Id) -> Vec<Id> {
    let mut thread = Vec::new();
    let mut seen = BTreeSet::new();
    let mut cursor = Some(leaf);
    while let Some(node) = cursor {
        if !seen.insert(node) {
            break;
        }
        thread.push(node);
        cursor = find!(
            previous: Id,
            pattern!(facts, [{ node @ schema::block::previous: ?previous }])
        )
        .min();
    }
    thread.reverse();
    thread
}

/// The thread ending at `leaf` as turns: each a maximal run of consecutive
/// nodes whose facts have one and the same direction. An act is a run of
/// `out` nodes and a perception a run of `in` nodes; a node holding facts of
/// both directions at once is a turn of its own. This is the view of acts
/// and perceptions; it is derived, never stored.
pub fn turns_along<P: TriblePattern>(facts: &P, leaf: Id) -> Vec<Vec<Id>> {
    let mut turns: Vec<(BTreeSet<Id>, Vec<Id>)> = Vec::new();
    for node in thread_to(facts, leaf) {
        let directions = node_directions(facts, node);
        match turns.last_mut() {
            Some((current, run)) if directions.len() == 1 && *current == directions => {
                run.push(node)
            }
            _ => turns.push((directions, vec![node])),
        }
    }
    turns.into_iter().map(|(_, run)| run).collect()
}

/// The turn that holds `node`: the maximal run of consecutive nodes of its
/// direction around it, following its predecessor back and its single
/// successor forward. A branch ends the run forward.
pub fn turn_of<P: TriblePattern>(facts: &P, node: Id) -> Vec<Id> {
    let directions = node_directions(facts, node);
    let mut run = vec![node];
    if directions.len() != 1 {
        return run;
    }
    let mut cursor = node;
    loop {
        let previous: Vec<Id> = find!(
            previous: Id,
            pattern!(facts, [{ cursor @ schema::block::previous: ?previous }])
        )
        .collect();
        match previous.as_slice() {
            [single] if node_directions(facts, *single) == directions && !run.contains(single) => {
                run.insert(0, *single);
                cursor = *single;
            }
            _ => break,
        }
    }
    let mut cursor = node;
    loop {
        let next: Vec<Id> = find!(
            next: Id,
            pattern!(facts, [{ ?next @ schema::block::previous: cursor }])
        )
        .collect();
        match next.as_slice() {
            [single] if node_directions(facts, *single) == directions && !run.contains(single) => {
                run.push(*single);
                cursor = *single;
            }
            _ => break,
        }
    }
    run
}

/// Construct one producer-native receipt: a producer's own canonical record
/// of the event, in the producer's own encoding.
///
/// The identity contains the source namespace, the producer's locator, the
/// producer's receipt bytes, and the projected block. Drive writes its live
/// history this way. Importers never do: they store no source bytes and use
/// [`block_source_projection`] instead.
pub fn source_projection<T>(
    source_namespace: Id,
    source_locator: impl Into<String>,
    raw_record: T,
    block: Fragment,
) -> Result<Fragment>
where
    T: triblespace::core::blob::IntoBlob<RawBytes>,
{
    rooted(&block, "block")?;
    let mut fragment = Fragment::empty();
    let source_locator = fragment.put::<UTF8String, _>(source_locator.into());
    let raw_record = fragment.put::<RawBytes, _>(raw_record);
    fragment += entity! { _ @
        schema::source_projection::source_namespace: &source_namespace,
        schema::source_projection::source_locator: source_locator,
        schema::source_projection::raw_record: raw_record,
        schema::source_projection::projects_to*: block,
    };
    attach_kind(
        fragment,
        schema::source_projection::KIND,
        "source projection",
    )
}

/// Construct the source-scoped receipt of one canonical block, keyed by what
/// the projected message IS rather than by where it was read.
///
/// Its identity is the source namespace and the projected block, nothing else.
/// The block already fixes the message's content and its place in the
/// conversation (its predecessor chain), so every source occurrence of that
/// message converges on this one receipt: the original line, a fork's replayed
/// copy of it, a resumed rollout's, a re-export's under new provider ids. Time,
/// actor, model and conversation id annotate the receipt (directly or
/// through [`source_occurrence`]) and never enter its identity.
///
/// Every importer uses this receipt. No source bytes are part of it.
pub fn block_source_projection(source_namespace: Id, block: Fragment) -> Result<Fragment> {
    rooted(&block, "block")?;
    let fragment = entity! { _ @
        schema::source_projection::source_namespace: &source_namespace,
        schema::source_projection::projects_to*: block,
    };
    attach_kind(
        fragment,
        schema::source_projection::KIND,
        "source projection",
    )
}

/// One source occurrence of a [`block_source_projection`] receipt, as a
/// fragment of its own.
///
/// It holds only annotations of `receipt`. The import writer publishes every
/// annotation apart from the node identities ([`Identity`]), which stay
/// the same bytes wherever the node is read. No source bytes are part of an
/// occurrence.
pub fn source_occurrence(receipt: Id, annotations: ProjectionAnnotations) -> Result<Fragment> {
    annotate_source_projection(Fragment::rooted(receipt, TribleSet::new()), annotations)
}

/// Attach occurrence-scoped evidence without changing a projection receipt's
/// intrinsic root.
pub fn annotate_source_projection(
    mut projection: Fragment,
    annotations: ProjectionAnnotations,
) -> Result<Fragment> {
    let root = rooted(&projection, "source projection")?;
    let raw_author = annotations
        .raw_author
        .map(|value| projection.put::<UTF8String, _>(value));
    let raw_role = annotations
        .raw_role
        .map(|value| projection.put::<UTF8String, _>(value));
    let raw_model = annotations
        .raw_model
        .map(|value| projection.put::<UTF8String, _>(value));
    let source_locator = annotations
        .source_locator
        .map(|value| projection.put::<UTF8String, _>(value));
    let source_detail = annotations
        .source_detail
        .map(|value| projection.put::<UTF8String, _>(value));
    let raw_agent_name = annotations
        .raw_agent_name
        .map(|value| projection.put::<UTF8String, _>(value));
    let source_thread = annotations
        .source_thread
        .map(|value| projection.put::<UTF8String, _>(value));
    let raw_thread_source = annotations
        .raw_thread_source
        .map(|value| projection.put::<UTF8String, _>(value));
    let raw_agent_path = annotations
        .raw_agent_path
        .map(|value| projection.put::<UTF8String, _>(value));

    projection += entity! { ExclusiveId::force_ref(&root) @
        schema::source_projection::semantic_predecessor_support*:
            annotations.semantic_predecessor_support.iter(),
        schema::source_projection::source_timestamp?: annotations.source_timestamp,
        schema::source_projection::author?: annotations.author,
        schema::source_projection::experiencer?: annotations.experiencer,
        schema::source_projection::raw_author?: raw_author.as_ref(),
        schema::source_projection::raw_role?: raw_role.as_ref(),
        schema::source_projection::raw_model?: raw_model.as_ref(),
        schema::source_projection::generated_by*: annotations.generated_by.iter(),
        schema::source_projection::source_locator?: source_locator.as_ref(),
        schema::source_projection::source_detail?: source_detail.as_ref(),
        schema::source_projection::raw_agent_name?: raw_agent_name.as_ref(),
        schema::source_projection::source_thread?: source_thread.as_ref(),
        schema::source_projection::raw_thread_source?: raw_thread_source.as_ref(),
        schema::source_projection::raw_agent_path?: raw_agent_path.as_ref(),
    };
    debug_assert_eq!(projection.root(), Some(root));
    Ok(projection)
}

/// [`annotate_source_projection`] for an occurrence a named model wrote:
/// `generated_by` points at `model` (a [`vendor_model`] fragment), and the
/// model's facts and name blob ride along unexported, so the receipt keeps
/// its one root.
pub fn annotate_generated_projection(
    projection: Fragment,
    mut annotations: ProjectionAnnotations,
    model: &Fragment,
) -> Result<Fragment> {
    annotations
        .generated_by
        .push(rooted(model, "vendor model")?);
    let mut projection = annotate_source_projection(projection, annotations)?;
    let (_, facts, metafacts, blobs) = model.clone().into_parts();
    projection += Fragment::from_parts(facts, metafacts, blobs);
    Ok(projection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::files as files_schema;
    use hifitime::Epoch;
    use triblespace::core::repo::BlobStoreGet;
    use triblespace::prelude::inlineencodings::Handle;

    fn id(byte: u8) -> Id {
        Id::new([byte; 16]).expect("test ids are non-nil")
    }

    fn instant(seconds: f64) -> Inline<NsTAIInterval> {
        let epoch = Epoch::from_unix_seconds(seconds);
        (epoch, epoch).try_to_inline().expect("valid interval")
    }

    fn text(value: &str) -> Fragment {
        text_fact(
            schema::content_fact::modality::TEXT,
            schema::content_fact::direction::IN,
            value,
        )
        .unwrap()
    }

    fn one_part(value: &str) -> Fragment {
        text(value)
    }

    fn projection(parts: Fragment) -> Fragment {
        source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "session:test",
            br#"{"type":"test"}"#.as_slice(),
            block([], None, parts).unwrap(),
        )
        .unwrap()
    }

    fn said(direction: Id, value: &str) -> Fragment {
        text_fact(schema::content_fact::modality::TEXT, direction, value).unwrap()
    }

    #[test]
    fn a_simultaneous_set_node_keeps_both_facts_and_no_order() {
        let heard = said(schema::content_fact::direction::IN, "heard");
        let spoken = said(schema::content_fact::direction::OUT, "spoken");
        let one_way = block([], None, heard.clone() + spoken.clone()).unwrap();
        let other_way = block([], None, spoken + heard).unwrap();
        assert_eq!(one_way.root(), other_way.root(), "a set has no order");
        let node = one_way.root().unwrap();
        assert_eq!(
            find!(fact: Id, pattern!(&one_way, [{ node @ schema::block::holds: ?fact }])).count(),
            2
        );
        assert_eq!(node_directions(one_way.facts(), node).len(), 2);
        assert_eq!(
            turn_of(one_way.facts(), node),
            vec![node],
            "both ways at once is a turn of its own"
        );
    }

    #[test]
    fn order_is_recovered_from_the_chain_alone() {
        let mut union = Fragment::empty();
        let mut previous = None;
        let mut expected = Vec::new();
        // An equal fact twice is two nodes: the chain, not a position, tells
        // them apart.
        for value in ["first", "second", "first", "third"] {
            let node = block(previous, None, text(value)).unwrap();
            previous = node.root();
            expected.push(previous.unwrap());
            union += node;
        }
        assert_eq!(expected.iter().collect::<BTreeSet<_>>().len(), 4);
        assert_eq!(
            thread_to(union.facts(), *expected.last().unwrap()),
            expected
        );
    }

    #[test]
    fn the_turn_view_groups_runs_of_one_direction() {
        use schema::content_fact::direction::{IN, OUT};
        let mut union = Fragment::empty();
        let mut previous = None;
        let mut nodes = Vec::new();
        for (direction, value) in [
            (IN, "question"),
            (OUT, "thinking"),
            (OUT, "call"),
            (IN, "tool result"),
            (IN, "user message"),
            (OUT, "answer"),
        ] {
            let node = block(previous, None, said(direction, value)).unwrap();
            previous = node.root();
            nodes.push(previous.unwrap());
            union += node;
        }
        assert_eq!(
            turns_along(union.facts(), nodes[5]),
            vec![
                vec![nodes[0]],
                vec![nodes[1], nodes[2]],
                vec![nodes[3], nodes[4]],
                vec![nodes[5]],
            ],
            "a tool result and the user message after it are one perception"
        );
        assert_eq!(turn_of(union.facts(), nodes[4]), vec![nodes[3], nodes[4]]);
        assert_eq!(turn_of(union.facts(), nodes[1]), vec![nodes[1], nodes[2]]);
    }

    #[test]
    fn responds_to_links_node_to_node_without_changing_identity() {
        let call = block([], None, text("call")).unwrap();
        let call_id = call.root().unwrap();
        let result = block([call_id], None, text("result")).unwrap();
        let result_id = result.root().unwrap();
        let mut linked = call + result;
        linked += responds_to(result_id, call_id);
        assert!(exists!(pattern!(&linked, [{
            result_id @ schema::block::responds_to: call_id
        }])));
        let receipt = source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "linked",
            b"{}".as_slice(),
            Fragment::rooted(result_id, linked.facts().clone()),
        )
        .unwrap();
        let mut catalog = receipt;
        catalog += source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "call",
            b"{}".as_slice(),
            block([], None, text("call")).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn raw_only_receipt_projects_to_the_canonical_bottom_block() {
        let receipt = projection(Fragment::empty());
        let receipt_id = receipt.root().unwrap();
        let block = find!(
            (block: Id),
            pattern!(&receipt, [{
                receipt_id @ schema::source_projection::projects_to: ?block
            }])
        )
        .next()
        .map(|(block,)| block)
        .unwrap();
        assert!(!exists!(pattern!(&receipt, [{
            block @ schema::block::holds: _?fact
        }])));
    }

    #[test]
    fn direction_is_in_or_out_only() {
        assert_eq!(
            schema::content_fact::direction::SPECS,
            &[
                (schema::content_fact::direction::IN, "in"),
                (schema::content_fact::direction::OUT, "out"),
            ]
        );
    }

    #[test]
    fn archive_vocabulary_is_queryable_collection_data() {
        let mut vocabulary = vocabulary_fragment();
        let text = schema::content_fact::modality::TEXT;
        let alias = vocabulary.put::<UTF8String, _>("written text".to_owned());
        vocabulary += entity! { ExclusiveId::force_ref(&text) @
            metadata::name: alias,
        };
        let names: BTreeSet<_> = find!(
            value: Inline<Handle<UTF8String>>,
            pattern!(&vocabulary, [{
                text @ metadata::tag: &metadata::KIND_TAG,
                metadata::name: ?value,
            }])
        )
        .collect();
        let mut blobs = vocabulary.blobs().clone();
        let reader = blobs.snapshot().unwrap();
        let values: BTreeSet<String> = names
            .into_iter()
            .map(|name| {
                let value: anybytes::View<str> = reader.get(name).unwrap();
                value.to_string()
            })
            .collect();
        assert_eq!(
            values,
            BTreeSet::from(["text".to_owned(), "written text".to_owned()])
        );
    }

    #[test]
    fn timestamp_is_a_repeatable_annotation_not_intrinsic_state() {
        let without = block([], None, one_part("same")).unwrap();
        let with_42 = block([], Some(instant(42.0)), one_part("same")).unwrap();
        let with_43 = block([], Some(instant(43.0)), one_part("same")).unwrap();
        assert_eq!(without.root(), with_42.root());
        assert_eq!(without.root(), with_43.root());
        let root = without.root().unwrap();
        assert!(!exists!(pattern!(&without, [{
            root @ schema::block::timestamp: _?timestamp
        }])));

        let mut observed_twice = with_42;
        observed_twice += with_43;
        let projected = source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "timestamp-observations",
            b"{}".as_slice(),
            observed_twice,
        )
        .unwrap();
        let timestamps: BTreeSet<_> = find!(
            timestamp: Inline<NsTAIInterval>,
            pattern!(&projected, [{ root @ schema::block::timestamp: ?timestamp }])
        )
        .collect();
        assert_eq!(timestamps, BTreeSet::from([instant(42.0), instant(43.0)]));
    }

    #[test]
    fn source_projection_is_idempotent_but_preserves_occurrences() {
        let make = |locator: &str| {
            source_projection(
                schema::source_projection::SOURCE_CLAUDE_CODE,
                locator,
                br#"{\"type\":\"user\"}"#.as_slice(),
                block([], None, one_part("hello")).unwrap(),
            )
            .unwrap()
        };
        let first = make("session:7");
        let replay = make("session:7");
        let other_locator = make("session:8");
        let other_raw = source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "session:7",
            b"different raw record".as_slice(),
            block([], None, one_part("hello")).unwrap(),
        )
        .unwrap();
        assert_eq!(first, replay);
        assert_ne!(first.root(), other_locator.root());
        assert_ne!(first.root(), other_raw.root());

        let projected = |projection: &Fragment| {
            let root = projection.root().unwrap();
            find!(
                block: Id,
                pattern!(projection, [{
                    root @ schema::source_projection::projects_to: ?block
                }])
            )
            .next()
            .unwrap()
        };
        assert_eq!(projected(&first), projected(&other_locator));
        assert_eq!(projected(&first), projected(&other_raw));
    }

    #[test]
    fn occurrence_annotations_do_not_change_projection_identity() {
        let projection = source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "session:7",
            b"raw".as_slice(),
            block([], None, one_part("hello")).unwrap(),
        )
        .unwrap();
        let root = projection.root().unwrap();
        let annotated = annotate_source_projection(
            projection,
            ProjectionAnnotations {
                semantic_predecessor_support: vec![id(8)],
                source_timestamp: Some(instant(42.0)),
                author: Some(id(9)),
                experiencer: Some(id(10)),
                raw_author: Some("human".into()),
                raw_role: Some("user".into()),
                raw_model: Some("model-v1".into()),
                generated_by: vec![id(11)],
                source_locator: Some("conversation/7".into()),
                source_detail: Some(r#"{"agentType":"general-purpose"}"#.into()),
                raw_agent_name: Some("agent-cc".into()),
                source_thread: Some("thread/t-7".into()),
                raw_thread_source: Some("subagent".into()),
                raw_agent_path: Some("/root/task".into()),
            },
        )
        .unwrap();
        assert_eq!(annotated.root(), Some(root));
        assert!(exists!(pattern!(&annotated, [{
            root @ schema::source_projection::raw_agent_name: "agent-cc",
            schema::source_projection::source_thread: "thread/t-7",
            schema::source_projection::raw_thread_source: "subagent",
            schema::source_projection::raw_agent_path: "/root/task",
        }])));
        assert!(exists!(pattern!(&annotated, [{
            root @ schema::source_projection::author: &id(9),
            schema::source_projection::experiencer: &id(10),
            schema::source_projection::generated_by: &id(11),
        }])));
        assert!(exists!(pattern!(&annotated, [{
            root @ schema::source_projection::source_locator: _?locator
        }])));
    }

    #[test]
    fn semantic_predecessor_support_is_read_wherever_it_points() {
        let parent = block([], None, one_part("parent")).unwrap();
        let parent_id = parent.root().unwrap();
        let unrelated = block([], None, one_part("unrelated")).unwrap();
        let child = block([parent_id], None, one_part("child")).unwrap();

        let parent_receipt = source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "session:parent",
            b"parent".as_slice(),
            parent,
        )
        .unwrap();
        let unrelated_receipt = source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "session:unrelated",
            b"unrelated".as_slice(),
            unrelated,
        )
        .unwrap();
        let unrelated_receipt_id = unrelated_receipt.root().unwrap();
        let child_receipt = source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "session:child",
            b"child".as_slice(),
            child,
        )
        .and_then(|projection| {
            annotate_source_projection(
                projection,
                ProjectionAnnotations {
                    semantic_predecessor_support: vec![unrelated_receipt_id],
                    ..ProjectionAnnotations::default()
                },
            )
        })
        .unwrap();

        let mut fragment = parent_receipt;
        fragment += unrelated_receipt;
        fragment += child_receipt;
    }

    #[test]
    fn binary_facts_share_files_media_type_entities() {
        let fact = blob_fact(
            schema::content_fact::modality::IMAGE,
            schema::content_fact::direction::IN,
            b"png".as_slice(),
            "IMAGE/PNG; charset=binary",
        )
        .unwrap();
        let root = fact.root().unwrap();
        let media_types: Vec<Id> = find!(
            media_type: Id,
            pattern!(&fact, [{ root @ schema::content_fact::media_type: ?media_type }])
        )
        .collect();
        assert_eq!(media_types.len(), 1);
        assert!(exists!(pattern!(&fact, [{
            media_types[0] @ metadata::tag: &files_schema::KIND_MEDIA_TYPE
        }])));
    }

    #[test]
    fn only_the_canonical_bottom_block_is_built_content_free() {
        let bottom = block([], None, Fragment::empty()).unwrap();
        let predecessor = block([], None, one_part("predecessor")).unwrap();
        let predecessor_id = predecessor.root().unwrap();

        assert!(block([predecessor_id], None, Fragment::empty()).is_err());
        assert!(block([], Some(instant(42.0)), Fragment::empty()).is_err());

        let invalid_core = entity! { _ @
            schema::block::previous: &predecessor_id,
        };
        let invalid = attach_kind(invalid_core, schema::block::KIND, "block").unwrap();
        let invalid_id = invalid.root().unwrap();
        let receipt = source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "invalid:content-free-child",
            b"{}".as_slice(),
            invalid,
        )
        .unwrap();
        let mut catalog = bottom;
        catalog += predecessor;
        catalog += receipt;
        // Another writer's content-free child is read all the same.
        assert!(exists!(pattern!(&catalog, [{
            invalid_id @ schema::block::previous: &predecessor_id
        }])));
    }

    #[test]
    fn kind_markers_do_not_participate_in_intrinsic_ids() {
        let fact = text("identity");
        let fact_id = fact.root().unwrap();
        let payload = find!(
            payload: Inline<Handle<UTF8String>>,
            pattern!(&fact, [{
                fact_id @ schema::content_fact::payload: ?payload
            }])
        )
        .next()
        .unwrap();
        let core = entity! { _ @
            schema::content_fact::modality: &schema::content_fact::modality::TEXT,
            schema::content_fact::direction: &schema::content_fact::direction::IN,
            schema::content_fact::payload: payload,
        };
        assert_eq!(core.root(), Some(fact_id));
        assert!(exists!(pattern!(&fact, [{
            fact_id @ metadata::tag: &schema::content_fact::KIND
        }])));
    }

    #[test]
    fn resolving_pointer_bytes_keeps_the_intrinsic_root() {
        let fact = asset_pointer_fact(
            schema::content_fact::modality::IMAGE,
            schema::content_fact::direction::IN,
            id(11),
            "asset-42",
            Some("image/png"),
            Some(3),
        )
        .unwrap();
        let root = fact.root().unwrap();
        let resolved = resolve_pointer_fact(fact, b"png".as_slice()).unwrap();
        assert_eq!(resolved.root(), Some(root));
        let handles: Vec<Inline<Handle<RawBytes>>> = find!(
            handle: Inline<Handle<RawBytes>>,
            pattern!(&resolved, [{ root @ schema::content_fact::resolved_to: ?handle }])
        )
        .collect();
        assert_eq!(handles.len(), 1);
    }

    fn jpeg(segments: &[(u8, &[u8])], scan: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8];
        for (marker, data) in segments {
            bytes.extend([0xFF, *marker]);
            bytes.extend(u16::try_from(data.len() + 2).unwrap().to_be_bytes());
            bytes.extend(*data);
        }
        bytes.extend([0xFF, 0xDA, 0x00, 0x02]);
        bytes.extend(scan);
        bytes.extend([0xFF, 0xD9]);
        bytes
    }

    fn png(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in chunks {
            bytes.extend(u32::try_from(data.len()).unwrap().to_be_bytes());
            bytes.extend(*kind);
            bytes.extend(*data);
            bytes.extend([0; 4]);
        }
        bytes
    }

    #[test]
    fn exif_is_found_by_walking_the_container() {
        let jfif: (u8, &[u8]) = (0xE0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0");
        let exif: (u8, &[u8]) = (0xE1, b"Exif\0\0MM\0*");
        let xmp: (u8, &[u8]) = (0xE1, b"http://ns.adobe.com/xap/1.0/\0<x/>");
        assert!(carries_exif(&jpeg(&[jfif, exif], b"scan")));
        assert!(!carries_exif(&jpeg(&[jfif], b"scan")));
        assert!(!carries_exif(&jpeg(&[jfif, xmp], b"scan")));
        // The marker's text inside compressed data is not a segment.
        assert!(!carries_exif(&jpeg(&[jfif], b"Exif\0\0MM")));
        // A segment that claims more bytes than there are.
        let mut truncated = jpeg(&[jfif, exif], b"");
        truncated.truncate(24);
        assert!(!carries_exif(&truncated));

        let ihdr: (&[u8; 4], &[u8]) = (b"IHDR", &[0; 13]);
        assert!(carries_exif(&png(&[
            ihdr,
            (b"eXIf", b"MM\0*"),
            (b"IEND", b"")
        ])));
        assert!(!carries_exif(&png(&[
            ihdr,
            (b"tEXt", b"eXIf"),
            (b"IEND", b"")
        ])));

        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend(10u32.to_le_bytes());
        webp.extend([0; 10]);
        webp.extend(b"EXIF");
        webp.extend(4u32.to_le_bytes());
        webp.extend(b"MM\0*");
        assert!(carries_exif(&webp));
        assert!(!carries_exif(&webp[..30]));
        assert!(!carries_exif(b"plain bytes"));
    }

    #[test]
    fn the_preferred_resolution_follows_the_stated_order() {
        let jfif: (u8, &[u8]) = (0xE0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0");
        let exif: (u8, &[u8]) = (0xE1, b"Exif\0\0MM\0*");
        let original = jpeg(&[jfif, exif], b"o");
        let stripped_large = jpeg(&[jfif], b"a much larger stripped body");
        let stripped_small = jpeg(&[jfif], b"s");
        let labelled = jpeg(&[jfif, exif], b"labelled but large, with metadata");
        let handle = |bytes: &[u8]| -> Inline<Handle<RawBytes>> { bytes.to_blob().get_handle() };
        let fact = |bodies: &[&[u8]], sanitized: &[&[u8]]| {
            let mut fact = asset_pointer_fact(
                schema::content_fact::modality::IMAGE,
                schema::content_fact::direction::IN,
                id(11),
                "sediment://file_1",
                Some("image/jpeg"),
                None,
            )
            .unwrap();
            for body in bodies {
                fact = resolve_pointer_fact(fact, body.to_vec()).unwrap();
            }
            for body in sanitized {
                fact = resolve_sanitized_pointer_fact(fact, body.to_vec()).unwrap();
            }
            fact
        };
        let pick = |fact: &Fragment, readable: &[&[u8]]| {
            let root = fact.root().unwrap();
            let readable: Vec<Vec<u8>> = readable.iter().map(|body| body.to_vec()).collect();
            preferred_resolution(fact.facts(), root, |wanted| {
                readable
                    .iter()
                    .find(|body| handle(body.as_slice()) == wanted)
                    .map(|body| anybytes::Bytes::from_source(body.clone()))
            })
        };
        let every: &[&[u8]] = &[&original, &stripped_large, &stripped_small, &labelled];

        // Metadata beats size; a sanitized label beats metadata.
        let all = fact(&[&original, &stripped_large, &stripped_small], &[&labelled]);
        assert_eq!(pick(&all, every), Some(handle(&original)));
        // Without metadata anywhere unlabelled, the larger body.
        let stripped = fact(&[&stripped_large, &stripped_small], &[&labelled]);
        assert_eq!(pick(&stripped, every), Some(handle(&stripped_large)));
        // Only a labelled body: it is still the one shown.
        let only = fact(&[], &[&labelled]);
        assert_eq!(pick(&only, every), Some(handle(&labelled)));
        // A body that cannot be read ranks below one that can.
        assert_eq!(
            pick(&all, &[&stripped_small, &labelled]),
            Some(handle(&stripped_small))
        );
        // Nothing readable: the smallest handle, whatever the insertion order.
        let forward = fact(&[&original, &stripped_large], &[]);
        let reverse = fact(&[&stripped_large, &original], &[]);
        let smallest = [handle(&original), handle(&stripped_large)]
            .into_iter()
            .min_by_key(|handle| handle.raw)
            .unwrap();
        assert_eq!(pick(&forward, &[]), Some(smallest));
        assert_eq!(pick(&reverse, &[]), Some(smallest));
        // No resolution at all.
        assert_eq!(pick(&fact(&[], &[]), every), None);
    }

    #[test]
    fn a_page_annotation_keeps_the_intrinsic_root() {
        let fact = asset_pointer_fact(
            schema::content_fact::modality::IMAGE,
            schema::content_fact::direction::IN,
            id(11),
            "sediment://hash#file_1#p_3.jpg",
            None,
            Some(5),
        )
        .unwrap();
        let root = fact.root().unwrap();
        let annotated = page_of_pointer_fact(fact, "sediment://file_1".to_owned(), 3).unwrap();
        assert_eq!(annotated.root(), Some(root));
        let document: Inline<Handle<UTF8String>> =
            "sediment://file_1".to_owned().to_blob().get_handle();
        assert!(exists!(pattern!(&annotated, [{
            root @ schema::content_fact::page_of: document,
                schema::content_fact::page: 3u128,
        }])));
        assert!(!exists!(pattern!(&annotated, [{
            root @ schema::content_fact::resolved_to: _?bytes
        }])));
    }

    #[test]
    fn an_exact_body_annotates_and_never_identifies() {
        let make = |bytes: &'static [u8]| {
            asset_pointer_fact(
                schema::content_fact::modality::FILE,
                schema::content_fact::direction::IN,
                id(11),
                "sandbox:/same",
                Some("application/octet-stream"),
                Some(bytes.len() as u128),
            )
            .and_then(|fact| resolve_pointer_fact(fact, bytes))
            .unwrap()
        };
        let first = block([], None, make(b"first")).unwrap();
        let second = block([], None, make(b"other")).unwrap();
        assert_eq!(first.root(), second.root(), "the body is not the node");
        let body = (b"first".as_slice()).to_blob().get_handle();
        let receipt = source_projection(
            schema::source_projection::SOURCE_DRIVE,
            "occurrence",
            b"{}".as_slice(),
            first,
        )
        .unwrap();
        let root = receipt.root().unwrap();
        let mut selected = receipt.clone();
        selected += entity! { ExclusiveId::force_ref(&root) @
            schema::source_projection::resolution: body,
        };
        assert_eq!(selected.root(), Some(root));
    }

    #[test]
    fn unresolved_and_ambiguously_resolved_pointers_are_constructed() {
        let unresolved = asset_pointer_fact(
            schema::content_fact::modality::IMAGE,
            schema::content_fact::direction::IN,
            id(11),
            "vendor:asset-42",
            Some("image/png"),
            Some(3),
        )
        .unwrap();
        let unresolved = projection(unresolved);
        assert!(!exists!(pattern!(&unresolved, [{
            _?fact @ schema::content_fact::resolved_to: _?bytes
        }])));

        let resolved = asset_pointer_fact(
            schema::content_fact::modality::IMAGE,
            schema::content_fact::direction::IN,
            id(11),
            "vendor:asset-42",
            None,
            None,
        )
        .and_then(|fact| resolve_pointer_fact(fact, b"first".as_slice()))
        .and_then(|fact| resolve_pointer_fact(fact, b"second".as_slice()))
        .unwrap();
        let resolved = projection(resolved);
        let bytes: BTreeSet<_> = find!(
            bytes: Inline<Handle<RawBytes>>,
            pattern!(&resolved, [{ _?fact @ schema::content_fact::resolved_to: ?bytes }])
        )
        .collect();
        assert_eq!(bytes.len(), 2);
    }

    #[test]
    fn locators_are_additive_occurrence_annotations() {
        let projection = block_source_projection(
            schema::source_projection::SOURCE_CHATGPT,
            block([], None, one_part("moved export")).unwrap(),
        )
        .unwrap();
        let root = projection.root().unwrap();
        let projection = annotate_source_projection(
            projection,
            ProjectionAnnotations {
                source_locator: Some("conversation/first".into()),
                ..ProjectionAnnotations::default()
            },
        )
        .and_then(|projection| {
            annotate_source_projection(
                projection,
                ProjectionAnnotations {
                    source_locator: Some("conversation/second".into()),
                    ..ProjectionAnnotations::default()
                },
            )
        })
        .unwrap();
        assert_eq!(projection.root(), Some(root));
    }
}
