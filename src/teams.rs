//! Canonical Teams collection semantics shared by the faculty and cutovers.
//!
//! Direct configured operations live in [`operations`], with tailored [`cli`]
//! and [`mcp`] frontends. This module also owns the monotone data model:
//! intrinsic source/context/receipt
//! construction, receipt-DAG evaluation, and catalog validation. Keeping that
//! seam shared is especially important for the generation-0 legacy snapshot:
//! migrated history and the first genuine Graph page must agree on one causal
//! interpretation.

pub mod cli;
pub mod mcp;
pub mod operations;
pub mod render;

pub use operations::{
    Activity, ArchiveAccess, ArchivedMessage, AttachmentData, AttachmentGetOptions, AttachmentInfo,
    AttachmentListOptions, AttachmentLookup, AttachmentMatch, AuthProfileInput, AuthSet,
    ContextSet, DirectoryUser, InvitedMember, LoginInput, LoginReceipt, Presence, PresenceActivity,
    PresenceAvailability, PresenceSet, PresentationContext, PullReceipt, ReadOptions, SentMessage,
    Teams,
};

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{bail, Context, Result};
use hifitime::Epoch;
use triblespace::core::inline::encodings::genid::GenId;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::BlobStoreGet;
use triblespace::macros::id_hex;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

use crate::files;
use crate::schemas::archive::archive;
use crate::schemas::files::{file, KIND_FILE, KIND_MEDIA_TYPE};
use crate::schemas::teams::teams;
use crate::secrets::SecretsSnapshot;

const SNAPSHOT_COORDINATE_PREFIX: &str = "teams-legacy-snapshot-v1:";

// These identifiers were published by the legacy Teams Repository schema.
// They are recognized only to keep plaintext OAuth evidence out of the
// semantic Teams collection. The migration module remains the sole decoder of the
// retired rows.
const RETIRED_OAUTH_KINDS: [Id; 2] = [
    id_hex!("7B6DBE9FD29182D97F1699437CF6627C"),
    id_hex!("0D7F4BBE36BD0D6FF4E6C651110D6E8B"),
];
const RETIRED_OAUTH_SECRET_ATTRIBUTES: [Id; 3] = [
    id_hex!("438A29922F91F873A69C3856AA7A553F"),
    id_hex!("60C85DD37D09D3D27BC6BFA0E8040EA9"),
    id_hex!("0E734F66EBBA45ED022D1EE539B11EBE"),
];

pub type TextHandle = Inline<Handle<UTF8String>>;

/// The heads of the source-scoped receipt DAG, all of which the next page
/// supersedes, and where its pull continues.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverageFrontier {
    pub heads: BTreeSet<Id>,
    /// The greatest generation among the heads.
    pub generation: u128,
    /// Absent only for a generation-0 frozen legacy snapshot. The next pull
    /// must then use Graph's base endpoint.
    pub cursor: Option<String>,
    /// One head which completes a round: its cursor asks what is new since,
    /// and an empty answer tells nothing the head does not.
    pub settled: bool,
}

/// Complete public state of one immutable Teams authentication profile.
/// Secret-bearing values live in an explicitly configured Secrets collection;
/// these fields are exact immutable version references, never names resolved
/// by time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthProfileRecord {
    pub id: Id,
    pub source: Id,
    pub client_id: TextHandle,
    pub user_id: TextHandle,
    pub scopes: TextHandle,
    pub client_secret_version: Option<Id>,
    pub delegated_token_version: Option<Id>,
    pub predecessors: Vec<Id>,
}

/// Explicit state of a source-scoped auth-profile frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthProfileHead {
    Missing,
    Unique(Id),
    Forked(Vec<Id>),
}

/// One causally current version of a logical message.
///
/// A message ordinarily has one. Several are unordered observations of the
/// same message: none of them supersedes another, so all are current.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MessageVersion {
    Present(Id),
    Deleted(Id),
    /// A source tombstone, which carries no observation.
    Removed,
}

/// Canonical presentation of one current version of one logical Teams message.
///
/// The receipt DAG, rather than iteration order, selects the versions. Where
/// it leaves several unordered, each is its own record, numbered by
/// `version_index` among `version_count`, and none is chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentMessage {
    pub message: Id,
    pub observation: Option<Id>,
    pub chat: Id,
    pub deleted: bool,
    pub created_at: Option<Inline<NsTAIInterval>>,
    pub modified_at: Option<Inline<NsTAIInterval>>,
    pub author: Option<Id>,
    pub author_names: Vec<TextHandle>,
    pub content: Option<TextHandle>,
    pub attachments: Vec<Id>,
    pub version_index: usize,
    pub version_count: usize,
}

/// Construct the intrinsic source entity for one concrete tenant.
pub fn source_fragment(tenant: &str) -> Fragment {
    let mut source = Fragment::empty();
    let tenant = source.put::<UTF8String, _>(canonical_tenant(tenant));
    source += entity! {
        metadata::tag: teams::kind_source,
        teams::tenant_id: tenant,
    };
    source
}

/// Construct one immutable professional-context version.
pub fn context_fragment(
    source: Id,
    created_at: Inline<NsTAIInterval>,
    predecessors: impl IntoIterator<Item = Id>,
    name: &str,
    boundary: &str,
) -> Result<Fragment> {
    let name = name.trim();
    let boundary = boundary.trim();
    if name.is_empty() || boundary.is_empty() {
        bail!("Teams context name and boundary must not be empty");
    }
    let predecessors = predecessors.into_iter().collect::<BTreeSet<_>>();
    let mut fragment = Fragment::empty();
    let name = fragment.put::<UTF8String, _>(name.to_owned());
    let boundary = fragment.put::<UTF8String, _>(boundary.to_owned());
    fragment += entity! {
        metadata::tag: teams::kind_context,
        teams::source: source,
        metadata::created_at: created_at,
        metadata::supersedes*: predecessors,
        metadata::name: name,
        metadata::description: boundary,
    };
    Ok(fragment)
}

/// The content handle of `value` as text: how a closed word is compared
/// against a stored one without reading the stored blob.
fn text_handle(value: &str) -> Inline<Handle<UTF8String>> {
    value.to_owned().to_blob().get_handle()
}

fn canonical_nonempty(value: &str, field: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(value.to_owned())
}

/// Canonicalize delegated OAuth scopes as a set. OAuth scope order carries no
/// meaning, so profile identity must not depend on caller ordering.
pub fn canonical_auth_scopes(scopes: &str) -> Result<String> {
    let values = scopes
        .split_whitespace()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if values.is_empty() {
        bail!("Teams delegated scopes must not be empty");
    }
    Ok(values.into_iter().collect::<Vec<_>>().join(" "))
}

fn auth_profile_record(record: &AuthProfileRecord) -> Fragment {
    entity! {
        metadata::tag: teams::kind_auth_profile,
        teams::source: record.source,
        teams::auth_client_id: record.client_id,
        teams::auth_user_id: record.user_id,
        teams::auth_scopes: record.scopes,
        teams::auth_client_secret_version?: record.client_secret_version,
        teams::auth_delegated_token_version?: record.delegated_token_version,
        metadata::supersedes*: record.predecessors.iter(),
    }
}

/// Construct one intrinsic full-state authentication profile.
///
/// At least one exact Secrets version is required. A delegated-only profile
/// can drive user operations; adding a client-secret version also enables the
/// app-only delta endpoint. Replacing either secret produces a successor that
/// retains the independently rotating other reference.
#[allow(clippy::too_many_arguments)]
pub fn auth_profile_fragment(
    source: Id,
    client_id: &str,
    user_id: &str,
    scopes: &str,
    client_secret_version: Option<Id>,
    delegated_token_version: Option<Id>,
    predecessors: impl IntoIterator<Item = Id>,
) -> Result<(Fragment, Id)> {
    if client_secret_version.is_none() && delegated_token_version.is_none() {
        bail!("Teams auth profile requires at least one exact Secrets version");
    }
    let client_id = canonical_nonempty(client_id, "Teams client id")?;
    let user_id = canonical_nonempty(user_id, "Teams user id")?;
    let scopes = canonical_auth_scopes(scopes)?;
    let predecessors = predecessors.into_iter().collect::<BTreeSet<_>>();
    let mut fragment = Fragment::empty();
    let record = AuthProfileRecord {
        id: source,
        source,
        client_id: fragment.put::<UTF8String, _>(client_id),
        user_id: fragment.put::<UTF8String, _>(user_id),
        scopes: fragment.put::<UTF8String, _>(scopes),
        client_secret_version,
        delegated_token_version,
        predecessors: predecessors.into_iter().collect(),
    };
    let profile = auth_profile_record(&record);
    let id = profile.root().expect("Teams auth profile has one root");
    fragment += profile;
    Ok((fragment, id))
}

/// Decode one auth-profile record without selecting a current head.
///
/// Every field is part of the profile's identity, so a profile id names one
/// set of values; should a writer ever reuse the id, the smallest combination
/// is taken. A profile without its client, user, or scopes cannot authenticate.
pub fn auth_profile<P>(catalog: &P, id: Id) -> Result<AuthProfileRecord>
where
    P: TriblePattern,
{
    let (source, client_id, user_id, scopes) = find!(
        (source: Id, client_id: TextHandle, user_id: TextHandle, scopes: TextHandle),
        pattern!(catalog, [{
            id @
            teams::source: ?source,
            teams::auth_client_id: ?client_id,
            teams::auth_user_id: ?user_id,
            teams::auth_scopes: ?scopes,
        }])
    )
    .min()
    .ok_or_else(|| {
        anyhow::anyhow!("Teams auth profile {id:x} lacks its source, client id, user id, or scopes")
    })?;
    Ok(AuthProfileRecord {
        id,
        source,
        client_id,
        user_id,
        scopes,
        client_secret_version: find!(
            value: Id,
            pattern!(catalog, [{ id @ teams::auth_client_secret_version: ?value }])
        )
        .min(),
        delegated_token_version: find!(
            value: Id,
            pattern!(catalog, [{ id @ teams::auth_delegated_token_version: ?value }])
        )
        .min(),
        predecessors: find!(
            value: Id,
            pattern!(catalog, [{ id @ metadata::supersedes: ?value }])
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect(),
    })
}

pub fn auth_profile_ids<P>(catalog: &P, source: Id) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    find!(
        profile: Id,
        pattern!(catalog, [{
            ?profile @
            metadata::tag: teams::kind_auth_profile,
            teams::source: source,
        }])
    )
    .collect()
}

/// Sources for which at least one auth-profile version exists.
pub fn auth_profile_sources<P>(catalog: &P) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    find!(
        source: Id,
        pattern!(catalog, [{
            _?profile @
            metadata::tag: teams::kind_auth_profile,
            teams::source: ?source,
        }])
    )
    .collect()
}

pub fn auth_profile_head_ids<P>(catalog: &P, source: Id) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    let profiles = auth_profile_ids(catalog, source);
    let superseded = find!(
        predecessor: Id,
        pattern!(catalog, [{
            _?successor @
            metadata::tag: teams::kind_auth_profile,
            teams::source: source,
            metadata::supersedes: ?predecessor,
        }])
    )
    .collect::<BTreeSet<_>>();
    profiles.difference(&superseded).copied().collect()
}

pub fn auth_profile_head<P>(catalog: &P, source: Id) -> AuthProfileHead
where
    P: TriblePattern,
{
    let heads = auth_profile_head_ids(catalog, source)
        .into_iter()
        .collect::<Vec<_>>();
    match heads.as_slice() {
        [] => AuthProfileHead::Missing,
        [id] => AuthProfileHead::Unique(*id),
        _ => AuthProfileHead::Forked(heads),
    }
}

/// Validate the active auth-profile references against the configured Secrets
/// snapshot. This deliberately performs no name or timestamp resolution.
///
/// Superseded profiles are provenance, not live configuration. Their exact
/// secret versions may legitimately have been left behind by an additive cutover;
/// requiring those historical versions would make an additive successor
/// unable to repair the active profile.
pub fn validate_auth_secret_references<R, P>(
    teams_catalog: &P,
    secrets: &SecretsSnapshot<R>,
) -> Result<()>
where
    P: TriblePattern,
{
    validate_auth_secret_references_for_sources(
        teams_catalog,
        secrets,
        auth_profile_sources(teams_catalog),
    )
}

/// Validate active auth-profile references only for the named sources.
///
/// This is the publication-boundary form: a candidate update cannot be
/// blocked by unrelated historical profiles elsewhere in the collection.
pub fn validate_auth_secret_references_for_sources<R, P>(
    teams_catalog: &P,
    secrets: &SecretsSnapshot<R>,
    sources: impl IntoIterator<Item = Id>,
) -> Result<()>
where
    P: TriblePattern,
{
    for source in sources {
        for profile in auth_profile_head_ids(teams_catalog, source) {
            let record = auth_profile(teams_catalog, profile)?;
            for (label, secret) in [
                ("client secret", record.client_secret_version),
                ("delegated token bundle", record.delegated_token_version),
            ] {
                if let Some(secret) = secret {
                    if !secrets.contains(secret) {
                        bail!(
                            "Teams auth profile {profile:x} names unknown {label} Secrets version {secret:x}"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Optional canonical byte materialization of one attachment occurrence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachmentMaterialization {
    pub bytes: Vec<u8>,
    pub file_name: String,
    pub media_type: String,
}

/// Exact source evidence for one attachment occurrence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachmentInput {
    pub kind: String,
    pub source_id: String,
    pub name: Option<String>,
    pub source_pointers: BTreeSet<String>,
    pub materialization: Option<AttachmentMaterialization>,
}

/// One complete immutable Graph message observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageObservationInput {
    pub chat_id: String,
    pub message_id: String,
    /// Exact source representations. Distinct raw encodings of the same Graph
    /// version are additive evidence and therefore repeated values.
    pub raw: BTreeSet<String>,
    pub author_user_id: Option<String>,
    pub author_name: Option<String>,
    pub content: Option<String>,
    pub created_at: Option<Inline<NsTAIInterval>>,
    pub modified_at: Inline<NsTAIInterval>,
    pub deleted_at: Option<Inline<NsTAIInterval>>,
    pub etag: String,
    pub attachments: Vec<AttachmentInput>,
}

/// Construct a complete independently valid observation transaction.
///
/// The returned fragment includes the source identity closure expected by a
/// signed Teams COMMIT. It does not include a receipt; callers may aggregate
/// several observations and then add one exact page/snapshot receipt.
pub fn observation_fragment(
    tenant: &str,
    expected_source: Id,
    input: MessageObservationInput,
) -> Result<(Fragment, Id)> {
    if input.raw.is_empty() {
        bail!("Teams observation requires at least one raw source representation");
    }
    if input.etag.trim().is_empty() {
        bail!("Teams observation requires a Graph etag");
    }
    let deleted = input.deleted_at.is_some();
    if !deleted && (input.created_at.is_none() || input.content.is_none()) {
        bail!("present Teams observation requires created time and content");
    }

    let mut fragment = source_fragment(tenant);
    if fragment.root() != Some(expected_source) {
        bail!("Teams tenant/source identity mismatch while constructing observation");
    }
    let chat_external = fragment.put::<UTF8String, _>(input.chat_id);
    let chat = entity! {
        metadata::tag: teams::kind_chat,
        teams::source: expected_source,
        teams::chat_id: chat_external,
    };
    let chat_id = chat.root().expect("Teams chat has one root");
    fragment += chat;
    let message_external = fragment.put::<UTF8String, _>(input.message_id);
    let message = entity! {
        metadata::tag: archive::kind_message,
        teams::chat: chat_id,
        teams::message_id: message_external,
    };
    let message_id = message.root().expect("Teams message has one root");
    fragment += message;

    let author = input
        .author_user_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|external| {
            let external = fragment.put::<UTF8String, _>(external.to_owned());
            let author = entity! {
                metadata::tag: archive::kind_author,
                teams::source: expected_source,
                teams::user_id: external,
            };
            let id = author.root().expect("Teams author has one root");
            fragment += author;
            id
        });

    let mut attachment_ids = BTreeSet::new();
    for attachment in input.attachments {
        let kind = attachment.kind.trim();
        let source_id = attachment.source_id.trim();
        if !matches!(kind, "attachment" | "hosted-content") || source_id.is_empty() {
            bail!("invalid Teams attachment source evidence");
        }
        let source_handle = fragment.put::<UTF8String, _>(source_id.to_owned());
        let name_text = attachment
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let name = name_text
            .as_ref()
            .map(|name| fragment.put::<UTF8String, _>(name.clone()));
        let occurrence = entity! {
            metadata::tag: archive::kind_attachment,
            archive::attachment_source_id: source_handle,
            teams::attachment_message: message_id,
            teams::attachment_kind: kind,
            archive::attachment_name?: name,
        };
        let occurrence_id = occurrence.root().expect("Teams attachment has one root");
        fragment += occurrence;
        let pointers = attachment
            .source_pointers
            .into_iter()
            .map(|pointer| fragment.put::<UTF8String, _>(pointer))
            .collect::<BTreeSet<_>>();
        let (file_id, size) = if let Some(materialization) = attachment.materialization {
            let file_name = materialization.file_name.trim();
            if file_name.is_empty() {
                bail!("materialized Teams attachment has an empty file name");
            }
            let size: Inline<U256BE> = (materialization.bytes.len() as u128).to_inline();
            let media_type = files::normalize_media_type_or_default(&materialization.media_type);
            let file = files::stage(materialization.bytes, file_name, &media_type)?;
            let file_id = file.root().expect("canonical file has one root");
            fragment += file;
            (Some(file_id), Some(size))
        } else {
            (None, None)
        };
        fragment += entity! { ExclusiveId::force_ref(&occurrence_id) @
            archive::attachment_source_pointer*: pointers,
            archive::attachment_file?: file_id,
            archive::attachment_size_bytes?: size,
        };
        attachment_ids.insert(occurrence_id);
    }

    let etag = fragment.put::<UTF8String, _>(input.etag);
    let observation = entity! {
        metadata::tag: teams::kind_message_observation,
        teams::message: message_id,
        teams::modified_at: input.modified_at,
        teams::etag: etag,
    };
    let observation_id = observation.root().expect("Teams observation has one root");
    fragment += observation;
    let raw = input
        .raw
        .into_iter()
        .map(|raw| fragment.put::<UTF8String, _>(raw))
        .collect::<BTreeSet<_>>();
    let author_name = input
        .author_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| fragment.put::<UTF8String, _>(name.to_owned()));
    let content = input
        .content
        .map(|content| fragment.put::<UTF8String, _>(content));
    let state = if deleted { "deleted" } else { "present" };
    fragment += entity! { ExclusiveId::force_ref(&observation_id) @
        teams::message_state: state,
        metadata::created_at?: input.created_at,
        teams::deleted_at?: input.deleted_at,
        archive::author?: author,
        teams::author_name?: author_name,
        archive::content?: content,
        archive::attachment*: attachment_ids,
        teams::message_raw*: raw,
    };
    Ok((fragment, observation_id))
}

/// Canonical, inspectable coordinate of the exact frozen legacy source.
///
/// This deliberately serializes the branch/pin/head values into a UTF8String
/// instead of storing raw repository handles as trible values. Conservative
/// blob discovery therefore retains the coordinate, not the secret-bearing
/// legacy commit closure which the cutover is replacing.
pub fn snapshot_source_coordinate(branch: Id, pin: [u8; 32], head: [u8; 32]) -> String {
    format!(
        "{SNAPSHOT_COORDINATE_PREFIX}{:X}:{}:{}",
        branch,
        hex::encode_upper(pin),
        hex::encode_upper(head)
    )
}

pub fn is_canonical_snapshot_source_coordinate(value: &str) -> bool {
    let Some(rest) = value.strip_prefix(SNAPSHOT_COORDINATE_PREFIX) else {
        return false;
    };
    let mut parts = rest.split(':');
    let Some(branch) = parts.next() else {
        return false;
    };
    let Some(pin) = parts.next() else {
        return false;
    };
    let Some(head) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && branch.len() == 32
        && pin.len() == 64
        && head.len() == 64
        && [branch, pin, head].into_iter().all(|part| {
            part.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
        })
}

/// Construct a source-scoped generation-0 frozen snapshot receipt.
pub fn legacy_snapshot_fragment(
    source: Id,
    coordinate: &str,
    observations: impl IntoIterator<Item = Id>,
) -> Result<Fragment> {
    if !is_canonical_snapshot_source_coordinate(coordinate) {
        bail!("non-canonical Teams legacy snapshot source coordinate");
    }
    let observations = observations.into_iter().collect::<BTreeSet<_>>();
    let mut fragment = Fragment::empty();
    let coordinate = fragment.put::<UTF8String, _>(coordinate.to_owned());
    let generation: Inline<U256BE> = 0_u128.to_inline();
    fragment += entity! {
        metadata::tag: teams::kind_legacy_snapshot,
        teams::source: source,
        teams::coverage_generation: generation,
        teams::snapshot_source_coordinate: coordinate,
        teams::coverage_observation*: observations,
    };
    Ok(fragment)
}

/// Construct one ordinary Graph page receipt.
pub fn coverage_fragment(
    source: Id,
    generation: u128,
    predecessors: impl IntoIterator<Item = Id>,
    request: &str,
    cursor: &str,
    kind: &str,
    observations: impl IntoIterator<Item = Id>,
) -> Result<Fragment> {
    if kind != "next" && kind != "delta" {
        bail!("invalid Teams coverage cursor kind {kind:?}");
    }
    let predecessors = predecessors.into_iter().collect::<BTreeSet<_>>();
    let observations = observations.into_iter().collect::<BTreeSet<_>>();
    let mut fragment = Fragment::empty();
    let request = fragment.put::<UTF8String, _>(request.to_owned());
    let cursor = fragment.put::<UTF8String, _>(cursor.to_owned());
    let generation: Inline<U256BE> = generation.to_inline();
    fragment += entity! {
        metadata::tag: teams::kind_coverage,
        teams::source: source,
        teams::coverage_generation: generation,
        teams::coverage_request: request,
        teams::coverage_cursor: cursor,
        teams::coverage_kind: kind,
        metadata::supersedes*: predecessors,
        teams::coverage_observation*: observations,
    };
    Ok(fragment)
}

pub fn coverage_head_ids<P>(catalog: &P, source: Id) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    let receipts = receipt_ids(catalog, source);
    let superseded = find!(
        predecessor: Id,
        pattern!(catalog, [{
            _?successor @
            teams::source: source,
            metadata::supersedes: ?predecessor,
        }])
    )
    .collect::<BTreeSet<_>>();
    receipts.difference(&superseded).copied().collect()
}

/// Where the next pull of a source continues. Hosts which pulled independently
/// leave several heads; the next page supersedes all of them.
pub fn coverage_frontier<P>(
    reader: &impl BlobStoreGet,
    catalog: &P,
    source: Id,
) -> Result<Option<CoverageFrontier>>
where
    P: TriblePattern,
{
    let heads = coverage_head_ids(catalog, source);
    let mut generation = 0;
    let mut continued = None;
    for head in &heads {
        let depth = receipt_generation(catalog, *head)?;
        generation = generation.max(depth);
        // The cursor is part of the receipt's identity; only a frozen legacy
        // snapshot has none.
        let cursor = find!(
            cursor: Inline<Handle<UTF8String>>,
            pattern!(catalog, [{ head @ teams::coverage_cursor: ?cursor }])
        )
        .min();
        if let Some(cursor) = cursor {
            continued = continued.max(Some((depth, *head, cursor)));
        }
    }
    if heads.is_empty() {
        return Ok(None);
    }
    // Every head's cursor continues a complete history of its own, and what
    // the other heads observed is resident: the deepest one is as complete as
    // any, with the least to fetch again.
    let cursor = continued
        .map(|(_, _, cursor)| read_utf8string(reader, cursor, "Teams coverage cursor"))
        .transpose()?;
    let delta = text_handle("delta");
    let settled = heads.len() == 1
        && heads
            .iter()
            .all(|head| exists!(pattern!(catalog, [{ head @ teams::coverage_kind: &delta }])));
    Ok(Some(CoverageFrontier {
        heads,
        generation,
        cursor,
        settled,
    }))
}

/// Evaluate the receipt DAG and return every logical message's current
/// versions: those no later receipt and no newer source modification time
/// supersedes. The set is never empty; several versions are unordered, and
/// the caller shows them all. Unversioned tombstones are causally ordered by
/// their receipts; full observations use Graph's source modification time.
///
/// An event of a kind this reader does not know is not in the view. A
/// receipt whose predecessor has not arrived yet speaks for itself alone.
pub fn current_message_states<P>(catalog: &P, source: Id) -> BTreeMap<Id, BTreeSet<MessageVersion>>
where
    P: TriblePattern,
{
    let mut observations = BTreeMap::new();
    for (observation, message, modified) in find!(
        (
            observation: Id,
            message: Id,
            modified: Inline<NsTAIInterval>
        ),
        pattern!(catalog, [{
            ?observation @
            metadata::tag: teams::kind_message_observation,
            teams::message: ?message,
            teams::modified_at: ?modified,
        }])
    ) {
        // An observation written before the state became text carries it
        // under the retired 32-byte attribute, which this reader does not
        // model; until a backfill gives it its twin it is not in the view.
        // The state is an annotation: an observation any writer recorded as
        // deleted reads as deleted.
        let states = find!(
            value: Inline<Handle<UTF8String>>,
            pattern!(catalog, [{ observation @ teams::message_state: ?value }])
        )
        .collect::<BTreeSet<_>>();
        if states.is_empty() {
            continue;
        }
        observations.insert(
            observation,
            ObservationOrder {
                message,
                modified: interval_key(modified),
                deleted: states.contains(&text_handle("deleted")),
            },
        );
    }
    let tombstones = find!(
        (tombstone: Id, message: Id),
        pattern!(catalog, [{
            ?tombstone @
            metadata::tag: teams::kind_message_tombstone,
            teams::message: ?message,
        }])
    )
    .collect::<BTreeMap<_, _>>();

    let receipts = receipt_ids(catalog, source);
    let mut predecessors = BTreeMap::<Id, BTreeSet<Id>>::new();
    let mut successors = BTreeMap::<Id, BTreeSet<Id>>::new();
    for receipt in &receipts {
        let resident = find!(
            value: Id,
            pattern!(catalog, [{ receipt @ metadata::supersedes: ?value }])
        )
        .filter(|predecessor| predecessor != receipt && receipts.contains(predecessor))
        .collect::<BTreeSet<_>>();
        for predecessor in &resident {
            successors.entry(*predecessor).or_default().insert(*receipt);
        }
        predecessors.insert(*receipt, resident);
    }

    let mut waiting = predecessors
        .iter()
        .map(|(receipt, before)| (*receipt, before.len()))
        .collect::<BTreeMap<_, _>>();
    let mut ready = receipts
        .iter()
        .filter(|receipt| waiting[*receipt] == 0)
        .copied()
        .collect::<BTreeSet<_>>();
    let mut remaining_children = successors
        .iter()
        .map(|(receipt, after)| (*receipt, after.len()))
        .collect::<BTreeMap<_, _>>();
    let mut states = BTreeMap::<Id, BTreeMap<Id, CausalMessageState>>::new();
    // Content-derived ids cannot form a cycle. Should a writer reuse one into
    // a cycle anyway, its members are taken smallest first.
    while let Some(receipt) = ready.pop_first().or_else(|| waiting.keys().next().copied()) {
        waiting.remove(&receipt);
        for successor in successors.get(&receipt).into_iter().flatten() {
            if let Some(count) = waiting.get_mut(successor) {
                *count -= 1;
                if *count == 0 {
                    ready.insert(*successor);
                }
            }
        }

        let mut state: Option<BTreeMap<Id, CausalMessageState>> = None;
        for predecessor in &predecessors[&receipt] {
            let remaining = remaining_children
                .get_mut(predecessor)
                .expect("predecessor child count was collected");
            *remaining -= 1;
            let parent = if *remaining == 0 {
                states.remove(predecessor)
            } else {
                states.get(predecessor).cloned()
            };
            let Some(parent) = parent else { continue };
            match &mut state {
                None => state = Some(parent),
                Some(state) => merge_causal_states(state, parent),
            }
        }
        let mut state = state.unwrap_or_default();

        let mut pages = BTreeMap::<Id, PageEvents>::new();
        for event in find!(
            value: Id,
            pattern!(catalog, [{ receipt @ teams::coverage_observation: ?value }])
        ) {
            if let Some(observation) = observations.get(&event) {
                pages
                    .entry(observation.message)
                    .or_default()
                    .observations
                    .push((observation.modified, event));
            } else if let Some(message) = tombstones.get(&event) {
                pages.entry(*message).or_default().removed = true;
            }
        }
        for (message, page) in pages {
            state.entry(message).or_default().apply_page(receipt, &page);
        }
        states.insert(receipt, state);
    }

    let mut current = BTreeMap::new();
    for head in coverage_head_ids(catalog, source) {
        if let Some(state) = states.remove(&head) {
            merge_causal_states(&mut current, state);
        }
    }
    current
        .into_iter()
        .map(|(message, state)| {
            let versions = state
                .current
                .into_iter()
                .map(|event| match event {
                    CausalEvent::Observed(observation) => {
                        if observations[&observation].deleted {
                            MessageVersion::Deleted(observation)
                        } else {
                            MessageVersion::Present(observation)
                        }
                    }
                    CausalEvent::Removed(_) => MessageVersion::Removed,
                })
                .collect();
            (message, versions)
        })
        .collect()
}

/// Stable native Teams sources present in the materialized collection.
pub fn source_ids<P>(catalog: &P) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    find!(
        source: Id,
        pattern!(catalog, [{ ?source @ metadata::tag: teams::kind_source }])
    )
    .collect()
}

/// Decode the exact tenant coordinate of a native source.
pub fn source_label<P>(reader: &impl BlobStoreGet, catalog: &P, source: Id) -> Result<String>
where
    P: TriblePattern,
{
    // The tenant is the source's identity; without it the source has no label.
    let handle = find!(
        value: TextHandle,
        pattern!(catalog, [{ source @ metadata::tag: teams::kind_source, teams::tenant_id: ?value }])
    )
    .min()
    .ok_or_else(|| anyhow::anyhow!("Teams source tenant is missing"))?;
    read_utf8string(reader, handle, "Teams source tenant")
}

/// Decode every source-scoped chat identity. The external id is the chat's
/// identity, so each chat has one; should a writer ever reuse a chat id, the
/// smallest handle is shown.
pub fn chat_labels<P>(
    reader: &impl BlobStoreGet,
    catalog: &P,
    source: Id,
) -> Result<BTreeMap<Id, String>>
where
    P: TriblePattern,
{
    let mut handles = BTreeMap::<Id, TextHandle>::new();
    for (chat, handle) in find!(
        (chat: Id, handle: TextHandle),
        pattern!(catalog, [{
            ?chat @
            metadata::tag: teams::kind_chat,
            teams::source: source,
            teams::chat_id: ?handle,
        }])
    ) {
        handles
            .entry(chat)
            .and_modify(|kept| *kept = (*kept).min(handle))
            .or_insert(handle);
    }
    handles
        .into_iter()
        .map(|(chat, handle)| {
            Ok((
                chat,
                read_utf8string(reader, handle, "Teams chat external id")?,
            ))
        })
        .collect()
}

/// Evaluate the source receipt DAG and materialize its current message
/// versions as reusable presentation records.
pub fn current_messages<P>(catalog: &P, source: Id) -> Result<Vec<CurrentMessage>>
where
    P: TriblePattern,
{
    // The chat is part of the message's identity, so each message has one;
    // should a writer ever reuse a message id, the smallest chat is taken.
    let mut message_chats = BTreeMap::<Id, Id>::new();
    for (message, chat) in find!(
        (message: Id, chat: Id),
        pattern!(catalog, [
            {
                ?message @
                metadata::tag: archive::kind_message,
                teams::chat: ?chat,
                teams::message_id: _?external,
            },
            {
                ?chat @
                metadata::tag: teams::kind_chat,
                teams::source: source,
            }
        ])
    ) {
        message_chats
            .entry(message)
            .and_modify(|kept| *kept = (*kept).min(chat))
            .or_insert(chat);
    }

    let mut rows = Vec::new();
    for (message, versions) in current_message_states(catalog, source) {
        let chat = message_chats.get(&message).copied().ok_or_else(|| {
            anyhow::anyhow!("Teams message {message:x} chat in source {source:x} is missing")
        })?;
        let mut records = Vec::<CurrentMessage>::new();
        for version in versions {
            let (observation, deleted) = match version {
                MessageVersion::Present(observation) => (Some(observation), false),
                MessageVersion::Deleted(observation) => (Some(observation), true),
                MessageVersion::Removed => (None, true),
            };
            let record = current_message_record(catalog, message, chat, observation, deleted)?;
            // Versions which differ only in the source's etag present the
            // same state, which is not a conflict.
            if !records.iter().any(|kept| {
                *kept
                    == CurrentMessage {
                        observation: kept.observation,
                        ..record.clone()
                    }
            }) {
                records.push(record);
            }
        }
        let version_count = records.len();
        rows.extend(
            records
                .into_iter()
                .enumerate()
                .map(|(version_index, record)| CurrentMessage {
                    version_index,
                    version_count,
                    ..record
                }),
        );
    }
    rows.sort_by_key(|row| {
        (
            row.created_at.map(interval_key).unwrap_or(i128::MIN),
            row.message,
            row.version_index,
        )
    });
    Ok(rows)
}

fn current_message_record<P>(
    catalog: &P,
    message: Id,
    chat: Id,
    observation: Option<Id>,
    deleted: bool,
) -> Result<CurrentMessage>
where
    P: TriblePattern,
{
    let Some(observation_id) = observation else {
        return Ok(CurrentMessage {
            message,
            observation: None,
            chat,
            deleted,
            created_at: None,
            modified_at: None,
            author: None,
            author_names: Vec::new(),
            content: None,
            attachments: Vec::new(),
            version_index: 0,
            version_count: 1,
        });
    };

    // The modification time is the observation's identity. Creation time,
    // content, and author annotate it: should two writers disagree, the
    // earliest time and the smallest handle or id are shown.
    let created_at = find!(
        value: Inline<NsTAIInterval>,
        pattern!(catalog, [{ observation_id @ metadata::created_at: ?value }])
    )
    .min_by_key(|value| (interval_key(*value), *value));
    let modified_at = find!(
        value: Inline<NsTAIInterval>,
        pattern!(catalog, [{ observation_id @ teams::modified_at: ?value }])
    )
    .min_by_key(|value| (interval_key(*value), *value))
    .ok_or_else(|| anyhow::anyhow!("Teams message modification time is missing"))?;
    let content = find!(
        value: TextHandle,
        pattern!(catalog, [{ observation_id @ archive::content: ?value }])
    )
    .min();
    let author = find!(
        value: Id,
        pattern!(catalog, [{ observation_id @ archive::author: ?value }])
    )
    .min();
    let author_names = find!(
        value: TextHandle,
        pattern!(catalog, [{ observation_id @ teams::author_name: ?value }])
    )
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect();
    let attachments = find!(
        value: Id,
        pattern!(catalog, [{ observation_id @ archive::attachment: ?value }])
    )
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect();
    if !deleted && (created_at.is_none() || content.is_none()) {
        bail!("present Teams observation {observation_id:x} lacks creation time or content");
    }
    Ok(CurrentMessage {
        message,
        observation: Some(observation_id),
        chat,
        deleted,
        created_at: created_at.or(Some(modified_at)),
        modified_at: Some(modified_at),
        author,
        author_names,
        content,
        attachments,
        version_index: 0,
        version_count: 1,
    })
}

#[derive(Clone, Copy, Debug)]
struct ObservationOrder {
    message: Id,
    modified: i128,
    deleted: bool,
}

/// One message's events in one page receipt.
#[derive(Default)]
struct PageEvents {
    observations: Vec<(i128, Id)>,
    removed: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CausalEvent {
    Observed(Id),
    /// A tombstone as carried by one receipt. The tombstone entity is the
    /// same for every page; only its receipts order it.
    Removed(Id),
}

/// What one receipt's causal past says about one message.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CausalMessageState {
    /// The newest source modification time seen.
    modified: Option<i128>,
    /// The events seen: the observations of that time, and every tombstone.
    seen: BTreeSet<CausalEvent>,
    /// Those of them nothing seen supersedes.
    current: BTreeSet<CausalEvent>,
}

impl CausalMessageState {
    /// Observations older than the newest one seen are no longer versions of
    /// the message. Tombstones carry no source time and stay.
    fn advance(&mut self, modified: Option<i128>) {
        if modified > self.modified {
            self.modified = modified;
            self.seen
                .retain(|event| matches!(event, CausalEvent::Removed(_)));
            self.current
                .retain(|event| matches!(event, CausalEvent::Removed(_)));
        }
    }

    /// One page after everything this state has seen. A newer version
    /// supersedes what was current; an unseen version of the same time, or
    /// one beside a tombstone in its page, is unordered with it. A version
    /// seen before adds nothing, so a replay cannot restore it.
    fn apply_page(&mut self, receipt: Id, page: &PageEvents) {
        let newest = page
            .observations
            .iter()
            .map(|(modified, _)| *modified)
            .max();
        let newer = newest > self.modified;
        self.advance(newest);
        if newer || page.removed {
            self.current.clear();
        }
        for (modified, observation) in &page.observations {
            let event = CausalEvent::Observed(*observation);
            if Some(*modified) == self.modified && self.seen.insert(event) {
                self.current.insert(event);
            }
        }
        if page.removed {
            let event = CausalEvent::Removed(receipt);
            self.seen.insert(event);
            self.current.insert(event);
        }
    }

    /// Two concurrent pasts. An event stays current unless the other past has
    /// seen it and superseded it.
    fn merge(&mut self, mut other: Self) {
        let modified = self.modified.max(other.modified);
        self.advance(modified);
        other.advance(modified);
        self.current
            .retain(|event| other.current.contains(event) || !other.seen.contains(event));
        for event in other.current {
            if !self.seen.contains(&event) {
                self.current.insert(event);
            }
        }
        self.seen.extend(other.seen);
    }
}

fn merge_causal_states(
    target: &mut BTreeMap<Id, CausalMessageState>,
    parent: BTreeMap<Id, CausalMessageState>,
) {
    for (message, incoming) in parent {
        target.entry(message).or_default().merge(incoming);
    }
}

fn receipt_ids<P>(catalog: &P, source: Id) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    let mut receipts = find!(
        receipt: Id,
        pattern!(catalog, [{
            ?receipt @
            metadata::tag: teams::kind_coverage,
            teams::source: source,
        }])
    )
    .collect::<BTreeSet<_>>();
    receipts.extend(find!(
        receipt: Id,
        pattern!(catalog, [{
            ?receipt @
            metadata::tag: teams::kind_legacy_snapshot,
            teams::source: source,
        }])
    ));
    receipts
}

pub fn canonical_tenant(tenant: &str) -> String {
    tenant.trim().to_ascii_lowercase()
}

pub fn is_generic_tenant(tenant: &str) -> bool {
    matches!(
        tenant.trim().to_ascii_lowercase().as_str(),
        "common" | "organizations" | "consumers"
    )
}

/// Parse the timestamp shapes emitted by Microsoft Graph.
pub fn parse_graph_datetime(value: &str) -> Option<Epoch> {
    let value = value.trim();
    let (date, time) = value.split_once('T')?;
    let mut date = date.splitn(3, '-');
    let year = date.next()?.parse::<i32>().ok()?;
    let month = date.next()?.parse::<u8>().ok()?;
    let day = date.next()?.parse::<u8>().ok()?;
    let (time, offset) = parse_time_and_offset(time)?;
    let (hour, minute, second, nanos) = time;
    let mut epoch = Epoch::from_gregorian_utc(year, month, day, hour, minute, second, nanos);
    if offset != 0 {
        epoch -= hifitime::Duration::from_seconds(offset as f64);
    }
    Some(epoch)
}

fn parse_time_and_offset(value: &str) -> Option<((u8, u8, u8, u32), i32)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(value) = value.strip_suffix('Z') {
        return Some((parse_hms_fraction(value)?, 0));
    }
    if let Some((time, offset)) = split_timezone_offset(value) {
        return Some((parse_hms_fraction(time)?, parse_offset_seconds(offset)?));
    }
    Some((parse_hms_fraction(value)?, 0))
}

fn split_timezone_offset(value: &str) -> Option<(&str, &str)> {
    for index in (0..value.len()).rev() {
        let byte = value.as_bytes()[index];
        if byte == b'+' || byte == b'-' {
            let (time, offset) = value.split_at(index);
            return (offset.len() >= 3).then_some((time, offset));
        }
    }
    None
}

fn parse_offset_seconds(offset: &str) -> Option<i32> {
    let offset = offset.trim();
    let sign = if offset.starts_with('+') {
        1
    } else if offset.starts_with('-') {
        -1
    } else {
        return None;
    };
    let (hours, minutes) = offset[1..].split_once(':')?;
    Some(sign * (hours.parse::<i32>().ok()? * 3600 + minutes.parse::<i32>().ok()? * 60))
}

fn parse_hms_fraction(value: &str) -> Option<(u8, u8, u8, u32)> {
    let (hms, fraction) = value.trim().split_once('.').unwrap_or((value.trim(), ""));
    let mut hms = hms.splitn(3, ':');
    let hour = hms.next()?.parse().ok()?;
    let minute = hms.next()?.parse().ok()?;
    let second = hms.next()?.parse().ok()?;
    let nanos = if fraction.is_empty() {
        0
    } else {
        let mut digits = fraction
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            0
        } else {
            digits.truncate(9);
            while digits.len() < 9 {
                digits.push('0');
            }
            digits.parse().ok()?
        }
    };
    Some((hour, minute, second, nanos))
}

/// Extract unique hosted-content identifiers in source order from a Graph
/// message body. These references are source evidence even when their bytes
/// have not yet been materialized.
pub fn extract_hosted_content_ids(content: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let needle = "/hostedContents/";
    let mut position = 0;
    while let Some(offset) = content[position..].find(needle) {
        let start = position + offset + needle.len();
        let rest = &content[start..];
        let end = rest.find('/').unwrap_or(rest.len());
        let id = rest[..end].trim();
        if !id.is_empty() && seen.insert(id.to_owned()) {
            ids.push(id.to_owned());
        }
        position = start + end;
    }
    ids
}

pub fn interval_key(interval: Inline<NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.try_from_inline().unwrap();
    lower.to_tai_duration().total_nanoseconds()
}

fn inline_u256_to_u128(value: Inline<U256BE>) -> Result<u128> {
    let raw = value.raw;
    if raw[..16].iter().any(|byte| *byte != 0) {
        bail!("Teams coverage generation exceeds u128");
    }
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&raw[16..]);
    Ok(u128::from_be_bytes(bytes))
}

/// A receipt's generation is part of its identity. Should a writer ever reuse
/// a receipt id, the smallest generation is taken.
fn receipt_generation<P>(catalog: &P, receipt: Id) -> Result<u128>
where
    P: TriblePattern,
{
    let generation = find!(
        value: Inline<U256BE>,
        pattern!(catalog, [{ receipt @ teams::coverage_generation: ?value }])
    )
    .min()
    .ok_or_else(|| anyhow::anyhow!("Teams receipt {receipt:x} generation is missing"))?;
    inline_u256_to_u128(generation)
}

fn read_utf8string(
    reader: &impl BlobStoreGet,
    handle: Inline<Handle<UTF8String>>,
    field: &str,
) -> Result<String> {
    let view: anybytes::View<str> = reader
        .get(handle)
        .with_context(|| format!("read {field} payload {}", hex::encode_upper(handle.raw)))?;
    Ok(view.as_ref().to_owned())
}

/// Decode one Teams text attachment with field-specific context.
pub fn read_text(reader: &impl BlobStoreGet, handle: TextHandle, field: &str) -> Result<String> {
    read_utf8string(reader, handle, field)
}

/// Enforce the independently signed Teams transaction boundary.
pub fn validate_commit_fragment(facts: &TribleSet) -> Result<()> {
    reject_retired_oauth_evidence(facts)?;
    let mut receipts = find!(
        receipt: Id,
        pattern!(facts, [{ ?receipt @ metadata::tag: teams::kind_coverage }])
    )
    .collect::<BTreeSet<_>>();
    receipts.extend(find!(
        receipt: Id,
        pattern!(facts, [{ ?receipt @ metadata::tag: teams::kind_legacy_snapshot }])
    ));
    let observations = find!(
        observation: Id,
        pattern!(facts, [{ ?observation @ metadata::tag: teams::kind_message_observation }])
    )
    .collect::<BTreeSet<_>>();
    let tombstones = find!(
        tombstone: Id,
        pattern!(facts, [{ ?tombstone @ metadata::tag: teams::kind_message_tombstone }])
    )
    .collect::<BTreeSet<_>>();

    if receipts.is_empty() && observations.is_empty() && tombstones.is_empty() {
        return Ok(());
    }
    // One page is one receipt: this counts receipt entities in the staged
    // transaction, not values of an attribute.
    let receipt = match receipts.len() {
        1 => *receipts.first().expect("one receipt"),
        0 => bail!("Teams page receipt in one COMMIT is missing"),
        count => bail!(
            "Teams page receipt in one COMMIT has {count} values; refusing arbitrary selection"
        ),
    };
    let covered = find!(
        event: Id,
        pattern!(facts, [{ receipt @ teams::coverage_observation: ?event }])
    )
    .collect::<BTreeSet<_>>();
    let mut events = observations.clone();
    events.extend(tombstones.iter().copied());
    if covered != events {
        bail!("Teams page COMMIT receipt coverage does not exactly match its message events");
    }

    // The page carries the identity of every source its receipt names, and no
    // other source.
    let receipt_sources = find!(
        source: Id,
        pattern!(facts, [{ receipt @ teams::source: ?source }])
    )
    .collect::<BTreeSet<_>>();
    if receipt_sources.is_empty() {
        bail!("Teams page receipt source is missing");
    }
    let sources = find!(
        value: Id,
        pattern!(facts, [{ ?value @ metadata::tag: teams::kind_source }])
    )
    .collect::<BTreeSet<_>>();
    if sources != receipt_sources {
        bail!("Teams page COMMIT must contain exactly its receipt source identity");
    }
    for source in &sources {
        validate_source_identity(facts, *source)?;
    }

    let chats = find!(
        value: Id,
        pattern!(facts, [{ ?value @ metadata::tag: teams::kind_chat }])
    )
    .collect::<BTreeSet<_>>();
    for chat in &chats {
        validate_chat_identity(facts, *chat, &sources)?;
    }
    let authors = find!(
        value: Id,
        pattern!(facts, [{ ?value @ metadata::tag: archive::kind_author }])
    )
    .collect::<BTreeSet<_>>();
    for author in &authors {
        validate_author_identity(facts, *author, &sources)?;
    }
    let messages = find!(
        value: Id,
        pattern!(facts, [{ ?value @ metadata::tag: archive::kind_message }])
    )
    .collect::<BTreeSet<_>>();
    for message in &messages {
        validate_message_identity(facts, *message, &chats)?;
    }
    let attachments = find!(
        value: Id,
        pattern!(facts, [{ ?value @ metadata::tag: archive::kind_attachment }])
    )
    .collect::<BTreeSet<_>>();
    for attachment in &attachments {
        validate_attachment(facts, *attachment, &messages)?;
    }
    for observation in &observations {
        validate_observation(facts, *observation, &messages, &attachments, &authors)?;
    }
    for tombstone in &tombstones {
        validate_tombstone(facts, *tombstone, &messages)?;
    }
    validate_receipt_shape(facts, receipt)?;
    validate_attachment_file_structure(facts, &attachments)
}

// A staged page must carry the identity closure of every entity it names, so
// its history stays readable without borrowing facts from elsewhere. These are
// presence checks: an attribute that holds more than one value is open world.

fn validate_source_identity(facts: &TribleSet, source: Id) -> Result<()> {
    if !exists!(pattern!(facts, [{ source @ teams::tenant_id: _?value }])) {
        bail!("Teams source tenant is missing");
    }
    Ok(())
}

fn validate_chat_identity(facts: &TribleSet, chat: Id, sources: &BTreeSet<Id>) -> Result<()> {
    let named = find!(value: Id, pattern!(facts, [{ chat @ teams::source: ?value }]))
        .collect::<BTreeSet<_>>();
    if named.is_empty() {
        bail!("Teams chat source is missing");
    }
    if !named.is_subset(sources) {
        bail!("Teams chat {chat:x} names an unknown source");
    }
    if !exists!(pattern!(facts, [{ chat @ teams::chat_id: _?value }])) {
        bail!("Teams chat external id is missing");
    }
    Ok(())
}

fn validate_author_identity(facts: &TribleSet, author: Id, sources: &BTreeSet<Id>) -> Result<()> {
    let named = find!(value: Id, pattern!(facts, [{ author @ teams::source: ?value }]))
        .collect::<BTreeSet<_>>();
    if named.is_empty() {
        bail!("Teams user source is missing");
    }
    if !named.is_subset(sources) {
        bail!("Teams user {author:x} names an unknown source");
    }
    if !exists!(pattern!(facts, [{ author @ teams::user_id: _?value }])) {
        bail!("Teams user external id is missing");
    }
    Ok(())
}

fn validate_message_identity(facts: &TribleSet, message: Id, chats: &BTreeSet<Id>) -> Result<()> {
    let named = find!(value: Id, pattern!(facts, [{ message @ teams::chat: ?value }]))
        .collect::<BTreeSet<_>>();
    if named.is_empty() {
        bail!("Teams message chat is missing");
    }
    if !named.is_subset(chats) {
        bail!("Teams message {message:x} names an unknown chat");
    }
    if !exists!(pattern!(facts, [{ message @ teams::message_id: _?value }])) {
        bail!("Teams message external id is missing");
    }
    Ok(())
}

fn validate_receipt_shape(facts: &TribleSet, receipt: Id) -> Result<()> {
    let coverage = exists!(pattern!(facts, [{ receipt @ metadata::tag: teams::kind_coverage }]));
    let legacy = exists!(pattern!(facts, [{
        receipt @ metadata::tag: teams::kind_legacy_snapshot
    }]));
    let generations = find!(
        value: Inline<U256BE>,
        pattern!(facts, [{ receipt @ teams::coverage_generation: ?value }])
    )
    .map(inline_u256_to_u128)
    .collect::<Result<BTreeSet<_>>>()?;
    if generations.is_empty() {
        bail!("Teams receipt generation is missing");
    }
    let request = exists!(pattern!(facts, [{ receipt @ teams::coverage_request: _?value }]));
    let cursor = exists!(pattern!(facts, [{ receipt @ teams::coverage_cursor: _?value }]));
    let kinds = find!(
        value: Inline<Handle<UTF8String>>,
        pattern!(facts, [{ receipt @ teams::coverage_kind: ?value }])
    )
    .collect::<BTreeSet<_>>();
    let predecessors = exists!(pattern!(facts, [{ receipt @ metadata::supersedes: _?value }]));
    let coordinate = exists!(pattern!(facts, [{
        receipt @ teams::snapshot_source_coordinate: _?value
    }]));

    if coverage && legacy {
        bail!("Teams receipt {receipt:x} is both a Graph coverage receipt and a legacy snapshot");
    } else if coverage {
        if !request {
            bail!("Teams coverage request is missing");
        }
        if !cursor {
            bail!("Teams coverage cursor is missing");
        }
        if kinds.is_empty() {
            bail!("Teams coverage kind is missing");
        }
        if kinds
            .iter()
            .any(|kind| *kind != text_handle("next") && *kind != text_handle("delta"))
        {
            bail!("invalid Teams coverage kind");
        }
        if coordinate {
            bail!("Graph Teams coverage receipt carries a legacy source coordinate");
        }
    } else if legacy {
        if generations != BTreeSet::from([0])
            || request
            || cursor
            || !kinds.is_empty()
            || predecessors
        {
            bail!("legacy Teams snapshot has Graph cursor state, a predecessor, or nonzero generation");
        }
        if !coordinate {
            bail!("legacy Teams snapshot source coordinate is missing");
        }
    } else {
        bail!("unknown Teams receipt kind on {receipt:x}");
    }
    Ok(())
}

fn validate_attachment_file_structure(facts: &TribleSet, attachments: &BTreeSet<Id>) -> Result<()> {
    let referenced_files = attachments
        .iter()
        .flat_map(|attachment| {
            find!(
                value: Id,
                pattern!(facts, [{ *attachment @ archive::attachment_file: ?value }])
            )
        })
        .collect::<BTreeSet<_>>();
    let files_present = find!(
        value: Id,
        pattern!(facts, [{ ?value @ metadata::tag: KIND_FILE }])
    )
    .collect::<BTreeSet<_>>();
    let media_types = find!(
        value: Id,
        pattern!(facts, [{ ?value @ metadata::tag: KIND_MEDIA_TYPE }])
    )
    .collect::<BTreeSet<_>>();
    if !referenced_files.is_subset(&files_present) {
        let missing = referenced_files
            .difference(&files_present)
            .next()
            .expect("non-subset has a witness");
        bail!("Teams facts omit attachment file {missing:x}");
    }
    for media_type in &media_types {
        if !exists!(pattern!(facts, [{ *media_type @ metadata::name: _?value }])) {
            bail!("attachment media type name is missing");
        }
    }
    for file_id in files_present {
        if !exists!(pattern!(facts, [{ file_id @ file::content: _?value }])) {
            bail!("attachment file content is missing");
        }
        if !exists!(pattern!(facts, [{ file_id @ file::name: _?value }])) {
            bail!("attachment file name is missing");
        }
        let named = find!(value: Id, pattern!(facts, [{ file_id @ file::media_type: ?value }]))
            .collect::<BTreeSet<_>>();
        if named.is_empty() {
            bail!("attachment file media type is missing");
        }
        if let Some(media_type) = named.difference(&media_types).next() {
            bail!("Teams facts omit media-type identity {media_type:x}");
        }
    }
    Ok(())
}

fn reject_retired_oauth_evidence(catalog: &TribleSet) -> Result<()> {
    let retired_kinds = RETIRED_OAUTH_KINDS
        .into_iter()
        .map(|kind| {
            let encoded: Inline<GenId> = kind.to_inline();
            encoded.raw
        })
        .collect::<BTreeSet<_>>();

    if let Some(fact) = catalog.iter().find(|fact| {
        RETIRED_OAUTH_SECRET_ATTRIBUTES.contains(fact.a())
            || (fact.a() == &metadata::tag.id()
                && retired_kinds.contains(&fact.v::<UnknownInline>().raw))
    }) {
        bail!(
            "native Teams collection contains retired plaintext OAuth evidence on entity {:x}; migrate into an empty native collection",
            fact.e()
        );
    }
    Ok(())
}

fn validate_attachment(catalog: &TribleSet, attachment: Id, messages: &BTreeSet<Id>) -> Result<()> {
    let named = find!(
        value: Id,
        pattern!(catalog, [{ attachment @ teams::attachment_message: ?value }])
    )
    .collect::<BTreeSet<_>>();
    if named.is_empty() {
        bail!("Teams attachment message is missing");
    }
    if !named.is_subset(messages) {
        bail!("Teams attachment {attachment:x} names an unknown message");
    }
    if !exists!(pattern!(catalog, [{ attachment @ archive::attachment_source_id: _?value }])) {
        bail!("Teams attachment source id is missing");
    }
    let kinds = find!(
        value: Inline<Handle<UTF8String>>,
        pattern!(catalog, [{ attachment @ teams::attachment_kind: ?value }])
    )
    .collect::<BTreeSet<_>>();
    if kinds.is_empty() {
        bail!("Teams attachment kind is missing");
    }
    if kinds
        .iter()
        .any(|kind| *kind != text_handle("attachment") && *kind != text_handle("hosted-content"))
    {
        bail!("invalid Teams attachment kind");
    }
    let file = exists!(pattern!(catalog, [{ attachment @ archive::attachment_file: _?value }]));
    let size = exists!(pattern!(catalog, [{
        attachment @ archive::attachment_size_bytes: _?value
    }]));
    if file != size {
        bail!("Teams attachment {attachment:x} must carry file and byte size together");
    }
    Ok(())
}

fn validate_observation(
    catalog: &TribleSet,
    observation: Id,
    messages: &BTreeSet<Id>,
    attachments: &BTreeSet<Id>,
    authors: &BTreeSet<Id>,
) -> Result<()> {
    let named = find!(value: Id, pattern!(catalog, [{ observation @ teams::message: ?value }]))
        .collect::<BTreeSet<_>>();
    if named.is_empty() {
        bail!("Teams observation message is missing");
    }
    if !named.is_subset(messages) {
        bail!("Teams observation {observation:x} names an unknown message");
    }
    let states = find!(
        value: Inline<Handle<UTF8String>>,
        pattern!(catalog, [{ observation @ teams::message_state: ?value }])
    )
    .collect::<BTreeSet<_>>();
    if states.is_empty() {
        bail!("Teams observation state is missing");
    }
    if states
        .iter()
        .any(|state| *state != text_handle("present") && *state != text_handle("deleted"))
    {
        bail!("invalid Teams observation state");
    }
    let present = states.contains(&text_handle("present"));
    let deleted_state = states.contains(&text_handle("deleted"));
    let created = exists!(pattern!(catalog, [{ observation @ metadata::created_at: _?value }]));
    if !exists!(pattern!(catalog, [{ observation @ teams::modified_at: _?value }])) {
        bail!("Teams observation modified time is missing");
    }
    let deleted = exists!(pattern!(catalog, [{ observation @ teams::deleted_at: _?value }]));
    if find!(value: Id, pattern!(catalog, [{ observation @ archive::author: ?value }]))
        .any(|author| !authors.contains(&author))
    {
        bail!("Teams observation {observation:x} names an unknown author");
    }
    let content = exists!(pattern!(catalog, [{ observation @ archive::content: _?value }]));
    if !exists!(pattern!(catalog, [{ observation @ teams::etag: _?value }])) {
        bail!("Teams observation etag is missing");
    }
    let observation_attachments = find!(
        value: Id,
        pattern!(catalog, [{ observation @ archive::attachment: ?value }])
    )
    .collect::<BTreeSet<_>>();
    if !observation_attachments.is_subset(attachments) {
        bail!("Teams observation {observation:x} names an unknown attachment");
    }
    for attachment in &observation_attachments {
        let owners = find!(
            value: Id,
            pattern!(catalog, [{ *attachment @ teams::attachment_message: ?value }])
        )
        .collect::<BTreeSet<_>>();
        if owners.is_empty() {
            bail!("Teams attachment owner is missing");
        }
        if !owners.is_subset(&named) {
            bail!("Teams observation {observation:x} links an attachment owned by another message");
        }
    }
    if !exists!(pattern!(catalog, [{ observation @ teams::message_raw: _?value }])) {
        bail!("Teams observation {observation:x} has no raw source representation");
    }
    if present && (!created || !content) {
        bail!("present Teams observation {observation:x} lacks created time or content");
    }
    if present && deleted {
        bail!("present Teams observation {observation:x} carries a deletion time");
    }
    if deleted_state && !deleted {
        bail!("versioned deleted Teams observation {observation:x} lacks deletedDateTime");
    }
    Ok(())
}

fn validate_tombstone(catalog: &TribleSet, tombstone: Id, messages: &BTreeSet<Id>) -> Result<()> {
    let named = find!(value: Id, pattern!(catalog, [{ tombstone @ teams::message: ?value }]))
        .collect::<BTreeSet<_>>();
    if named.is_empty() {
        bail!("Teams tombstone message is missing");
    }
    if !named.is_subset(messages) {
        bail!("Teams tombstone {tombstone:x} names an unknown message");
    }
    let states = find!(
        value: Inline<Handle<UTF8String>>,
        pattern!(catalog, [{ tombstone @ teams::message_state: ?value }])
    )
    .collect::<BTreeSet<_>>();
    if states.is_empty() {
        bail!("Teams tombstone state is missing");
    }
    if states.iter().any(|state| *state != text_handle("deleted")) {
        bail!("invalid Teams tombstone state");
    }
    if !exists!(pattern!(catalog, [{ tombstone @ teams::message_raw: _?value }])) {
        bail!("Teams tombstone {tombstone:x} has no raw source representation");
    }
    Ok(())
}

fn context_ids<P>(catalog: &P, source: Id) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    find!(
        context: Id,
        pattern!(catalog, [{
            ?context @
            metadata::tag: teams::kind_context,
            teams::source: source,
        }])
    )
    .collect()
}

pub fn context_head_ids<P>(catalog: &P, source: Id) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    let contexts = context_ids(catalog, source);
    let superseded = find!(
        predecessor: Id,
        pattern!(catalog, [{
            _?successor @
            metadata::tag: teams::kind_context,
            teams::source: source,
            metadata::supersedes: ?predecessor,
        }])
    )
    .collect::<BTreeSet<_>>();
    contexts.difference(&superseded).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use triblespace::core::blob::encodings::UnknownBlob;
    use triblespace::core::repo::pile::Pile;
    use triblespace::core::repo::BlobStorePut;

    fn point(second: f64) -> Inline<NsTAIInterval> {
        let epoch = Epoch::from_tai_seconds(second);
        (epoch, epoch).try_to_inline().unwrap()
    }

    #[test]
    fn native_admission_rejects_retired_oauth_evidence_before_projection() {
        let retired = entity! { metadata::tag: RETIRED_OAUTH_KINDS[0] };
        let error = validate_commit_fragment(retired.facts()).unwrap_err();
        assert!(error
            .to_string()
            .contains("retired plaintext OAuth evidence"));
    }

    #[test]
    fn auth_reference_validation_uses_exact_secret_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("teams-secrets.pile");
        std::fs::File::create(&path).unwrap();
        let signer = SigningKey::from_bytes(&[0x31; 32]);
        let mut pile = Pile::open_as(&path, signer.verifying_key()).unwrap();
        let collection =
            crate::storage::open_secrets_collection(&mut pile, signer.verifying_key()).unwrap();

        let exact = crate::secrets::storage::add_secret(
            &mut pile,
            &signer,
            collection,
            "same-name",
            b"exact",
            point(2.0),
        )
        .unwrap();

        let later = crate::secrets::storage::add_secret(
            &mut pile,
            &signer,
            collection,
            "same-name",
            b"later",
            point(3.0),
        )
        .unwrap();
        assert_ne!(exact, later);

        let collection =
            crate::storage::open_secrets_collection_read(&mut pile, signer.verifying_key())
                .unwrap();
        let secrets = pollster::block_on(crate::secrets::storage::ensure_and_snapshot(
            &mut pile, collection, &signer,
        ))
        .unwrap();
        assert_eq!(secrets.open(exact, &signer).unwrap(), b"exact");

        let source_identity = source_fragment("tenant.example");
        let source = source_identity.root().unwrap();
        let (profile, _) = auth_profile_fragment(
            source,
            "client",
            "user",
            "offline_access",
            None,
            Some(exact),
            [],
        )
        .unwrap();
        let mut teams = source_identity;
        teams += profile;
        validate_auth_secret_references(teams.facts(), &secrets).unwrap();

        let unknown = Id::new([0x33; 16]).unwrap();
        let (profile, dangling_profile) = auth_profile_fragment(
            source,
            "client",
            "user",
            "offline_access",
            None,
            Some(unknown),
            [],
        )
        .unwrap();
        let mut dangling = source_fragment("tenant.example");
        dangling += profile;
        let error = validate_auth_secret_references(dangling.facts(), &secrets).unwrap_err();
        assert!(format!("{error:#}").contains(&format!("{unknown:x}")));

        let (successor, _) = auth_profile_fragment(
            source,
            "client",
            "user",
            "offline_access",
            None,
            Some(exact),
            [dangling_profile],
        )
        .unwrap();
        dangling += successor;
        validate_auth_secret_references(dangling.facts(), &secrets).unwrap();

        pile.close().unwrap();
    }

    #[test]
    fn auth_profiles_form_an_explicit_source_scoped_dag_without_clock_selection() {
        let source_identity = source_fragment("tenant.example");
        let source = source_identity.root().unwrap();
        let client_secret = Id::new([0x11; 16]).unwrap();
        let token_root = Id::new([0x22; 16]).unwrap();
        let token_left = Id::new([0x33; 16]).unwrap();
        let token_right = Id::new([0x44; 16]).unwrap();

        let (root, root_id) = auth_profile_fragment(
            source,
            "client",
            "user",
            "offline_access Chat.ReadWrite offline_access",
            Some(client_secret),
            Some(token_root),
            [],
        )
        .unwrap();
        assert!(find!(
            value: Inline<NsTAIInterval>,
            pattern!(&root, [{ root_id @ metadata::created_at: ?value }])
        )
        .next()
        .is_none());
        let root_record = auth_profile(root.facts(), root_id).unwrap();

        let (left, left_id) = auth_profile_fragment(
            source,
            "client",
            "user",
            "Chat.ReadWrite offline_access",
            Some(client_secret),
            Some(token_left),
            [root_id],
        )
        .unwrap();
        let (right, right_id) = auth_profile_fragment(
            source,
            "client",
            "user",
            "offline_access Chat.ReadWrite",
            Some(client_secret),
            Some(token_right),
            [root_id],
        )
        .unwrap();

        let mut forked = source_identity;
        forked += root;
        forked += left;
        forked += right;
        assert_eq!(
            auth_profile_head(forked.facts(), source),
            AuthProfileHead::Forked(BTreeSet::from([left_id, right_id]).into_iter().collect())
        );
        assert_eq!(
            read_text(
                &{
                    let directory = tempfile::tempdir().unwrap();
                    let path = directory.path().join("auth.pile");
                    std::fs::File::create(&path).unwrap();
                    let mut pile = Pile::open(&path).unwrap();
                    let mut blobs = forked.blobs().clone();
                    for (_, blob) in blobs.snapshot().unwrap() {
                        pile.put::<UnknownBlob, _>(blob).unwrap();
                    }
                    pile.snapshot().unwrap()
                },
                root_record.scopes,
                "test scopes",
            )
            .unwrap(),
            "Chat.ReadWrite offline_access"
        );

        let (reconciled, reconciled_id) = auth_profile_fragment(
            source,
            "client",
            "user",
            "Chat.ReadWrite offline_access",
            Some(client_secret),
            Some(token_right),
            [left_id, right_id],
        )
        .unwrap();
        forked += reconciled;
        assert_eq!(
            auth_profile_head(forked.facts(), source),
            AuthProfileHead::Unique(reconciled_id)
        );
    }

    #[test]
    fn graph_page_advances_generation_zero_snapshot_and_pointer_only_hosted_content_is_valid() {
        let tenant = "tenant.example";
        let source_identity = source_fragment(tenant);
        let source = source_identity.root().unwrap();
        let (mut snapshot_commit, observation) = observation_fragment(
            tenant,
            source,
            MessageObservationInput {
                chat_id: "chat".to_owned(),
                message_id: "message".to_owned(),
                raw: BTreeSet::from(["{\"id\":\"message\"}".to_owned()]),
                author_user_id: Some("author".to_owned()),
                author_name: Some("Author".to_owned()),
                content: Some("hello".to_owned()),
                created_at: Some(point(1.0)),
                modified_at: point(2.0),
                deleted_at: None,
                etag: "etag".to_owned(),
                attachments: vec![
                    AttachmentInput {
                        kind: "hosted-content".to_owned(),
                        source_id: "hosted".to_owned(),
                        name: None,
                        source_pointers: BTreeSet::from([
                            "https://graph.microsoft.test/hosted".to_owned()
                        ]),
                        materialization: None,
                    },
                    AttachmentInput {
                        kind: "attachment".to_owned(),
                        source_id: "materialized".to_owned(),
                        name: Some("materialized.bin".to_owned()),
                        source_pointers: BTreeSet::new(),
                        materialization: Some(AttachmentMaterialization {
                            bytes: vec![1, 2, 3],
                            file_name: "materialized.bin".to_owned(),
                            media_type: "application/octet-stream".to_owned(),
                        }),
                    },
                ],
            },
        )
        .unwrap();
        let coordinate =
            snapshot_source_coordinate(Id::new([0x11; 16]).unwrap(), [0x22; 32], [0x33; 32]);
        let snapshot = legacy_snapshot_fragment(source, &coordinate, [observation]).unwrap();
        let snapshot_id = snapshot.root().unwrap();
        snapshot_commit += snapshot;
        validate_commit_fragment(snapshot_commit.facts()).unwrap();

        let mut graph_commit = source_identity;
        let graph = coverage_fragment(
            source,
            1,
            [snapshot_id],
            "https://graph.microsoft.test/messages/delta",
            "https://graph.microsoft.test/messages/delta?cursor=one",
            "delta",
            [],
        )
        .unwrap();
        let graph_id = graph.root().unwrap();
        graph_commit += graph;
        validate_commit_fragment(graph_commit.facts()).unwrap();

        let mut catalog = snapshot_commit;
        catalog += graph_commit;
        let directory = tempfile::tempdir().unwrap();
        let pile_path = directory.path().join("teams.pile");
        std::fs::File::create(&pile_path).unwrap();
        let mut pile = Pile::open(&pile_path).unwrap();
        let mut blobs = catalog.blobs().clone();
        for (_, blob) in blobs.snapshot().unwrap() {
            pile.put::<UnknownBlob, _>(blob).unwrap();
        }
        let reader = pile.snapshot().unwrap();

        assert_eq!(
            coverage_head_ids(catalog.facts(), source),
            BTreeSet::from([graph_id])
        );
        assert_eq!(
            coverage_frontier(&reader, catalog.facts(), source).unwrap(),
            Some(CoverageFrontier {
                heads: BTreeSet::from([graph_id]),
                generation: 1,
                cursor: Some("https://graph.microsoft.test/messages/delta?cursor=one".to_owned()),
                settled: true,
            })
        );
        assert_eq!(
            current_message_states(catalog.facts(), source)
                .into_values()
                .collect::<Vec<_>>(),
            vec![BTreeSet::from([MessageVersion::Present(observation)])]
        );
        assert_eq!(source_ids(catalog.facts()), BTreeSet::from([source]));
        assert_eq!(
            source_label(&reader, catalog.facts(), source).unwrap(),
            tenant
        );
        assert_eq!(
            chat_labels(&reader, catalog.facts(), source)
                .unwrap()
                .into_values()
                .collect::<Vec<_>>(),
            vec!["chat".to_owned()]
        );
        let presented = current_messages(catalog.facts(), source).unwrap();
        assert_eq!(presented.len(), 1);
        assert_eq!(presented[0].observation, Some(observation));
        assert!(!presented[0].deleted);
        assert_eq!(
            read_text(
                &reader,
                presented[0].content.expect("present content"),
                "test content"
            )
            .unwrap(),
            "hello"
        );

        drop(reader);
        pile.close().unwrap();
    }

    #[test]
    fn an_event_of_an_unknown_kind_is_not_in_the_view_and_refuses_nothing() {
        let tenant = "tenant.example";
        let source = source_fragment(tenant).root().unwrap();
        let (mut catalog, observation) = observation_fragment(
            tenant,
            source,
            MessageObservationInput {
                chat_id: "chat".to_owned(),
                message_id: "message".to_owned(),
                raw: BTreeSet::from(["{\"id\":\"message\"}".to_owned()]),
                author_user_id: None,
                author_name: None,
                content: Some("hello".to_owned()),
                created_at: Some(point(1.0)),
                modified_at: point(2.0),
                deleted_at: None,
                etag: "etag".to_owned(),
                attachments: Vec::new(),
            },
        )
        .unwrap();
        catalog += coverage_fragment(
            source,
            1,
            [],
            "https://graph.microsoft.test/messages/delta",
            "https://graph.microsoft.test/messages/delta?cursor=one",
            "delta",
            [observation, Id::new([0x77; 16]).unwrap()],
        )
        .unwrap();
        assert_eq!(
            current_message_states(catalog.facts(), source)
                .into_values()
                .collect::<Vec<_>>(),
            vec![BTreeSet::from([MessageVersion::Present(observation)])]
        );
    }

    #[test]
    fn graph_datetime_and_hosted_content_parsers_are_shared_source_semantics() {
        let utc = parse_graph_datetime("2026-08-09T12:34:56.1234567Z").unwrap();
        let offset = parse_graph_datetime("2026-08-09T14:34:56.1234567+02:00").unwrap();
        assert_eq!(utc, offset);
        assert_eq!(
            extract_hosted_content_ids(
                "a /hostedContents/one/$value b /hostedContents/two/$value c /hostedContents/one/$value"
            ),
            vec!["one".to_owned(), "two".to_owned()]
        );
    }

    fn page(observations: &[(i128, Id)], removed: bool) -> PageEvents {
        PageEvents {
            observations: observations.to_vec(),
            removed,
        }
    }

    fn merged(left: &CausalMessageState, right: &CausalMessageState) -> BTreeSet<CausalEvent> {
        let mut one = left.clone();
        one.merge(right.clone());
        let mut other = right.clone();
        other.merge(left.clone());
        assert_eq!(one, other, "a merge has no order");
        let mut again = one.clone();
        again.merge(right.clone());
        assert_eq!(again, one, "a merge is idempotent");
        one.current
    }

    #[test]
    fn a_merge_orders_full_versions_but_not_unseen_tombstones() {
        use CausalEvent::{Observed, Removed};
        let [first, second, root, left, right] =
            [1, 2, 3, 4, 5].map(|byte| Id::new([byte; 16]).unwrap());
        let mut base = CausalMessageState::default();
        base.apply_page(root, &page(&[(10, first)], false));

        let mut edited = base.clone();
        edited.apply_page(left, &page(&[(11, second)], false));
        assert_eq!(merged(&base, &edited), BTreeSet::from([Observed(second)]));

        // Removed knowing the version the other side still shows: removed.
        let mut removed = base.clone();
        removed.apply_page(right, &page(&[], true));
        assert_eq!(merged(&base, &removed), BTreeSet::from([Removed(right)]));

        // Edited and removed concurrently: unordered.
        assert_eq!(
            merged(&edited, &removed),
            BTreeSet::from([Observed(second), Removed(right)])
        );

        // Two versions of one time are unordered.
        let mut tie = base.clone();
        tie.apply_page(left, &page(&[(10, second)], false));
        assert_eq!(
            tie.current,
            BTreeSet::from([Observed(first), Observed(second)])
        );
        // Seeing one of them again settles nothing; a newer version does.
        tie.apply_page(right, &page(&[(10, first)], false));
        assert_eq!(tie.current.len(), 2);
        tie.apply_page(right, &page(&[(12, root)], false));
        assert_eq!(tie.current, BTreeSet::from([Observed(root)]));

        // A restoration which saw the removal supersedes it in every merge.
        let mut restored = removed.clone();
        restored.apply_page(left, &page(&[(11, second)], false));
        assert_eq!(
            merged(&removed, &restored),
            BTreeSet::from([Observed(second)])
        );
    }
}
