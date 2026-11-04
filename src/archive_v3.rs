//! Constructors for Archive model v3 ([`crate::schemas::archive_v3`]).
//!
//! Every constructor builds an entity with `entity!` and an intrinsic id
//! derived from the entity's identity core alone, and returns it as a rooted
//! [`Fragment`]. The kind marker is attached afterwards. Annotations are built
//! by separate functions that return fragments exporting nothing, so merging
//! them can never change an id: the importer stages identity and annotations
//! side by side.
//!
//! What goes into a fact core is decided by the importer's rendering table,
//! never by which fields one file happens to record. [`FactCore`] holds only
//! what her window rendered; a source record's claims about an item form one
//! [`occurrence`] of the witness ([`WitnessAnnotations`]).
//!
//! Nothing here reads a store: no lookups, no validation of derived ids.

use anyhow::{anyhow, bail, Result};
use triblespace::core::metadata;
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

use crate::blockdag;
use crate::schemas::archive_v3 as schema;

pub mod claude_code;
pub mod fields;
pub mod gate;
pub mod media_type;
#[cfg(test)]
mod tests;

pub use media_type::MediaType;

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

/// The same facts, metafacts and blobs, exporting nothing: the shape of an
/// annotation fragment.
pub(crate) fn unexported(fragment: Fragment) -> Fragment {
    let (_, facts, metafacts, blobs) = fragment.into_parts();
    Fragment::from_parts(facts, metafacts, blobs)
}

/// Everything her window rendered of one fact: the fact's identity core.
///
/// Each field is present only when the rendering table says her window
/// rendered it. `modality` and `direction` are absent only on a span gap
/// whose kind or direction is unknown.
#[derive(Clone, Debug, Default)]
pub struct FactCore {
    pub modality: Option<Id>,
    pub direction: Option<Id>,
    /// The exact decoded text. Never `""`.
    pub payload: Option<String>,
    /// Resident bytes.
    pub blob: Option<anybytes::Bytes>,
    /// A `json_tree` document, rooted.
    pub structure: Option<Fragment>,
    pub tool_name: Option<String>,
    pub tool_namespace: Option<String>,
    /// A [`schema::fact::status`] term.
    pub status: Option<Id>,
    pub exit_code: Option<i64>,
    pub media_type: Option<MediaType>,
    pub source_uri: Vec<String>,
    pub title: Option<String>,
    /// The sense label she was shown.
    pub sense: Option<String>,
    /// A [`schema::fact::detail`] term.
    pub detail: Option<Id>,
    /// A [`schema::fact::register`] term.
    pub register: Option<Id>,
    /// A [`schema::fact::form`] term.
    pub form: Option<Id>,
    /// A [`schema::fact::absent`] term: this fact is a gap.
    pub absent: Option<Id>,
    /// A [`schema::fact::effort`] term, on the system fact that renders a
    /// request's settings.
    pub effort: Option<Id>,
}

/// One fact, rooted at its intrinsic id.
pub fn fact(core: FactCore) -> Result<Fragment> {
    if (core.modality.is_none() || core.direction.is_none())
        && core.absent != Some(schema::fact::absent::SPAN)
    {
        bail!("only a span gap may leave its modality or direction unknown");
    }
    if core.payload.as_deref() == Some("") {
        bail!("an empty payload is never stored");
    }
    let structure = match core.structure {
        Some(tree) => {
            rooted(&tree, "a structure document")?;
            tree
        }
        None => Fragment::empty(),
    };
    let mut fragment = Fragment::empty();
    let payload = core.payload.map(|text| fragment.put::<UTF8String, _>(text));
    let blob = core.blob.map(|bytes| fragment.put::<RawBytes, _>(bytes));
    let tool_name = core
        .tool_name
        .map(|name| fragment.put::<UTF8String, _>(name));
    let tool_namespace = core
        .tool_namespace
        .map(|name| fragment.put::<UTF8String, _>(name));
    let source_uri: Vec<_> = core
        .source_uri
        .into_iter()
        .map(|uri| fragment.put::<UTF8String, _>(uri))
        .collect();
    let title = core.title.map(|title| fragment.put::<UTF8String, _>(title));
    let media_type = core
        .media_type
        .map(|media_type| media_type.core())
        .unwrap_or_else(Fragment::empty);
    let sense = core
        .sense
        .map(|label| sense(&label))
        .unwrap_or_else(Fragment::empty);
    fragment += entity! { _ @
        schema::fact::modality?: core.modality,
        schema::fact::direction?: core.direction,
        schema::fact::payload?: payload,
        schema::fact::blob?: blob,
        schema::fact::structure*: structure,
        schema::fact::tool_name?: tool_name,
        schema::fact::tool_namespace?: tool_namespace,
        schema::fact::status?: core.status,
        schema::fact::exit_code?: core.exit_code,
        schema::fact::media_type*: media_type,
        schema::fact::source_uri*: source_uri.iter(),
        schema::fact::title?: title,
        schema::fact::sense*: sense,
        schema::fact::detail?: core.detail,
        schema::fact::register?: core.register,
        schema::fact::form?: core.form,
        schema::fact::absent?: core.absent,
        schema::fact::effort?: core.effort,
    };
    attach_kind(fragment, schema::fact::KIND, "fact")
}

/// A sense entity: `{ tag: sense::KIND, name: label }`, the label exactly as
/// she was shown it.
pub fn sense(label: &str) -> Fragment {
    let mut fragment = Fragment::empty();
    let name = fragment.put::<UTF8String, _>(label.to_owned());
    fragment += entity! {
        metadata::tag: &schema::sense::KIND,
        metadata::name: name,
    };
    fragment
}

/// One node: its predecessors and the fact(s) it holds (`holds` must export
/// them). Exactly main's node, with no timestamp; time lives on witnesses.
pub fn node(previous: impl IntoIterator<Item = Id>, holds: Fragment) -> Result<Fragment> {
    if holds.exports().next().is_none() {
        bail!("a v3 node holds at least one fact");
    }
    blockdag::block(previous, None, holds)
}

/// Annotations of a node. None of them is identity.
#[derive(Clone, Debug, Default)]
pub struct NodeAnnotations {
    /// Call node(s) this result answers.
    pub responds_to: Vec<Id>,
    /// Tail(s) of the context this root replaced.
    pub continues: Vec<Id>,
    /// Call node(s) that created this root's context.
    pub spawned_by: Vec<Id>,
    /// Original(s) this node re-presents.
    pub recalls: Vec<Id>,
    /// [`schema::node::ends`] terms.
    pub ends: Vec<Id>,
    /// [`schema::node::rendering`] terms.
    pub rendering: Vec<Id>,
}

/// The annotations of `node`, as a fragment that exports nothing.
pub fn annotate_node(node: Id, annotations: &NodeAnnotations) -> Fragment {
    unexported(entity! { ExclusiveId::force_ref(&node) @
        schema::node::responds_to*: annotations.responds_to.iter(),
        schema::node::continues*: annotations.continues.iter(),
        schema::node::spawned_by*: annotations.spawned_by.iter(),
        schema::node::recalls*: annotations.recalls.iter(),
        schema::node::ends*: annotations.ends.iter(),
        schema::node::rendering*: annotations.rendering.iter(),
    })
}

/// One witness: `source_namespace` held `node`. Exactly main's importer
/// receipt.
pub fn witness(source_namespace: Id, node: Id) -> Result<Fragment> {
    let fragment = entity! { _ @
        schema::witness::source_namespace: &source_namespace,
        schema::witness::projects_to: &node,
    };
    attach_kind(fragment, schema::witness::KIND, "witness")
}

/// One source record's claims about a witnessed node: the core of one
/// [`occurrence`].
#[derive(Clone, Debug, Default)]
pub struct WitnessAnnotations {
    /// Model labels, exactly as the source spells them, of the model that
    /// sampled the node. Empty for anything she perceived.
    pub generated_by: Vec<String>,
    pub raw_model: Option<String>,
    pub raw_role: Option<String>,
    /// Who wrote what the node holds, as the source spells it.
    pub raw_author: Vec<String>,
    pub author: Option<Id>,
    pub experiencer: Option<Id>,
    /// The session id, on leaves only.
    pub source_locator: Option<String>,
    /// Times the source asserted.
    pub timestamp: Vec<Inline<NsTAIInterval>>,
    /// The provider's reference(s) for content the source does not hold.
    pub pointer: Vec<String>,
    pub claimed_size: Option<u128>,
    pub claimed_media_type: Option<MediaType>,
    pub claimed_title: Option<String>,
    /// The source marks this occurrence a compaction summary.
    pub compaction_summary: bool,
}

/// One occurrence of `witness`: the claims of one source record, rooted at an
/// id derived from the witness and all of them, so two records' claims never
/// mix. The vendor-model and media-type entities they point at ride along.
pub fn occurrence(witness: Id, annotations: WitnessAnnotations) -> Result<Fragment> {
    let mut fragment = Fragment::empty();
    let mut generated_by = Vec::new();
    for label in &annotations.generated_by {
        let model = blockdag::vendor_model(label);
        generated_by.push(rooted(&model, "vendor model")?);
        fragment += unexported(model);
    }
    let claimed_media_type = match &annotations.claimed_media_type {
        Some(media_type) => {
            let core = media_type.core();
            let root = rooted(&core, "media type")?;
            fragment += unexported(core);
            fragment += media_type.annotations();
            Some(root)
        }
        None => None,
    };
    let put = |fragment: &mut Fragment, text: Option<String>| {
        text.map(|text| fragment.put::<UTF8String, _>(text))
    };
    let raw_model = put(&mut fragment, annotations.raw_model);
    let raw_role = put(&mut fragment, annotations.raw_role);
    let raw_author: Vec<_> = annotations
        .raw_author
        .into_iter()
        .map(|author| fragment.put::<UTF8String, _>(author))
        .collect();
    let source_locator = put(&mut fragment, annotations.source_locator);
    let claimed_title = put(&mut fragment, annotations.claimed_title);
    let pointer: Vec<_> = annotations
        .pointer
        .into_iter()
        .map(|pointer| fragment.put::<UTF8String, _>(pointer))
        .collect();
    let compaction = annotations
        .compaction_summary
        .then_some(schema::witness::COMPACTION_SUMMARY);
    let core = entity! { _ @
        schema::occurrence::of: &witness,
        schema::witness::generated_by*: generated_by.iter(),
        schema::witness::raw_model?: raw_model,
        schema::witness::raw_role?: raw_role,
        schema::witness::raw_author*: raw_author.iter(),
        schema::witness::author?: annotations.author,
        schema::witness::experiencer?: annotations.experiencer,
        schema::witness::source_locator?: source_locator,
        schema::witness::timestamp*: annotations.timestamp.iter(),
        schema::witness::pointer*: pointer.iter(),
        schema::witness::claimed_size?: annotations.claimed_size,
        schema::witness::claimed_media_type?: claimed_media_type,
        schema::witness::claimed_title?: claimed_title,
        metadata::tag?: compaction,
    };
    let core = attach_kind(core, schema::occurrence::KIND, "occurrence")?;
    let root = rooted(&core, "occurrence")?;
    fragment += unexported(core);
    let (_, facts, metafacts, blobs) = fragment.into_parts();
    Ok(Fragment::rooted_from_parts(root, facts, metafacts, blobs))
}

/// One gloss: side-model `text` of `category` about `describes`.
pub fn gloss(describes: Id, category: Id, text: &str) -> Result<Fragment> {
    if text.is_empty() {
        bail!("an empty gloss is never stored");
    }
    let mut fragment = Fragment::empty();
    let text = fragment.put::<UTF8String, _>(text.to_owned());
    fragment += entity! { _ @
        schema::gloss::describes: &describes,
        schema::gloss::category: &category,
        schema::gloss::text: text,
    };
    attach_kind(fragment, schema::gloss::KIND, "gloss")
}

/// The model that wrote `gloss`, as an annotation fragment exporting nothing.
pub fn annotate_gloss(gloss: Id, generated_by: &str) -> Result<Fragment> {
    let model = blockdag::vendor_model(generated_by);
    let model_id = rooted(&model, "vendor model")?;
    let mut fragment = unexported(model);
    fragment += unexported(entity! { ExclusiveId::force_ref(&gloss) @
        schema::gloss::generated_by: &model_id,
    });
    Ok(fragment)
}

/// One citation of `uri` by the span `start..end` of `node`'s text. It is an
/// annotation entity: rooted at its own id, and never part of the node's.
pub fn citation(node: Id, start: u64, end: u64, uri: &str) -> Result<Fragment> {
    if end < start {
        bail!("citation span ends before it starts");
    }
    let mut fragment = Fragment::empty();
    let uri = fragment.put::<UTF8String, _>(uri.to_owned());
    fragment += entity! { _ @
        schema::citation::cites: &node,
        schema::citation::start: start,
        schema::citation::end: end,
        schema::citation::uri: uri,
    };
    attach_kind(fragment, schema::citation::KIND, "citation")
}

/// Every vocabulary term as queryable data: `{ tag: KIND_TAG, name }`, and on
/// `ends` terms whether they end the message.
pub fn vocabulary_fragment() -> Fragment {
    let mut fragment = Fragment::empty();
    let terms = schema::fact::modality::SPECS
        .iter()
        .chain(schema::fact::direction::SPECS)
        .chain(schema::fact::status::SPECS)
        .chain(schema::fact::detail::SPECS)
        .chain(schema::fact::register::SPECS)
        .chain(schema::fact::form::SPECS)
        .chain(schema::fact::absent::SPECS)
        .chain(schema::fact::effort::SPECS)
        .chain(schema::node::rendering::SPECS)
        .chain(schema::gloss::category::SPECS);
    for &(id, name) in terms {
        let name = fragment.put::<UTF8String, _>(name.to_owned());
        fragment += entity! { ExclusiveId::force_ref(&id) @
            metadata::tag: &metadata::KIND_TAG,
            metadata::name: name,
        };
    }
    for &(id, name, message_final) in schema::node::ends::SPECS {
        let name = fragment.put::<UTF8String, _>(name.to_owned());
        fragment += entity! { ExclusiveId::force_ref(&id) @
            metadata::tag: &metadata::KIND_TAG,
            metadata::name: name,
            schema::node::ends::message_final: message_final,
        };
    }
    unexported(fragment)
}
