//! Collection-native Headspace state and its fork-visible snapshot algebra.
//!
//! Headspace has exactly two kinds of changing state: one global config track
//! and one profile track per authored profile anchor. Every change is a
//! complete snapshot. `metadata::supersedes` carries lineage, set
//! union preserves concurrent heads, and wall-clock time is absent from state
//! and identity. If occurrence provenance later has a concrete consumer, it
//! belongs outside these snapshots. This module is the sole projector used by
//! the CLI, viewer, Web, and Triage; storage and effects remain at those
//! command boundaries.
//!
//! Native records contain only exact immutable Secrets-version ids. Generic
//! secret labels remain discovery/display conveniences and never participate
//! in runtime selection. Historical plaintext facts coexist in the same
//! collection after migration, but lack the live marker and are semantically
//! inert. Every reader, reconciliation included, queries the typed snapshots
//! it needs at its point of use; nothing loads the collection's history into
//! a catalog first.

pub mod cli;
pub mod mcp;
pub mod operations;
pub use operations::{
    AddProfileOptions, Credential, CredentialUpdateError, Headspace, OptionalProfileField,
    ProfileEdit, SecretRole,
};

use std::collections::{BTreeMap, BTreeSet};

use anybytes::View;
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::core::repo::{BlobStoreGet, BlobStoreMeta};
use triblespace::macros::{entity, find, pattern};
use triblespace::prelude::*;

use crate::schemas::cognition::DEFAULT_SCOPE_ID as COGNITION_SCOPE_ID;
use crate::schemas::headspace::{
    playground_config, DEFAULT_AUTHOR, DEFAULT_AUTHOR_ROLE, DEFAULT_BASE_URL,
    DEFAULT_CHARS_PER_TOKEN, DEFAULT_CONTEXT_SAFETY_MARGIN_TOKENS, DEFAULT_CONTEXT_WINDOW_TOKENS,
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MODEL, DEFAULT_POLL_MS, DEFAULT_STREAM,
    DEFAULT_SYSTEM_PROMPT, KIND_CONFIG_ID, KIND_LIVE_RECORD, KIND_MODEL_PROFILE_ID,
    KIND_PROFILE_ANCHOR_ID,
};
use crate::secrets::SecretsSnapshot;

pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type CountValue = Inline<inlineencodings::U256BE>;

/// One complete model-profile value.
///
/// `model_secret_version` names one exact immutable Secrets version. It is
/// deliberately not a label, scope, or "latest" selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileValue {
    pub anchor: Id,
    pub name: String,
    pub model: String,
    pub base_url: String,
    pub model_secret_version: Option<Id>,
    pub reasoning_effort: Option<String>,
    pub stream: bool,
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub context_safety_margin_tokens: u64,
    pub chars_per_token: u64,
}

/// One complete global Headspace configuration value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigValue {
    pub active_profile: Id,
    pub system_prompt: String,
    pub cognition_scope: Id,
    pub author: String,
    pub author_role: String,
    pub persona: Option<Id>,
    pub poll_ms: u64,
    pub tavily_secret_version: Option<Id>,
    pub exa_secret_version: Option<Id>,
    pub exec_default_cwd: Option<String>,
    pub exec_sandbox_profile: Option<Id>,
}

/// One live or historical immutable snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot<T> {
    pub id: Id,
    pub value: T,
    pub predecessors: Vec<Id>,
}

/// Current state of one snapshot track.
///
/// `Agreed` retains every provenance head even though their complete values are
/// equal. A successor must therefore use [`Self::head_ids`] rather than taking
/// one representative.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resolution<T> {
    Missing,
    Unique(Snapshot<T>),
    Agreed(Vec<Snapshot<T>>),
    Forked(Vec<Snapshot<T>>),
}

impl<T> Resolution<T> {
    pub fn head_ids(&self) -> Vec<Id> {
        match self {
            Self::Missing => Vec::new(),
            Self::Unique(snapshot) => vec![snapshot.id],
            Self::Agreed(snapshots) | Self::Forked(snapshots) => {
                snapshots.iter().map(|snapshot| snapshot.id).collect()
            }
        }
    }

    pub fn snapshots(&self) -> &[Snapshot<T>] {
        match self {
            Self::Unique(snapshot) => std::slice::from_ref(snapshot),
            Self::Agreed(snapshots) | Self::Forked(snapshots) => snapshots,
            Self::Missing => &[],
        }
    }

    pub fn settled_value(&self, label: &str) -> Result<Option<&T>> {
        match self {
            Self::Missing => Ok(None),
            Self::Unique(snapshot) => Ok(Some(&snapshot.value)),
            Self::Agreed(snapshots) => Ok(snapshots.first().map(|snapshot| &snapshot.value)),
            Self::Forked(snapshots) => bail!(
                "{label} is forked across heads {}",
                format_ids(snapshots.iter().map(|snapshot| snapshot.id))
            ),
        }
    }
}

/// Decrypted credentials for one already-resolved Headspace state.
///
/// This value is intentionally ephemeral. No constructor can turn it back
/// into a Headspace fragment, which keeps plaintext out of native records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenedSecrets {
    pub model_api_key: Option<String>,
    pub tavily_api_key: Option<String>,
    pub exa_api_key: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CurrentProfileRow {
    id: Id,
    anchor: Id,
    name: TextHandle,
    model: TextHandle,
    base_url: TextHandle,
    stream: u64,
    context_window_tokens: u64,
    max_output_tokens: u64,
    context_safety_margin_tokens: u64,
    chars_per_token: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CurrentConfigRow {
    id: Id,
    active_profile: Id,
    system_prompt: TextHandle,
    cognition_scope: Id,
    author: TextHandle,
    author_role: TextHandle,
    poll_ms: u64,
}

fn optional_variants<T: Copy + Ord>(values: impl IntoIterator<Item = T>) -> Vec<Option<T>> {
    let values: BTreeSet<_> = values.into_iter().collect();
    if values.is_empty() {
        vec![None]
    } else {
        values.into_iter().map(Some).collect()
    }
}

fn direct_predecessors<P>(facts: &P, id: Id) -> Vec<Id>
where
    P: TriblePattern + ?Sized,
{
    sorted_ids(find!(
        predecessor: Id,
        pattern!(facts, [{ id @ metadata::supersedes: ?predecessor }])
    ))
}

/// Resolve one explicitly scoped observation-DAG frontier.
///
/// Only complete typed candidates participate. Incomplete rows, unknown
/// schema generations, and cross-track edges are therefore inert rather than
/// poisoning an open-world reader. Immediate edges suffice because every
/// candidate named by another typed candidate is below the frontier.
fn typed_frontier<P>(facts: &P, candidates: &BTreeSet<Id>) -> BTreeSet<Id>
where
    P: TriblePattern + ?Sized,
{
    let mut superseded = BTreeSet::new();
    for successor in candidates {
        superseded.extend(
            find!(
                predecessor: Id,
                pattern!(facts, [{ successor @ metadata::supersedes: ?predecessor }])
            )
            .filter(|predecessor| candidates.contains(predecessor)),
        );
    }
    candidates.difference(&superseded).copied().collect()
}

fn resolve_variants<T: Clone + Eq>(snapshots: Vec<Snapshot<T>>) -> Resolution<T> {
    match snapshots.as_slice() {
        [] => Resolution::Missing,
        [snapshot] => Resolution::Unique(snapshot.clone()),
        _ if snapshots
            .iter()
            .all(|snapshot| snapshot.value == snapshots[0].value) =>
        {
            Resolution::Agreed(snapshots)
        }
        _ => Resolution::Forked(snapshots),
    }
}

fn current_profile_rows<P>(facts: &P, anchor: Id) -> BTreeSet<CurrentProfileRow>
where
    P: TriblePattern + ?Sized,
{
    find!(
        (
            id: Id,
            name: TextHandle,
            model: TextHandle,
            base_url: TextHandle,
            stream: u64,
            context_window_tokens: u64,
            max_output_tokens: u64,
            context_safety_margin_tokens: u64,
            chars_per_token: u64
        ),
        pattern!(facts, [{
            ?id @
                metadata::tag: KIND_LIVE_RECORD,
                metadata::tag: KIND_MODEL_PROFILE_ID,
                playground_config::model_profile_id: &anchor,
                metadata::name: ?name,
                playground_config::model_name: ?model,
                playground_config::model_base_url: ?base_url,
                playground_config::model_stream: ?stream,
                playground_config::model_context_window_tokens: ?context_window_tokens,
                playground_config::model_max_output_tokens: ?max_output_tokens,
                playground_config::model_context_safety_margin_tokens: ?context_safety_margin_tokens,
                playground_config::model_chars_per_token: ?chars_per_token,
        }])
    )
    .filter_map(
        |(
            id,
            name,
            model,
            base_url,
            stream,
            context_window_tokens,
            max_output_tokens,
            context_safety_margin_tokens,
            chars_per_token,
        )| {
            (stream <= 1).then_some(CurrentProfileRow {
                id,
                anchor,
                name,
                model,
                base_url,
                stream,
                context_window_tokens,
                max_output_tokens,
                context_safety_margin_tokens,
                chars_per_token,
            })
        },
    )
    .collect()
}

/// Project the current typed state of one profile track.
///
/// Scalar multiplicity yields multiple interpretations instead of a hidden
/// cardinality error or iterator-order winner. Additional facts are ignored,
/// entity ids remain opaque, and only frontier payloads are read.
pub fn current_profile<P>(
    reader: &impl BlobStoreGet,
    facts: &P,
    anchor: Id,
) -> Result<Resolution<ProfileValue>>
where
    P: TriblePattern + ?Sized,
{
    let rows = current_profile_rows(facts, anchor);
    let candidates = rows.iter().map(|row| row.id).collect::<BTreeSet<_>>();
    let mut snapshots = Vec::new();
    for id in typed_frontier(facts, &candidates) {
        for snapshot in profile_readings(reader, facts, &rows, id)? {
            if !snapshots.contains(&snapshot) {
                snapshots.push(snapshot);
            }
        }
    }
    Ok(resolve_variants(snapshots))
}

/// Every complete reading of one typed profile snapshot on one track.
fn profile_readings<P>(
    reader: &impl BlobStoreGet,
    facts: &P,
    rows: &BTreeSet<CurrentProfileRow>,
    id: Id,
) -> Result<Vec<Snapshot<ProfileValue>>>
where
    P: TriblePattern + ?Sized,
{
    let mut snapshots = Vec::new();
    let predecessors = direct_predecessors(facts, id);
    let model_secrets = optional_variants(find!(
        value: Id,
        pattern!(facts, [{ id @ playground_config::model_secret_version: ?value }])
    ));
    let reasoning_efforts = optional_variants(find!(
        value: TextHandle,
        pattern!(facts, [{ id @ playground_config::model_reasoning_effort: ?value }])
    ));
    for row in rows.iter().filter(|row| row.id == id) {
        let name = load_text_from(reader, row.name)?;
        let model = load_text_from(reader, row.model)?;
        let base_url = load_text_from(reader, row.base_url)?;
        for model_secret_version in &model_secrets {
            for reasoning_effort in &reasoning_efforts {
                let value = ProfileValue {
                    anchor: row.anchor,
                    name: name.clone(),
                    model: model.clone(),
                    base_url: base_url.clone(),
                    model_secret_version: *model_secret_version,
                    reasoning_effort: reasoning_effort
                        .map(|handle| load_text_from(reader, handle))
                        .transpose()?,
                    stream: row.stream == 1,
                    context_window_tokens: row.context_window_tokens,
                    max_output_tokens: row.max_output_tokens,
                    context_safety_margin_tokens: row.context_safety_margin_tokens,
                    chars_per_token: row.chars_per_token,
                };
                let snapshot = Snapshot {
                    id,
                    value,
                    predecessors: predecessors.clone(),
                };
                if !snapshots.contains(&snapshot) {
                    snapshots.push(snapshot);
                }
            }
        }
    }
    Ok(snapshots)
}

/// Project every declared or typed profile track independently at its current
/// frontier. This explicit roster result is for callers which actually need
/// every profile; point readers should call [`current_profile`] instead.
pub fn current_profiles<P>(
    reader: &impl BlobStoreGet,
    facts: &P,
) -> Result<BTreeMap<Id, Resolution<ProfileValue>>>
where
    P: TriblePattern + ?Sized,
{
    let declared = find!(
        anchor: Id,
        pattern!(facts, [{
            ?anchor @
                metadata::tag: KIND_LIVE_RECORD,
                metadata::tag: KIND_PROFILE_ANCHOR_ID,
        }])
    );
    let referenced = find!(
        anchor: Id,
        pattern!(facts, [{
            _?snapshot @
                metadata::tag: KIND_LIVE_RECORD,
                metadata::tag: KIND_MODEL_PROFILE_ID,
                playground_config::model_profile_id: ?anchor,
        }])
    );
    declared
        .chain(referenced)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|anchor| current_profile(reader, facts, anchor).map(|profile| (anchor, profile)))
        .collect()
}

fn current_config_rows<P>(facts: &P) -> BTreeSet<CurrentConfigRow>
where
    P: TriblePattern + ?Sized,
{
    find!(
        (
            id: Id,
            active_profile: Id,
            system_prompt: TextHandle,
            cognition_scope: Id,
            author: TextHandle,
            author_role: TextHandle,
            poll_ms: u64
        ),
        pattern!(facts, [{
            ?id @
                metadata::tag: KIND_LIVE_RECORD,
                metadata::tag: KIND_CONFIG_ID,
                playground_config::active_model_profile_id: ?active_profile,
                playground_config::system_prompt: ?system_prompt,
                playground_config::cognition_scope: ?cognition_scope,
                playground_config::author: ?author,
                playground_config::author_role: ?author_role,
                playground_config::poll_ms: ?poll_ms,
        }])
    )
    .map(
        |(id, active_profile, system_prompt, cognition_scope, author, author_role, poll_ms)| {
            CurrentConfigRow {
                id,
                active_profile,
                system_prompt,
                cognition_scope,
                author,
                author_role,
                poll_ms,
            }
        },
    )
    .collect()
}

/// Project the global current Headspace configuration directly from typed
/// rows. Only its explicit observation DAG chooses the frontier.
pub fn current_config<P>(reader: &impl BlobStoreGet, facts: &P) -> Result<Resolution<ConfigValue>>
where
    P: TriblePattern + ?Sized,
{
    let rows = current_config_rows(facts);
    let candidates = rows.iter().map(|row| row.id).collect::<BTreeSet<_>>();
    let mut snapshots = Vec::new();
    for id in typed_frontier(facts, &candidates) {
        for snapshot in config_readings(reader, facts, &rows, id)? {
            if !snapshots.contains(&snapshot) {
                snapshots.push(snapshot);
            }
        }
    }
    Ok(resolve_variants(snapshots))
}

/// Every complete reading of one typed config snapshot.
fn config_readings<P>(
    reader: &impl BlobStoreGet,
    facts: &P,
    rows: &BTreeSet<CurrentConfigRow>,
    id: Id,
) -> Result<Vec<Snapshot<ConfigValue>>>
where
    P: TriblePattern + ?Sized,
{
    let mut snapshots = Vec::new();
    let predecessors = direct_predecessors(facts, id);
    let personas = optional_variants(find!(
        value: Id,
        pattern!(facts, [{ id @ playground_config::persona_id: ?value }])
    ));
    let tavily_secrets = optional_variants(find!(
        value: Id,
        pattern!(facts, [{ id @ playground_config::tavily_secret_version: ?value }])
    ));
    let exa_secrets = optional_variants(find!(
        value: Id,
        pattern!(facts, [{ id @ playground_config::exa_secret_version: ?value }])
    ));
    let exec_cwds = optional_variants(find!(
        value: TextHandle,
        pattern!(facts, [{ id @ playground_config::exec_default_cwd: ?value }])
    ));
    let sandbox_profiles = optional_variants(find!(
        value: Id,
        pattern!(facts, [{ id @ playground_config::exec_sandbox_profile: ?value }])
    ));
    for row in rows.iter().filter(|row| row.id == id) {
        let system_prompt = load_text_from(reader, row.system_prompt)?;
        let author = load_text_from(reader, row.author)?;
        let author_role = load_text_from(reader, row.author_role)?;
        for persona in &personas {
            for tavily_secret_version in &tavily_secrets {
                for exa_secret_version in &exa_secrets {
                    for exec_cwd in &exec_cwds {
                        for exec_sandbox_profile in &sandbox_profiles {
                            let value = ConfigValue {
                                active_profile: row.active_profile,
                                system_prompt: system_prompt.clone(),
                                cognition_scope: row.cognition_scope,
                                author: author.clone(),
                                author_role: author_role.clone(),
                                persona: *persona,
                                poll_ms: row.poll_ms,
                                tavily_secret_version: *tavily_secret_version,
                                exa_secret_version: *exa_secret_version,
                                exec_default_cwd: exec_cwd
                                    .map(|handle| load_text_from(reader, handle))
                                    .transpose()?,
                                exec_sandbox_profile: *exec_sandbox_profile,
                            };
                            let snapshot = Snapshot {
                                id,
                                value,
                                predecessors: predecessors.clone(),
                            };
                            if !snapshots.contains(&snapshot) {
                                snapshots.push(snapshot);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(snapshots)
}

/// Validate only exact Secrets references visible at selected current
/// frontiers. Historical snapshots remain irrelevant to ordinary reads.
pub fn validate_current_secret_references<'a, R>(
    config: &Resolution<ConfigValue>,
    profiles: impl IntoIterator<Item = &'a Resolution<ProfileValue>>,
    secrets: &SecretsSnapshot<R>,
) -> Result<()> {
    for snapshot in config.snapshots() {
        for (role, secret) in [
            ("Tavily", snapshot.value.tavily_secret_version),
            ("Exa", snapshot.value.exa_secret_version),
        ] {
            if let Some(secret) = secret.filter(|secret| !secrets.contains(*secret)) {
                bail!(
                    "Headspace snapshot {:x} references missing exact {role} Secrets version {secret:x}",
                    snapshot.id
                );
            }
        }
    }
    for profile in profiles {
        for snapshot in profile.snapshots() {
            if let Some(secret) = snapshot
                .value
                .model_secret_version
                .filter(|secret| !secrets.contains(*secret))
            {
                bail!(
                    "Headspace snapshot {:x} references missing exact model Secrets version {secret:x}",
                    snapshot.id
                );
            }
        }
    }
    Ok(())
}

/// Every complete typed snapshot id on any track, current or historical,
/// for resolving an explicitly named snapshot.
pub fn snapshot_ids<P>(facts: &P) -> BTreeSet<Id>
where
    P: TriblePattern + ?Sized,
{
    let anchors = find!(
        anchor: Id,
        pattern!(facts, [{
            _?snapshot @
                metadata::tag: KIND_LIVE_RECORD,
                metadata::tag: KIND_MODEL_PROFILE_ID,
                playground_config::model_profile_id: ?anchor,
        }])
    )
    .collect::<BTreeSet<_>>();
    let mut ids = current_config_rows(facts)
        .into_iter()
        .map(|row| row.id)
        .collect::<BTreeSet<_>>();
    for anchor in anchors {
        ids.extend(
            current_profile_rows(facts, anchor)
                .into_iter()
                .map(|row| row.id),
        );
    }
    ids
}

/// Re-express one explicitly chosen complete snapshot as the intended value
/// while superseding every current head on its track.
///
/// Only the chosen snapshot and its track's current frontier are read. A
/// single head already carrying the intended value is a no-op. Several
/// equal-value heads still need an explicit join so the DAG itself is
/// reconciled rather than merely semantically agreed. A chosen id with
/// several readings (several values for one field, or several tracks) names
/// no single value, so the choice is refused rather than given a hidden
/// winner.
pub fn reconcile_fragment<P>(
    reader: &impl BlobStoreGet,
    facts: &P,
    chosen: Id,
) -> Result<Option<(Fragment, Id)>>
where
    P: TriblePattern + ?Sized,
{
    let config_rows = current_config_rows(facts);
    let configs = if config_rows.iter().any(|row| row.id == chosen) {
        config_readings(reader, facts, &config_rows, chosen)?
    } else {
        Vec::new()
    };
    let mut profiles = Vec::new();
    for anchor in find!(
        anchor: Id,
        pattern!(facts, [{
            chosen @
                metadata::tag: KIND_LIVE_RECORD,
                metadata::tag: KIND_MODEL_PROFILE_ID,
                playground_config::model_profile_id: ?anchor,
        }])
    )
    .collect::<BTreeSet<_>>()
    {
        let rows = current_profile_rows(facts, anchor);
        profiles.extend(profile_readings(reader, facts, &rows, chosen)?);
    }
    match (configs.as_slice(), profiles.as_slice()) {
        ([], []) => bail!("unknown Headspace snapshot {chosen:x}"),
        ([snapshot], []) => {
            let current = current_config(reader, facts)?;
            if resolution_is_unique_value(&current, &snapshot.value) {
                return Ok(None);
            }
            config_snapshot_fragment(&snapshot.value, &current.head_ids()).map(Some)
        }
        ([], [snapshot]) => {
            let current = current_profile(reader, facts, snapshot.value.anchor)?;
            if resolution_is_unique_value(&current, &snapshot.value) {
                return Ok(None);
            }
            profile_snapshot_fragment(&snapshot.value, &current.head_ids()).map(Some)
        }
        _ => bail!(
            "Headspace snapshot {chosen:x} has {} readings; choose a snapshot with one complete value",
            configs.len() + profiles.len()
        ),
    }
}

fn resolution_is_unique_value<T: Eq>(resolution: &Resolution<T>, intended: &T) -> bool {
    matches!(resolution, Resolution::Unique(snapshot) if &snapshot.value == intended)
}

pub fn default_profile(anchor: Id, name: impl Into<String>) -> ProfileValue {
    ProfileValue {
        anchor,
        name: name.into(),
        model: DEFAULT_MODEL.to_owned(),
        base_url: DEFAULT_BASE_URL.to_owned(),
        model_secret_version: None,
        reasoning_effort: None,
        stream: DEFAULT_STREAM,
        context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        context_safety_margin_tokens: DEFAULT_CONTEXT_SAFETY_MARGIN_TOKENS,
        chars_per_token: DEFAULT_CHARS_PER_TOKEN,
    }
}

pub fn default_config(active_profile: Id) -> ConfigValue {
    ConfigValue {
        active_profile,
        system_prompt: DEFAULT_SYSTEM_PROMPT.to_owned(),
        cognition_scope: COGNITION_SCOPE_ID,
        author: DEFAULT_AUTHOR.to_owned(),
        author_role: DEFAULT_AUTHOR_ROLE.to_owned(),
        persona: None,
        poll_ms: DEFAULT_POLL_MS,
        tavily_secret_version: None,
        exa_secret_version: None,
        exec_default_cwd: Some("/workspace".to_owned()),
        exec_sandbox_profile: None,
    }
}

pub fn profile_anchor_fragment(anchor: Id) -> Fragment {
    entity! { ExclusiveId::force_ref(&anchor) @
        metadata::tag: &KIND_LIVE_RECORD,
        metadata::tag: &KIND_PROFILE_ANCHOR_ID,
    }
}

pub fn profile_snapshot_fragment(
    value: &ProfileValue,
    predecessors: &[Id],
) -> Result<(Fragment, Id)> {
    validate_profile_value(value)?;
    let predecessors = sorted_ids(predecessors.iter().copied());
    let mut fragment = Fragment::empty();
    let name = fragment.put(value.name.clone());
    let model = fragment.put(value.model.clone());
    let base_url = fragment.put(value.base_url.clone());
    let reasoning_effort = value
        .reasoning_effort
        .clone()
        .map(|value| fragment.put(value));
    fragment += entity! {
        metadata::tag: &KIND_LIVE_RECORD,
        metadata::tag: &KIND_MODEL_PROFILE_ID,
        playground_config::model_profile_id: &value.anchor,
        metadata::name: name,
        playground_config::model_name: model,
        playground_config::model_base_url: base_url,
        playground_config::model_secret_version?: value.model_secret_version,
        playground_config::model_reasoning_effort?: reasoning_effort,
        playground_config::model_stream: bool_count(value.stream),
        playground_config::model_context_window_tokens: value.context_window_tokens.to_inline(),
        playground_config::model_max_output_tokens: value.max_output_tokens.to_inline(),
        playground_config::model_context_safety_margin_tokens: value.context_safety_margin_tokens.to_inline(),
        playground_config::model_chars_per_token: value.chars_per_token.to_inline(),
        metadata::supersedes*: predecessors.iter(),
    };
    let id = fragment
        .root()
        .ok_or_else(|| anyhow!("profile snapshot has no unique intrinsic root"))?;
    Ok((fragment, id))
}

pub fn config_snapshot_fragment(
    value: &ConfigValue,
    predecessors: &[Id],
) -> Result<(Fragment, Id)> {
    validate_config_value(value)?;
    let predecessors = sorted_ids(predecessors.iter().copied());
    let mut fragment = Fragment::empty();
    let system_prompt = fragment.put(value.system_prompt.clone());
    let author = fragment.put(value.author.clone());
    let author_role = fragment.put(value.author_role.clone());
    let exec_default_cwd = value
        .exec_default_cwd
        .clone()
        .map(|value| fragment.put(value));
    fragment += entity! {
        metadata::tag: &KIND_LIVE_RECORD,
        metadata::tag: &KIND_CONFIG_ID,
        playground_config::active_model_profile_id: &value.active_profile,
        playground_config::system_prompt: system_prompt,
        playground_config::cognition_scope: &value.cognition_scope,
        playground_config::author: author,
        playground_config::author_role: author_role,
        playground_config::persona_id?: value.persona,
        playground_config::poll_ms: value.poll_ms.to_inline(),
        playground_config::tavily_secret_version?: value.tavily_secret_version,
        playground_config::exa_secret_version?: value.exa_secret_version,
        playground_config::exec_default_cwd?: exec_default_cwd,
        playground_config::exec_sandbox_profile?: value.exec_sandbox_profile,
        metadata::supersedes*: predecessors.iter(),
    };
    let id = fragment
        .root()
        .ok_or_else(|| anyhow!("config snapshot has no unique intrinsic root"))?;
    Ok((fragment, id))
}

/// Construct profile genesis and the activating config successor as one
/// self-contained fragment suitable for one signed COMMIT.
pub fn add_profile_fragment(
    profile: &ProfileValue,
    config: &ConfigValue,
    config_predecessors: &[Id],
) -> Result<(Fragment, Id, Id)> {
    if profile.anchor != config.active_profile {
        bail!(
            "new profile anchor {:x} does not match activating config anchor {:x}",
            profile.anchor,
            config.active_profile
        );
    }
    let mut fragment = profile_anchor_fragment(profile.anchor);
    let (profile_fragment, profile_id) = profile_snapshot_fragment(profile, &[])?;
    let (config_fragment, config_id) = config_snapshot_fragment(config, config_predecessors)?;
    fragment += profile_fragment;
    fragment += config_fragment;
    Ok((fragment, profile_id, config_id))
}

fn bool_count(value: bool) -> CountValue {
    if value { 1u64 } else { 0u64 }.to_inline()
}

fn sorted_ids(values: impl IntoIterator<Item = Id>) -> Vec<Id> {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort_unstable();
    values.dedup();
    values
}

fn required_trimmed(value: &str, field: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value || value.bytes().any(|byte| byte == 0) {
        bail!("Headspace {field} is empty, contains NUL, or has surrounding whitespace");
    }
    Ok(())
}

fn non_nul(value: &str, field: &str) -> Result<()> {
    if value.bytes().any(|byte| byte == 0) {
        bail!("Headspace {field} contains NUL");
    }
    Ok(())
}

fn validate_profile_value(value: &ProfileValue) -> Result<()> {
    required_trimmed(&value.name, "profile name")?;
    required_trimmed(&value.model, "model name")?;
    required_trimmed(&value.base_url, "model base URL")?;
    if let Some(value) = value.reasoning_effort.as_deref() {
        required_trimmed(value, "reasoning effort")?;
    }
    Ok(())
}

fn validate_config_value(value: &ConfigValue) -> Result<()> {
    non_nul(&value.system_prompt, "system prompt")?;
    required_trimmed(&value.author, "author")?;
    required_trimmed(&value.author_role, "author role")?;
    if let Some(value) = value.exec_default_cwd.as_deref() {
        non_nul(value, "execution cwd")?;
    }
    Ok(())
}

fn load_text_from(reader: &impl BlobStoreGet, handle: TextHandle) -> Result<String> {
    let view: View<str> = reader
        .get(handle)
        .with_context(|| format!("read Headspace text payload {}", hex::encode(handle.raw)))?;
    Ok(view.to_string())
}

fn format_ids(ids: impl IntoIterator<Item = Id>) -> String {
    ids.into_iter()
        .map(|id| format!("{id:x}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FactRead;

    use std::fs::File;
    use std::path::Path;

    use ed25519_dalek::SigningKey;
    use triblespace::core::blob::encodings::succinctarchive::{
        Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
    };
    use triblespace::core::repo::pile::Pile;

    use crate::collection_names::open_configured;
    use crate::schemas::headspace::{playground_config, DEFAULT_SCOPE_ID, KIND_LIVE_RECORD};
    use crate::secrets;
    use crate::storage::{open_secrets_collection, open_secrets_collection_read, FactArchive};
    fn test_id(byte: u8) -> Id {
        Id::new([byte; 16]).unwrap()
    }

    fn at(second: i64) -> Inline<inlineencodings::NsTAIInterval> {
        let epoch = hifitime::Epoch::from_unix_seconds(second as f64);
        (epoch, epoch).try_to_inline().unwrap()
    }

    fn test_pile(path: &Path, signer: &SigningKey) -> Pile {
        File::create(path).unwrap();
        let mut pile = Pile::open_as(path, signer.verifying_key()).unwrap();
        pile.refresh().unwrap();
        pile
    }

    fn materialize(pile: &mut Pile, scope: Id, signer: &SigningKey) -> (FactArchive, PileSnapshot) {
        let source = open_configured(pile, scope, signer.verifying_key()).unwrap();
        let succinct = pile.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
        let rank9 = pile
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
            .unwrap();
        let reader = pollster::block_on(async {
            drop(pile.ensure(source, signer).await.unwrap());
            drop(pile.maintain_attached(succinct, signer).await.unwrap());
            pile.maintain_attached(rank9, signer).await.unwrap()
        });
        let facts = reader.read_facts(rank9).unwrap();
        (facts, reader)
    }

    fn commit(pile: &mut Pile, scope: Id, signer: &SigningKey, fragment: Fragment) {
        let collection = open_configured(pile, scope, signer.verifying_key()).unwrap();
        pile.commit(collection, signer, fragment).unwrap();
    }

    #[test]
    fn snapshot_identity_is_retry_stable_and_predecessor_order_independent() {
        let value = default_profile(test_id(0x12), "stable");
        let a = test_id(0x13);
        let b = test_id(0x14);
        let (first, first_id) = profile_snapshot_fragment(&value, &[b, a, b]).unwrap();
        let (second, second_id) = profile_snapshot_fragment(&value, &[a, b]).unwrap();
        assert_eq!(first, second);
        assert_eq!(first_id, second_id);
    }

    #[test]
    fn fork_visible_resolution_keeps_divergent_and_agreed_heads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("headspace.pile");
        let signer = SigningKey::from_bytes(&[0x21; 32]);
        let mut pile = test_pile(&path, &signer);
        let anchor = test_id(0x11);
        let profile = default_profile(anchor, "default");
        let config = default_config(anchor);
        let (genesis, profile_head, _) = add_profile_fragment(&profile, &config, &[]).unwrap();
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, genesis);

        let mut left = profile.clone();
        left.model = "left".to_owned();
        let mut right = profile.clone();
        right.model = "right".to_owned();
        let (left_fragment, left_id) = profile_snapshot_fragment(&left, &[profile_head]).unwrap();
        let (right_fragment, right_id) =
            profile_snapshot_fragment(&right, &[profile_head]).unwrap();
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, left_fragment);
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, right_fragment);

        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        assert!(matches!(
            current_profile(&reader, &facts, anchor).unwrap(),
            Resolution::Forked(ref heads) if heads.len() == 2
        ));

        let (left_join, left_join_id) = profile_snapshot_fragment(&profile, &[left_id]).unwrap();
        let (right_join, right_join_id) = profile_snapshot_fragment(&profile, &[right_id]).unwrap();
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, left_join);
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, right_join);

        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        assert!(matches!(
            current_profile(&reader, &facts, anchor).unwrap(),
            Resolution::Agreed(ref heads)
                if heads.iter().map(|head| head.id).collect::<BTreeSet<_>>()
                    == BTreeSet::from([left_join_id, right_join_id])
        ));
        pile.close().unwrap();
    }

    #[test]
    fn reconcile_joins_every_head_and_is_a_no_op_on_one_equal_head() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("headspace.pile");
        let signer = SigningKey::from_bytes(&[0x27; 32]);
        let mut pile = test_pile(&path, &signer);
        let anchor = test_id(0x28);
        let profile = default_profile(anchor, "default");
        let config = default_config(anchor);
        let (genesis, profile_head, config_head) =
            add_profile_fragment(&profile, &config, &[]).unwrap();
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, genesis);

        // One head already carrying the chosen value: nothing to write.
        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        assert!(reconcile_fragment(&reader, &facts, profile_head)
            .unwrap()
            .is_none());
        assert!(reconcile_fragment(&reader, &facts, config_head)
            .unwrap()
            .is_none());
        assert!(reconcile_fragment(&reader, &facts, test_id(0x29)).is_err());

        let mut left = profile.clone();
        left.model = "left".to_owned();
        let mut right = profile.clone();
        right.model = "right".to_owned();
        let (left_fragment, left_id) = profile_snapshot_fragment(&left, &[profile_head]).unwrap();
        let (right_fragment, right_id) =
            profile_snapshot_fragment(&right, &[profile_head]).unwrap();
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, left_fragment);
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, right_fragment);

        // Choosing a historical snapshot re-expresses its value over both heads.
        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        assert_eq!(
            snapshot_ids(&facts),
            BTreeSet::from([profile_head, config_head, left_id, right_id])
        );
        let (join, join_id) = reconcile_fragment(&reader, &facts, profile_head)
            .unwrap()
            .unwrap();
        assert_eq!(
            join,
            profile_snapshot_fragment(&profile, &[left_id, right_id])
                .unwrap()
                .0
        );
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, join);
        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        assert!(matches!(
            current_profile(&reader, &facts, anchor).unwrap(),
            Resolution::Unique(ref head) if head.id == join_id && head.value == profile
        ));

        // Two heads with equal values still need one explicit join.
        let (other, other_id) = profile_snapshot_fragment(&profile, &[left_id]).unwrap();
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, other);
        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        assert!(matches!(
            current_profile(&reader, &facts, anchor).unwrap(),
            Resolution::Agreed(ref heads) if heads.len() == 2
        ));
        let (join, _) = reconcile_fragment(&reader, &facts, join_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            join,
            profile_snapshot_fragment(&profile, &[join_id, other_id])
                .unwrap()
                .0
        );
        pile.close().unwrap();
    }

    #[test]
    fn snapshots_lacking_a_field_are_skipped_and_repeated_fields_are_each_read() {
        let anchor = test_id(0x24);
        let (mut fragment, profile_id, config_id) = add_profile_fragment(
            &default_profile(anchor, "open-world"),
            &default_config(anchor),
            &[],
        )
        .unwrap();
        let missing = test_id(0x25);
        fragment += entity! { ExclusiveId::force_ref(&missing) @
            metadata::tag: &KIND_LIVE_RECORD,
            metadata::tag: &KIND_CONFIG_ID,
            playground_config::active_model_profile_id: &anchor,
        };
        let other: TextHandle = fragment.put("other".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&profile_id) @
            playground_config::model_name: other,
        };
        let facts = fragment.facts().clone();
        let reader = fragment.blobs_mut().snapshot().unwrap();

        assert!(matches!(
            current_config(&reader, &facts).unwrap(),
            Resolution::Unique(ref head) if head.id == config_id
        ));
        assert!(!snapshot_ids(&facts).contains(&missing));
        assert!(snapshot_ids(&facts).contains(&profile_id));
        // Several values for one field are each a reading of the one head.
        let Resolution::Forked(readings) = current_profile(&reader, &facts, anchor).unwrap() else {
            panic!("one head with two readings");
        };
        assert!(readings.iter().all(|reading| reading.id == profile_id));
        assert_eq!(
            readings
                .iter()
                .map(|reading| reading.value.model.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["other", DEFAULT_MODEL])
        );
        // An explicit choice of that head names no single value.
        let error = reconcile_fragment(&reader, &facts, profile_id).unwrap_err();
        assert!(format!("{error:#}").contains("2 readings"));
    }

    #[test]
    fn missing_and_ambiguous_config_are_visible_failures() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("headspace.pile");
        let signer = SigningKey::from_bytes(&[0x22; 32]);
        let mut pile = test_pile(&path, &signer);
        let anchor = test_id(0x23);
        let profile = default_profile(anchor, "only-profile");
        let mut profile_only = profile_anchor_fragment(anchor);
        profile_only += profile_snapshot_fragment(&profile, &[]).unwrap().0;
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, profile_only);

        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        let config = current_config(&reader, &facts).unwrap();
        assert!(matches!(config, Resolution::Missing));
        assert!(config.settled_value("Headspace config").unwrap().is_none());

        let first = default_config(anchor);
        let mut second = first.clone();
        second.author = "other".to_owned();
        commit(
            &mut pile,
            DEFAULT_SCOPE_ID,
            &signer,
            config_snapshot_fragment(&first, &[]).unwrap().0,
        );
        commit(
            &mut pile,
            DEFAULT_SCOPE_ID,
            &signer,
            config_snapshot_fragment(&second, &[]).unwrap().0,
        );
        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        let config = current_config(&reader, &facts).unwrap();
        assert!(matches!(config, Resolution::Forked(ref heads) if heads.len() == 2));
        assert!(format!(
            "{:#}",
            config.settled_value("Headspace config").unwrap_err()
        )
        .contains("forked"));
        pile.close().unwrap();
    }

    #[test]
    fn native_records_hold_exact_secret_ids_and_no_plaintext_payload() {
        let anchor = test_id(0x31);
        let secret = test_id(0x32);
        let mut profile = default_profile(anchor, "private");
        profile.model_secret_version = Some(secret);
        let mut config = default_config(anchor);
        config.tavily_secret_version = Some(secret);
        config.exa_secret_version = Some(secret);
        let (fragment, _, _) = add_profile_fragment(&profile, &config, &[]).unwrap();
        let secret_inline: Inline<inlineencodings::GenId> = secret.to_inline();

        assert!(!fragment.facts().iter().any(|fact| {
            [
                playground_config::model_api_key.id(),
                playground_config::tavily_api_key.id(),
                playground_config::exa_api_key.id(),
            ]
            .contains(fact.a())
        }));
        assert!(fragment
            .facts()
            .iter()
            .filter(|fact| {
                [
                    playground_config::model_secret_version.id(),
                    playground_config::tavily_secret_version.id(),
                    playground_config::exa_secret_version.id(),
                ]
                .contains(fact.a())
            })
            .all(|fact| fact.v::<inlineencodings::GenId>() == &secret_inline));
    }

    #[test]
    fn exact_secret_opening_ignores_a_newer_same_label_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("headspace.pile");
        let signer = SigningKey::from_bytes(&[0x41; 32]);
        let mut pile = test_pile(&path, &signer);
        let collection = open_secrets_collection(&mut pile, signer.verifying_key()).unwrap();
        let first_id = secrets::storage::add_secret(
            &mut pile,
            &signer,
            collection,
            "hs/model",
            b"first",
            at(3),
        )
        .unwrap();
        secrets::storage::add_secret(&mut pile, &signer, collection, "hs/model", b"second", at(4))
            .unwrap();
        let collection = open_secrets_collection_read(&mut pile, signer.verifying_key()).unwrap();
        let secrets = pollster::block_on(secrets::storage::ensure_and_snapshot(
            &mut pile, collection, &signer,
        ))
        .unwrap();

        let anchor = test_id(0x42);
        let mut profile = default_profile(anchor, "exact");
        profile.model_secret_version = Some(first_id);
        let config = default_config(anchor);
        commit(
            &mut pile,
            DEFAULT_SCOPE_ID,
            &signer,
            add_profile_fragment(&profile, &config, &[]).unwrap().0,
        );

        let (headspace_facts, headspace_reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        let config = current_config(&headspace_reader, &headspace_facts).unwrap();
        let profile = current_profile(&headspace_reader, &headspace_facts, anchor).unwrap();
        validate_current_secret_references(&config, [&profile], &secrets).unwrap();
        let Resolution::Unique(ref profile) = profile else {
            panic!("one profile head");
        };
        let secret = profile.value.model_secret_version.unwrap();
        assert_eq!(secret, first_id);
        assert_eq!(secrets.open(secret, &signer).unwrap(), b"first");
        pile.close().unwrap();
    }

    #[test]
    fn missing_exact_secret_reference_is_rejected() {
        let anchor = test_id(0x51);
        let mut profile = default_profile(anchor, "missing");
        profile.model_secret_version = Some(test_id(0x52));
        let (fragment, _, _) =
            add_profile_fragment(&profile, &default_config(anchor), &[]).unwrap();

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("headspace.pile");
        let signer = SigningKey::from_bytes(&[0x53; 32]);
        let mut pile = test_pile(&path, &signer);
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, fragment);
        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        let config = current_config(&reader, &facts).unwrap();
        let profile = current_profile(&reader, &facts, anchor).unwrap();
        let collection = open_secrets_collection_read(&mut pile, signer.verifying_key()).unwrap();
        let secrets = pollster::block_on(secrets::storage::ensure_and_snapshot(
            &mut pile, collection, &signer,
        ))
        .unwrap();
        let error = validate_current_secret_references(&config, [&profile], &secrets).unwrap_err();
        assert!(format!("{error:#}").contains("missing exact model"));
        pile.close().unwrap();
    }

    #[test]
    fn historical_plaintext_is_exactly_present_but_semantically_inert() {
        let legacy_subject = test_id(0x61);
        let legacy_id = ExclusiveId::force_ref(&legacy_subject);
        let mut legacy = Fragment::empty();
        let plaintext = legacy.put("do-not-activate".to_owned());
        legacy += entity! { legacy_id @
            metadata::tag: &KIND_MODEL_PROFILE_ID,
            playground_config::model_api_key: plaintext,
        };

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("headspace.pile");
        let signer = SigningKey::from_bytes(&[0x62; 32]);
        let mut pile = test_pile(&path, &signer);
        commit(&mut pile, DEFAULT_SCOPE_ID, &signer, legacy.clone());
        let (facts, reader) = materialize(&mut pile, DEFAULT_SCOPE_ID, &signer);
        let materialized = facts.iter().collect::<TribleSet>();
        assert!(legacy
            .facts()
            .iter()
            .all(|fact| materialized.contains(fact)));
        assert!(matches!(
            current_config(&reader, &facts).unwrap(),
            Resolution::Missing
        ));
        assert!(current_profiles(&reader, &facts).unwrap().is_empty());
        assert!(snapshot_ids(&facts).is_empty());
        pile.close().unwrap();
    }
}
