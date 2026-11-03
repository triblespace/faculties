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
use triblespace::core::blob::MemoryBlobStoreSnapshot;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::core::repo::{BlobStoreList, SnapshotSource};
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
}

type TextHandle = Inline<Handle<UTF8String>>;
type RawHandle = Inline<Handle<RawBytes>>;
type OverlayReader = MemoryBlobStoreSnapshot;

/// Whether the bytes an Archive catalog names are resident.
///
/// This is a residency check and nothing else. The schema is open world, so
/// it refuses no fact: it does not check shapes, cardinalities, closure,
/// vocabularies or derived ids, and a fact it does not model is no concern
/// of it. `Pending` names the hashes still to arrive; every asserted handle
/// already fixes them, so they may arrive without changing the catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogValidation {
    /// Every blob the catalog names under an Archive byte attribute is
    /// resident.
    Accepted,
    /// These content hashes are not resident yet.
    Pending { missing: BTreeSet<[u8; 32]> },
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

/// The blobs the catalog names under the attributes Archive reads bytes
/// from, on any entity: every value counts, and none is refused.
fn named_attachments<P>(facts: &P) -> (BTreeSet<TextHandle>, BTreeSet<RawHandle>)
where
    P: TriblePattern,
{
    let mut texts: BTreeSet<TextHandle> = find!(
        value: TextHandle,
        pattern!(facts, [{ _?entity @ metadata::name: ?value }])
    )
    .collect();
    for attribute in [
        &*schema::content_fact::payload,
        &*schema::content_fact::asset_pointer,
        &*schema::source_projection::source_locator,
        &*schema::source_projection::raw_author,
        &*schema::source_projection::raw_role,
        &*schema::source_projection::raw_model,
        &*schema::source_projection::source_detail,
    ] {
        texts.extend(find!(
            value: TextHandle,
            pattern!(facts, [{ _?entity @ attribute: ?value }])
        ));
    }
    let mut raws = BTreeSet::new();
    for attribute in [
        &*schema::content_fact::blob,
        &*schema::content_fact::resolved_to,
        &*schema::source_projection::raw_record,
        &*schema::source_projection::resolution,
    ] {
        raws.extend(find!(
            value: RawHandle,
            pattern!(facts, [{ _?entity @ attribute: ?value }])
        ));
    }
    (texts, raws)
}

fn resident<S>(
    reader: &PileSnapshot,
    overlay: Option<&OverlayReader>,
    handle: Inline<Handle<S>>,
) -> Result<bool>
where
    S: BlobEncoding + 'static,
    Handle<S>: InlineEncoding,
{
    if let Some(overlay) = overlay {
        if overlay
            .contains_blob(handle)
            .map_err(|error| anyhow!("inspect a staged Archive attachment: {error}"))?
        {
            return Ok(true);
        }
    }
    reader
        .contains_blob(handle)
        .map_err(|error| anyhow!("inspect a resident Archive attachment: {error}"))
}

fn validate_catalog_with_overlay<P>(
    reader: &PileSnapshot,
    overlay: Option<&OverlayReader>,
    facts: &P,
) -> Result<CatalogValidation>
where
    P: TriblePattern,
{
    let (texts, raws) = named_attachments(facts);
    let mut missing = BTreeSet::new();
    for handle in texts {
        if !resident(reader, overlay, handle)? {
            missing.insert(handle.raw);
        }
    }
    for handle in raws {
        if !resident(reader, overlay, handle)? {
            missing.insert(handle.raw);
        }
    }
    Ok(if missing.is_empty() {
        CatalogValidation::Accepted
    } else {
        CatalogValidation::Pending { missing }
    })
}

/// Check that the bytes one Archive catalog names are resident in `reader`.
///
/// A pointer fact with no `resolved_to` edge is an unresolved external
/// asset, which names no bytes here.
pub fn validate_catalog<P>(reader: &PileSnapshot, facts: &P) -> Result<CatalogValidation>
where
    P: TriblePattern,
{
    validate_catalog_with_overlay(reader, None, facts)
}

/// [`validate_catalog`] over the staged union, without writing anything.
///
/// Existing handles resolve through `reader`; blobs carried by `fragment`
/// resolve through its in-memory overlay. The returned facts are the union
/// that was checked.
pub fn validate_catalog_union(
    reader: &PileSnapshot,
    current: &TribleSet,
    fragment: &Fragment,
) -> Result<(TribleSet, CatalogValidation)> {
    let mut union = current.clone();
    union += fragment.facts().clone();
    let mut staged = fragment.clone();
    let overlay = staged
        .blobs_mut()
        .snapshot()
        .expect("MemoryBlobStore reader creation is infallible");
    let validation = validate_catalog_with_overlay(reader, Some(&overlay), &union)?;
    Ok((union, validation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::files as files_schema;
    use hifitime::Epoch;
    use std::fs::File;
    use triblespace::core::blob::encodings::succinctarchive::{
        OrderedUniverse, SuccinctArchive, UnionArchive,
    };
    use triblespace::core::repo::pile::Pile;
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

    fn empty_reader() -> (tempfile::TempDir, PileSnapshot) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.pile");
        File::create(&path).unwrap();
        let mut pile = Pile::open(&path).unwrap();
        let reader = pile.snapshot().unwrap();
        pile.close().unwrap();
        (directory, reader)
    }

    fn validate_fragment(fragment: &Fragment) -> CatalogValidation {
        let (_directory, reader) = empty_reader();
        validate_catalog_union(&reader, &TribleSet::new(), fragment)
            .unwrap()
            .1
    }

    fn without_blobs(fragment: Fragment) -> Fragment {
        let (_, facts, metafacts, _) = fragment.into_parts();
        Fragment::from_parts(facts, metafacts, Default::default())
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
        let (_directory, reader) = empty_reader();
        let mut catalog = receipt;
        catalog += source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "call",
            b"{}".as_slice(),
            block([], None, text("call")).unwrap(),
        )
        .unwrap();
        let (_, validation) = validate_catalog_union(&reader, &TribleSet::new(), &catalog).unwrap();
        assert!(
            matches!(
                validation,
                CatalogValidation::Accepted | CatalogValidation::Pending { .. }
            ),
            "{validation:?}"
        );
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

        let (_directory, reader) = empty_reader();
        let (_, validation) = validate_catalog_union(&reader, &TribleSet::new(), &receipt).unwrap();
        assert_eq!(validation, CatalogValidation::Accepted);
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
            value: TextHandle,
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

        let (_directory, reader) = empty_reader();
        let (_, validation) =
            validate_catalog_union(&reader, &TribleSet::new(), &vocabulary).unwrap();
        assert_eq!(validation, CatalogValidation::Accepted);
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
        assert_eq!(validate_fragment(&projected), CatalogValidation::Accepted);
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
            },
        )
        .unwrap();
        assert_eq!(annotated.root(), Some(root));
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
        let (_directory, reader) = empty_reader();
        let (_, validation) =
            validate_catalog_union(&reader, &TribleSet::new(), &fragment).unwrap();
        assert_eq!(validation, CatalogValidation::Accepted);
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
        let (_directory, reader) = empty_reader();
        let (_, validation) = validate_catalog_union(&reader, &TribleSet::new(), &catalog).unwrap();
        // Another writer's content-free child is read all the same.
        assert_eq!(validation, CatalogValidation::Accepted);
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
        let (_directory, reader) = empty_reader();
        let (_, validation) =
            validate_catalog_union(&reader, &TribleSet::new(), &selected).unwrap();
        assert_eq!(validation, CatalogValidation::Accepted);
    }

    #[test]
    fn staged_complete_projection_is_accepted_without_writing() {
        let (_directory, reader) = empty_reader();
        let fragment = projection(one_part("hello from the staged overlay"));
        let (union, validation) =
            validate_catalog_union(&reader, &TribleSet::new(), &fragment).unwrap();
        assert_eq!(union, fragment.facts().clone());
        assert_eq!(validation, CatalogValidation::Accepted);
    }

    #[test]
    fn complete_graph_without_attachments_is_pending() {
        let (_directory, reader) = empty_reader();
        let fragment = projection(one_part("hello without resident bytes"));
        let validation = validate_catalog(&reader, fragment.facts()).unwrap();
        let CatalogValidation::Pending { missing } = validation else {
            panic!("complete graph with absent blobs should be pending");
        };
        assert_eq!(missing.len(), 3, "payload, locator, and raw source record");
    }

    #[test]
    fn a_succinct_cover_names_the_same_attachments() {
        let (_directory, reader) = empty_reader();
        let fragment = projection(one_part("hello from a succinct cover"));
        let segment = SuccinctArchive::<OrderedUniverse>::from(fragment.facts());
        let cover = UnionArchive::new(vec![segment.clone(), segment]);

        let validation = validate_catalog(&reader, &cover).unwrap();
        let CatalogValidation::Pending { missing } = validation else {
            panic!("complete succinct graph with absent blobs should be pending");
        };
        assert_eq!(missing.len(), 3, "payload, locator, and raw source record");
    }

    #[test]
    fn a_dangling_response_leaves_only_missing_attachments() {
        let (_directory, reader) = empty_reader();
        let mut dangling = projection(one_part("dangling response"));
        let node = find!(
            node: Id,
            pattern!(&dangling, [{ _?receipt @ schema::source_projection::projects_to: ?node }])
        )
        .next()
        .unwrap();
        dangling += responds_to(node, id(99));
        let validation = validate_catalog(&reader, dangling.facts()).unwrap();
        let CatalogValidation::Pending { missing } = validation else {
            panic!("a response to an absent node is read; only its bytes are missing");
        };
        assert_eq!(missing.len(), 3, "payload, locator, and raw source record");
    }

    #[test]
    fn unresolved_and_ambiguously_resolved_pointers_are_valid() {
        let (_directory, reader) = empty_reader();
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
        let (_, validation) =
            validate_catalog_union(&reader, &TribleSet::new(), &unresolved).unwrap();
        assert_eq!(validation, CatalogValidation::Accepted);

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
        let (_, validation) =
            validate_catalog_union(&reader, &TribleSet::new(), &resolved).unwrap();
        assert_eq!(validation, CatalogValidation::Accepted);
    }

    #[test]
    fn locators_are_additive_occurrence_annotations() {
        let (_directory, reader) = empty_reader();
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
        let (_, validation) =
            validate_catalog_union(&reader, &TribleSet::new(), &projection).unwrap();
        assert_eq!(validation, CatalogValidation::Accepted);
    }

    /// The open world, as this reader meets it: a catalog holding facts it
    /// does not model is read, never refused. Each case is staged with the
    /// bytes it names, so everything it names is resident.
    fn assert_read(fragment: &Fragment) {
        let validation = validate_fragment(fragment);
        assert_eq!(validation, CatalogValidation::Accepted, "{validation:?}");
    }

    fn receipt_of(block: Fragment) -> Fragment {
        source_projection(
            schema::source_projection::SOURCE_CLAUDE_CODE,
            "session:open-world",
            br#"{"type":"test"}"#.as_slice(),
            block,
        )
        .unwrap()
    }

    #[test]
    fn a_fact_with_an_unknown_modality_or_direction_is_read() {
        let modality = text_fact(id(12), schema::content_fact::direction::IN, "new modality");
        assert_read(&projection(modality.unwrap()));
        let direction = text_fact(
            schema::content_fact::modality::TEXT,
            id(14),
            "new direction",
        );
        assert_read(&projection(direction.unwrap()));
    }

    #[test]
    fn a_text_fact_with_unmodelled_attributes_is_read() {
        let mut fact = text("annotated by someone else");
        let fact_id = fact.root().unwrap();
        fact += entity! { ExclusiveId::force_ref(&fact_id) @
            metadata::name: "a name no Archive reader models on a fact",
            schema::content_fact::media_type: &id(13),
        };
        assert_read(&projection(fact));
    }

    #[test]
    fn a_fact_with_two_payloads_is_read() {
        let mut fact = text("first payload");
        let fact_id = fact.root().unwrap();
        fact += entity! { ExclusiveId::force_ref(&fact_id) @
            schema::content_fact::payload: "second payload",
        };
        assert_read(&projection(fact));
    }

    #[test]
    fn entities_whose_ids_do_not_re_derive_are_read() {
        let fact = fucid();
        let fact = entity! { &fact @
            metadata::tag: &schema::content_fact::KIND,
            schema::content_fact::modality: &schema::content_fact::modality::TEXT,
            schema::content_fact::direction: &schema::content_fact::direction::IN,
            schema::content_fact::payload: "held under a fresh id",
        };
        let node = fucid();
        let node = entity! { &node @
            metadata::tag: &schema::block::KIND,
            schema::block::holds*: fact,
        };
        assert_read(&receipt_of(node));
    }

    #[test]
    fn a_node_naming_entities_this_catalog_lacks_is_read() {
        let node = fucid();
        let node = entity! { &node @
            metadata::tag: &schema::block::KIND,
            schema::block::previous: &id(97),
            schema::block::holds: &id(98),
            schema::block::responds_to: &id(99),
        };
        let mut catalog = receipt_of(node);
        catalog += text("a fact no node holds");
        assert_read(&catalog);
    }
}
