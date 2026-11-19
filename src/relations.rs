//! Collection-native Relations values and read semantics.
//!
//! Stable person/group anchors define the addressable native subjects. Typed
//! queries recognize the snapshot relations this reader understands directly;
//! entity ids remain opaque and unrelated open-world facts remain invisible.
//! Predecessor relations expose replica-union forks without a clock-selected
//! winner. Persisted normalized labels are deliberately absent: lookup is a
//! pure view over the current profile text and can later be accelerated by a
//! derived collection without changing truth.

pub mod cli;
pub mod mcp;
pub mod operations;
pub mod render;

pub use operations::{
    AddedGroup, AddedOrganisation, AddedPerson, AddedTie, EndedTie, GroupAddition,
    GroupReconciliation, GroupRemoval, GroupRename, IdentityChange, LifecycleChange, PeopleFilter,
    PersonState, ProfileField, ProfilePatch, ProfileReconciliation, ProfileUpdate, Relations,
    TieRequest,
};

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anybytes::View;
use anyhow::{anyhow, bail, Context, Result};
#[cfg(test)]
use triblespace::core::blob::MemoryBlobStoreSnapshot;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::BlobStoreGet;
use triblespace::macros::{entity, find, pattern};
use triblespace::prelude::*;

use crate::schemas::relations::{
    group, identity, lifecycle, profile, relations as legacy, tie, KIND_GROUP, KIND_GROUP_SNAPSHOT,
    KIND_IDENTITY_VERDICT, KIND_ORGANISATION, KIND_ORGANISATION_LIFECYCLE,
    KIND_ORGANISATION_PROFILE, KIND_PERSON_ID, KIND_PERSON_LIFECYCLE, KIND_PERSON_PROFILE,
    KIND_TIE,
};

pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
/// One queryable observation of when a stable person or group anchor was
/// created in a source system.
pub type ObservedAt = Inline<inlineencodings::NsTAIInterval>;

/// The addressable Relations entries that carry a profile and a lifecycle.
///
/// Persons and organisations share the snapshot machinery (the same
/// attributes, the same head resolution) under distinct kind tags, so a
/// person query never sees an organisation and the reverse. Groups are a
/// separate addressable kind with their own member-set snapshots.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EntryKind {
    Person,
    Organisation,
}

impl EntryKind {
    pub const ALL: [EntryKind; 2] = [EntryKind::Person, EntryKind::Organisation];

    /// Tag on the stable anchor.
    pub fn anchor_tag(self) -> Id {
        match self {
            Self::Person => KIND_PERSON_ID,
            Self::Organisation => KIND_ORGANISATION,
        }
    }

    /// Tag on the anchor's profile snapshots.
    pub fn profile_tag(self) -> Id {
        match self {
            Self::Person => KIND_PERSON_PROFILE,
            Self::Organisation => KIND_ORGANISATION_PROFILE,
        }
    }

    /// Tag on the anchor's lifecycle snapshots.
    pub fn lifecycle_tag(self) -> Id {
        match self {
            Self::Person => KIND_PERSON_LIFECYCLE,
            Self::Organisation => KIND_ORGANISATION_LIFECYCLE,
        }
    }

    pub fn noun(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Organisation => "organisation",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProfileInput {
    pub label: String,
    pub aliases: Vec<String>,
    pub affinities: Vec<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub display_name: Option<String>,
    pub note: Option<String>,
    pub teams_user_ids: Vec<String>,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub company: Option<String>,
    pub position: Option<String>,
    pub profile_urls: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileSnapshot {
    pub id: Id,
    pub person: Id,
    pub label: TextHandle,
    pub aliases: Vec<TextHandle>,
    pub affinities: Vec<TextHandle>,
    pub first_name: Option<TextHandle>,
    pub last_name: Option<TextHandle>,
    pub display_name: Option<TextHandle>,
    pub note: Option<TextHandle>,
    pub teams_user_ids: Vec<TextHandle>,
    pub emails: Vec<TextHandle>,
    pub phones: Vec<TextHandle>,
    pub company: Option<TextHandle>,
    pub position: Option<TextHandle>,
    pub profile_urls: Vec<TextHandle>,
    pub predecessors: Vec<Id>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleSnapshot {
    pub id: Id,
    pub person: Id,
    pub retired: bool,
    pub predecessors: Vec<Id>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupSnapshot {
    pub id: Id,
    pub group: Id,
    pub name: TextHandle,
    pub members: Vec<Id>,
    pub predecessors: Vec<Id>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityVerdict {
    pub id: Id,
    pub low: Id,
    pub high: Id,
    pub same: bool,
    pub predecessors: Vec<Id>,
}

/// Current state of one snapshot track. A fork is legitimate grow-only
/// evidence and is therefore a value, not catalog corruption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Head {
    Missing,
    Unique(Id),
    Forked(Vec<Id>),
}

/// Typed result of resolving a human-facing person or group selector.
///
/// `Forked` keeps the two kinds of candidate apart: `forked` are the anchors
/// whose own snapshot track is unsettled — the actual blockers — and `settled`
/// are the other matches, retained so a caller sees the whole candidate set.
/// Flattening the two into one list is what made a fork on an irrelevant
/// anchor read as a fork on the anchor the user asked for. Callers can inspect
/// the corresponding `*_head` values for detail without scraping an error
/// string.
///
/// Only candidates that survive disqualification appear here at all: a
/// selector never reports a fork on an anchor it has already ruled out (see
/// [`resolve_person`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectorOutcome {
    Missing,
    Unique(Id),
    Ambiguous(Vec<Id>),
    Forked { forked: Vec<Id>, settled: Vec<Id> },
    Invalid(String),
}

impl SelectorOutcome {
    /// Every anchor this outcome matched, blockers and settled alike.
    pub fn candidates(&self) -> Vec<Id> {
        match self {
            Self::Unique(id) => vec![*id],
            Self::Ambiguous(ids) => ids.clone(),
            Self::Forked { forked, settled } => {
                sorted_ids(forked.iter().copied().chain(settled.iter().copied()))
            }
            Self::Missing | Self::Invalid(_) => Vec::new(),
        }
    }

    /// Render the typed outcome at a command boundary that requires one exact
    /// anchor. Library consumers should normally retain the enum.
    pub fn require_unique(self, kind: &str, input: &str) -> Result<Id> {
        match self {
            Self::Unique(id) => Ok(id),
            Self::Missing => bail!("no {kind} matches '{input}'"),
            Self::Ambiguous(ids) => bail!(
                "multiple {kind}s match '{input}': {}",
                ids.iter()
                    .map(|id| format!("{id:x}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Forked { forked, settled } => {
                let mut message = format!(
                    "cannot resolve {kind} '{input}': unreconciled {kind} state on {}",
                    forked
                        .iter()
                        .map(|id| format!("{id:x}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if !settled.is_empty() {
                    message.push_str(&format!(
                        " (also matched, but not selectable while the fork stands: {})",
                        settled
                            .iter()
                            .map(|id| format!("{id:x}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                bail!("{message}")
            }
            Self::Invalid(reason) => bail!("invalid {kind} selector '{input}': {reason}"),
        }
    }
}

fn sorted_ids(values: impl IntoIterator<Item = Id>) -> Vec<Id> {
    let mut values: Vec<Id> = values.into_iter().collect();
    values.sort_unstable();
    values.dedup();
    values
}

fn sorted_handles(values: impl IntoIterator<Item = TextHandle>) -> Vec<TextHandle> {
    let mut values: Vec<TextHandle> = values.into_iter().collect();
    values.sort_unstable_by_key(|value| value.raw);
    values.dedup();
    values
}

fn sorted_strings(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values: Vec<String> = values.into_iter().collect();
    values.sort();
    values.dedup();
    values
}

fn required_text(value: impl Into<String>, field: &str) -> Result<String> {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{field} is empty");
    }
    if trimmed.bytes().any(|byte| byte == 0) {
        bail!("{field} contains a NUL byte");
    }
    Ok(trimmed.to_owned())
}

fn optional_text(value: Option<String>, field: &str) -> Result<Option<String>> {
    value.map(|value| required_text(value, field)).transpose()
}

fn text_set(values: Vec<String>, field: &str) -> Result<Vec<String>> {
    let mut values: Vec<String> = values
        .into_iter()
        .map(|value| required_text(value, field))
        .collect::<Result<_>>()?;
    values.sort();
    values.dedup();
    Ok(values)
}

fn source_set(mut values: Vec<String>) -> Result<Vec<String>> {
    if values
        .iter()
        .any(|value| value.bytes().any(|byte| byte == 0))
    {
        bail!("source contains a NUL byte");
    }
    values.sort();
    values.dedup();
    Ok(values)
}

fn sorted_observations(values: impl IntoIterator<Item = ObservedAt>) -> Vec<ObservedAt> {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort_unstable_by_key(|value| value.raw);
    values.dedup();
    values
}

fn provenance_record(anchor: Id, sources: &[String], observed_at: &[ObservedAt]) -> Fragment {
    entity! { ExclusiveId::force_ref(&anchor) @
        legacy::source*: sources.iter().map(String::as_str),
        metadata::created_at*: observed_at.iter(),
    }
}

/// Build additive provenance for a stable person anchor.
///
/// Source labels and creation observations are deliberately outside profile
/// snapshot identity: learning another immutable source fact neither forks nor
/// rewrites mutable person state.
pub fn person_provenance_fragment(
    person: Id,
    sources: Vec<String>,
    observed_at: &[ObservedAt],
) -> Result<Fragment> {
    let sources = source_set(sources)?;
    let observed_at = sorted_observations(observed_at.iter().copied());
    Ok(provenance_record(person, &sources, &observed_at))
}

/// Build additive creation observations for a stable group anchor.
pub fn group_provenance_fragment(group: Id, observed_at: &[ObservedAt]) -> Fragment {
    provenance_record(
        group,
        &[],
        &sorted_observations(observed_at.iter().copied()),
    )
}

fn anchor_fragment(anchor: Id, kind: Id) -> Fragment {
    entity! { ExclusiveId::force_ref(&anchor) @ metadata::tag: &kind }
}

fn profile_record_of(kind: EntryKind, snapshot: &ProfileSnapshot) -> Fragment {
    let tag = kind.profile_tag();
    entity! {
        metadata::tag: &tag,
        profile::of: &snapshot.person,
        metadata::name: snapshot.label,
        profile::alias*: snapshot.aliases.iter(),
        profile::affinity*: snapshot.affinities.iter(),
        profile::first_name?: snapshot.first_name,
        profile::last_name?: snapshot.last_name,
        profile::display_name?: snapshot.display_name,
        metadata::description?: snapshot.note,
        profile::teams_user_id*: snapshot.teams_user_ids.iter(),
        profile::email*: snapshot.emails.iter(),
        profile::phone*: snapshot.phones.iter(),
        profile::company?: snapshot.company,
        profile::position?: snapshot.position,
        profile::profile_url*: snapshot.profile_urls.iter(),
        metadata::supersedes*: snapshot.predecessors.iter(),
    }
}

fn lifecycle_record_of(kind: EntryKind, snapshot: &LifecycleSnapshot) -> Fragment {
    let tag = kind.lifecycle_tag();
    entity! {
        metadata::tag: &tag,
        lifecycle::of: &snapshot.person,
        lifecycle::retired: snapshot.retired,
        metadata::supersedes*: snapshot.predecessors.iter(),
    }
}

fn group_record(snapshot: &GroupSnapshot) -> Fragment {
    entity! {
        metadata::tag: &KIND_GROUP_SNAPSHOT,
        group::snapshot_of: &snapshot.group,
        metadata::name: snapshot.name,
        group::member*: snapshot.members.iter(),
        metadata::supersedes*: snapshot.predecessors.iter(),
    }
}

fn identity_record(snapshot: &IdentityVerdict) -> Fragment {
    entity! {
        metadata::tag: &KIND_IDENTITY_VERDICT,
        identity::low: &snapshot.low,
        identity::high: &snapshot.high,
        identity::same: snapshot.same,
        metadata::supersedes*: snapshot.predecessors.iter(),
    }
}

/// Build an initial person declaration, full profile, and active lifecycle as
/// one self-contained publication fragment.
pub fn person_fragment(person: Id, input: ProfileInput) -> Result<(Fragment, Id, Id)> {
    entry_fragment(EntryKind::Person, person, input)
}

/// Build an initial organisation declaration, full profile, and active
/// lifecycle as one self-contained publication fragment.
pub fn organisation_fragment(organisation: Id, input: ProfileInput) -> Result<(Fragment, Id, Id)> {
    entry_fragment(EntryKind::Organisation, organisation, input)
}

/// Build an initial anchor declaration, full profile, and active lifecycle
/// for one addressable entry of `kind`.
pub fn entry_fragment(
    kind: EntryKind,
    anchor: Id,
    input: ProfileInput,
) -> Result<(Fragment, Id, Id)> {
    let mut fragment = anchor_fragment(anchor, kind.anchor_tag());
    let profile = profile_fragment_of(kind, anchor, input, &[])?;
    let profile_id = profile
        .root()
        .expect("profile fragment has one intrinsic root");
    fragment += profile;
    let lifecycle = lifecycle_fragment_of(kind, anchor, false, &[]);
    let lifecycle_id = lifecycle
        .root()
        .expect("lifecycle fragment has one intrinsic root");
    fragment += lifecycle;
    Ok((fragment, profile_id, lifecycle_id))
}

/// Build one exact intrinsic profile successor.
pub fn profile_fragment(person: Id, input: ProfileInput, predecessors: &[Id]) -> Result<Fragment> {
    profile_fragment_of(EntryKind::Person, person, input, predecessors)
}

/// Build one exact intrinsic profile successor for an entry of `kind`.
pub fn profile_fragment_of(
    kind: EntryKind,
    person: Id,
    input: ProfileInput,
    predecessors: &[Id],
) -> Result<Fragment> {
    let label = required_text(input.label, "label")?;
    let aliases = text_set(input.aliases, "alias")?;
    let affinities = text_set(input.affinities, "affinity")?;
    let teams_user_ids = text_set(input.teams_user_ids, "Teams user id")?;
    let emails = text_set(input.emails, "email")?;
    let phones = text_set(input.phones, "phone")?;
    let profile_urls = text_set(input.profile_urls, "profile URL")?;
    let first_name = optional_text(input.first_name, "first name")?;
    let last_name = optional_text(input.last_name, "last name")?;
    let display_name = optional_text(input.display_name, "display name")?;
    let company = optional_text(input.company, "company")?;
    let position = optional_text(input.position, "position")?;

    let mut fragment = Fragment::empty();
    let snapshot = ProfileSnapshot {
        id: person,
        person,
        label: fragment.put(label),
        aliases: aliases
            .into_iter()
            .map(|value| fragment.put(value))
            .collect(),
        affinities: affinities
            .into_iter()
            .map(|value| fragment.put(value))
            .collect(),
        first_name: first_name.map(|value| fragment.put(value)),
        last_name: last_name.map(|value| fragment.put(value)),
        display_name: display_name.map(|value| fragment.put(value)),
        note: input.note.map(|value| fragment.put(value)),
        teams_user_ids: teams_user_ids
            .into_iter()
            .map(|value| fragment.put(value))
            .collect(),
        emails: emails
            .into_iter()
            .map(|value| fragment.put(value))
            .collect(),
        phones: phones
            .into_iter()
            .map(|value| fragment.put(value))
            .collect(),
        company: company.map(|value| fragment.put(value)),
        position: position.map(|value| fragment.put(value)),
        profile_urls: profile_urls
            .into_iter()
            .map(|value| fragment.put(value))
            .collect(),
        predecessors: sorted_ids(predecessors.iter().copied()),
    };
    fragment += profile_record_of(kind, &snapshot);
    Ok(fragment)
}

pub fn lifecycle_fragment(person: Id, retired: bool, predecessors: &[Id]) -> Fragment {
    lifecycle_fragment_of(EntryKind::Person, person, retired, predecessors)
}

pub fn lifecycle_fragment_of(
    kind: EntryKind,
    person: Id,
    retired: bool,
    predecessors: &[Id],
) -> Fragment {
    lifecycle_record_of(
        kind,
        &LifecycleSnapshot {
            id: person,
            person,
            retired,
            predecessors: sorted_ids(predecessors.iter().copied()),
        },
    )
}

pub fn group_create_fragment(group_id: Id, name: impl Into<String>) -> Result<(Fragment, Id)> {
    let mut fragment = anchor_fragment(group_id, KIND_GROUP);
    let snapshot = group_snapshot_fragment(group_id, name, &[], &[])?;
    let snapshot_id = snapshot
        .root()
        .expect("group snapshot fragment has one intrinsic root");
    fragment += snapshot;
    Ok((fragment, snapshot_id))
}

pub fn group_snapshot_fragment(
    group_id: Id,
    name: impl Into<String>,
    members: &[Id],
    predecessors: &[Id],
) -> Result<Fragment> {
    let name = required_text(name, "group name")?;
    let mut fragment = Fragment::empty();
    let snapshot = GroupSnapshot {
        id: group_id,
        group: group_id,
        name: fragment.put(name),
        members: sorted_ids(members.iter().copied()),
        predecessors: sorted_ids(predecessors.iter().copied()),
    };
    fragment += group_record(&snapshot);
    Ok(fragment)
}

/// Reconcile concurrent group snapshots without losing any asserted member.
///
/// A normal single-parent successor is an authored full-state edit and may
/// add or remove members. A multi-parent successor has different semantics:
/// it closes a fork, so membership is the join (set union) of the snapshots it
/// names. The group name remains an explicit human choice because names do not
/// themselves form a useful join-semilattice.
pub fn reconcile_group_fragment<P>(
    facts: &P,
    group_id: Id,
    name: impl Into<String>,
    predecessors: &[Id],
) -> Result<Fragment>
where
    P: TriblePattern,
{
    let predecessors = sorted_ids(predecessors.iter().copied());
    if predecessors.len() < 2 {
        bail!("group reconciliation requires at least two distinct predecessors");
    }

    let mut members = BTreeSet::new();
    for predecessor in &predecessors {
        let snapshot = group_snapshot(facts, *predecessor)
            .with_context(|| format!("read group reconciliation predecessor {predecessor:x}"))?;
        if snapshot.group != group_id {
            bail!(
                "group reconciliation predecessor {predecessor:x} belongs to group {:x}, not {group_id:x}",
                snapshot.group
            );
        }
        members.extend(snapshot.members);
    }
    let members: Vec<_> = members.into_iter().collect();
    group_snapshot_fragment(group_id, name, &members, &predecessors)
}

pub fn identity_verdict_fragment(
    first: Id,
    second: Id,
    same: bool,
    predecessors: &[Id],
) -> Result<Fragment> {
    if first == second {
        bail!("identity verdict endpoints must be different people");
    }
    let (low, high) = if first < second {
        (first, second)
    } else {
        (second, first)
    };
    Ok(identity_record(&IdentityVerdict {
        id: low,
        low,
        high,
        same,
        predecessors: sorted_ids(predecessors.iter().copied()),
    }))
}

/// Read one decodable profile projection.
///
/// The entity id is an opaque anchor. Extra open-world facts do not invalidate
/// the projection, and values which do not decode to the requested schema are
/// skipped by the typed query. Published snapshots have one scalar projection;
/// if foreign additive facts provide several, the byte-smallest typed value is
/// used deterministically instead of imposing a collection-wide cardinality
/// constraint.
pub fn profile_snapshot<P>(facts: &P, id: Id) -> Result<ProfileSnapshot>
where
    P: TriblePattern,
{
    profile_snapshot_of(facts, id, EntryKind::Person)
}

/// Read one decodable profile projection of an entry of `kind`.
pub fn profile_snapshot_of<P>(facts: &P, id: Id, kind: EntryKind) -> Result<ProfileSnapshot>
where
    P: TriblePattern,
{
    let tag = kind.profile_tag();
    let (person, label) = find!(
        (person: Id, label: TextHandle),
        pattern!(facts, [{ id @
            metadata::tag: tag,
            profile::of: ?person,
            metadata::name: ?label,
        }])
    )
    .min_by(|(left_person, left_label), (right_person, right_label)| {
        left_person
            .cmp(right_person)
            .then_with(|| left_label.raw.cmp(&right_label.raw))
    })
    .ok_or_else(|| anyhow!("Relations entity {id:x} has no decodable profile projection"))?;
    Ok(ProfileSnapshot {
        id,
        person,
        label,
        aliases: sorted_handles(
            find!(v: TextHandle, pattern!(facts, [{ id @ profile::alias: ?v }])),
        ),
        affinities: sorted_handles(find!(
            v: TextHandle,
            pattern!(facts, [{ id @ profile::affinity: ?v }])
        )),
        first_name: find!(v: TextHandle, pattern!(facts, [{ id @ profile::first_name: ?v }]))
            .min_by_key(|value| value.raw),
        last_name: find!(v: TextHandle, pattern!(facts, [{ id @ profile::last_name: ?v }]))
            .min_by_key(|value| value.raw),
        display_name: find!(v: TextHandle, pattern!(facts, [{ id @ profile::display_name: ?v }]))
            .min_by_key(|value| value.raw),
        note: find!(v: TextHandle, pattern!(facts, [{ id @ metadata::description: ?v }]))
            .min_by_key(|value| value.raw),
        teams_user_ids: sorted_handles(find!(
            v: TextHandle,
            pattern!(facts, [{ id @ profile::teams_user_id: ?v }])
        )),
        emails: sorted_handles(find!(
            v: TextHandle,
            pattern!(facts, [{ id @ profile::email: ?v }])
        )),
        phones: sorted_handles(find!(
            v: TextHandle,
            pattern!(facts, [{ id @ profile::phone: ?v }])
        )),
        company: find!(v: TextHandle, pattern!(facts, [{ id @ profile::company: ?v }]))
            .min_by_key(|value| value.raw),
        position: find!(v: TextHandle, pattern!(facts, [{ id @ profile::position: ?v }]))
            .min_by_key(|value| value.raw),
        profile_urls: sorted_handles(find!(
            v: TextHandle,
            pattern!(facts, [{ id @ profile::profile_url: ?v }])
        )),
        predecessors: sorted_ids(find!(
            v: Id,
            pattern!(facts, [{ id @ metadata::supersedes: ?v }])
        )),
    })
}

pub fn lifecycle_snapshot<P>(facts: &P, id: Id) -> Result<LifecycleSnapshot>
where
    P: TriblePattern,
{
    lifecycle_snapshot_of(facts, id, EntryKind::Person)
}

/// Read one decodable lifecycle projection of an entry of `kind`.
pub fn lifecycle_snapshot_of<P>(facts: &P, id: Id, kind: EntryKind) -> Result<LifecycleSnapshot>
where
    P: TriblePattern,
{
    let tag = kind.lifecycle_tag();
    let (person, retired) = find!(
        (person: Id, retired: bool),
        pattern!(facts, [{ id @
            metadata::tag: tag,
            lifecycle::of: ?person,
            lifecycle::retired: ?retired,
        }])
    )
    .min_by(|left, right| left.cmp(right))
    .ok_or_else(|| anyhow!("Relations entity {id:x} has no decodable lifecycle projection"))?;
    Ok(LifecycleSnapshot {
        id,
        person,
        retired,
        predecessors: sorted_ids(find!(
            v: Id,
            pattern!(facts, [{ id @ metadata::supersedes: ?v }])
        )),
    })
}

pub fn group_snapshot<P>(facts: &P, id: Id) -> Result<GroupSnapshot>
where
    P: TriblePattern,
{
    let (group, name) = find!(
        (group: Id, name: TextHandle),
        pattern!(facts, [{ id @
            metadata::tag: &KIND_GROUP_SNAPSHOT,
            group::snapshot_of: ?group,
            metadata::name: ?name,
        }])
    )
    .min_by(|(left_group, left_name), (right_group, right_name)| {
        left_group
            .cmp(right_group)
            .then_with(|| left_name.raw.cmp(&right_name.raw))
    })
    .ok_or_else(|| anyhow!("Relations entity {id:x} has no decodable group projection"))?;
    Ok(GroupSnapshot {
        id,
        group,
        name,
        members: sorted_ids(find!(v: Id, pattern!(facts, [{ id @ group::member: ?v }]))),
        predecessors: sorted_ids(find!(
            v: Id,
            pattern!(facts, [{ id @ metadata::supersedes: ?v }])
        )),
    })
}

pub fn identity_verdict<P>(facts: &P, id: Id) -> Result<IdentityVerdict>
where
    P: TriblePattern,
{
    let (low, high, same) = find!(
        (low: Id, high: Id, same: bool),
        pattern!(facts, [{ id @
            metadata::tag: &KIND_IDENTITY_VERDICT,
            identity::low: ?low,
            identity::high: ?high,
            identity::same: ?same,
        }])
    )
    .min_by(|left, right| left.cmp(right))
    .ok_or_else(|| anyhow!("Relations entity {id:x} has no decodable identity projection"))?;
    Ok(IdentityVerdict {
        id,
        low,
        high,
        same,
        predecessors: sorted_ids(find!(
            v: Id,
            pattern!(facts, [{ id @ metadata::supersedes: ?v }])
        )),
    })
}

pub fn person_anchors<P>(facts: &P) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: &KIND_PERSON_ID }])).collect()
}

/// Every stable organisation anchor.
pub fn organisation_anchors<P>(facts: &P) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    ids_of_kind(facts, KIND_ORGANISATION)
}

/// Every stable anchor of `kind`.
pub fn entry_anchors<P>(facts: &P, kind: EntryKind) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    ids_of_kind(facts, kind.anchor_tag())
}

/// The profile-carrying kinds an anchor is tagged with, in [`EntryKind`]
/// order. Usually one; an open-world anchor could carry both tags.
pub fn entry_kinds<P>(facts: &P, anchor: Id) -> Vec<EntryKind>
where
    P: TriblePattern,
{
    let tags: BTreeSet<Id> =
        find!(tag: Id, pattern!(facts, [{ anchor @ metadata::tag: ?tag }])).collect();
    EntryKind::ALL
        .into_iter()
        .filter(|kind| tags.contains(&kind.anchor_tag()))
        .collect()
}

pub fn group_anchors<P>(facts: &P) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: &KIND_GROUP }])).collect()
}

/// Every exact source label asserted for a stable person anchor, sorted and
/// deduplicated. No scalar winner is selected when several systems observed
/// the same person.
pub fn person_sources<Store, P>(reader: &Store, facts: &P, person: Id) -> Result<Vec<String>>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    sources_of(reader, facts, person)
}

/// Every exact `relations::source` text annotating `entity` (a person or
/// organisation anchor, or a tie), sorted and deduplicated.
pub fn sources_of<Store, P>(reader: &Store, facts: &P, person: Id) -> Result<Vec<String>>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    Ok(sorted_strings(
        find!(
            value: TextHandle,
            pattern!(facts, [{ person @ legacy::source: ?value }])
        )
        .map(|value| read_text(reader, value))
        .collect::<Result<Vec<_>>>()?,
    ))
}

/// Every creation-time observation attached to a stable person or group
/// anchor, in chronological order. Repeated observations collapse by set
/// semantics.
pub fn creation_observations<P>(facts: &P, anchor: Id) -> Vec<ObservedAt>
where
    P: TriblePattern,
{
    sorted_observations(find!(
        value: ObservedAt,
        pattern!(facts, [{ anchor @ metadata::created_at: ?value }])
    ))
}

/// Deterministic lower bound of the known creation observations.
///
/// This is a projection, not a stored scalar fact: later union may reveal an
/// earlier observation without invalidating any existing assertion.
pub fn earliest_creation_observation<P>(facts: &P, anchor: Id) -> Option<ObservedAt>
where
    P: TriblePattern,
{
    creation_observations(facts, anchor).into_iter().next()
}

fn ids_of_kind<P>(facts: &P, kind: Id) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: kind }])).collect()
}

fn head_from(ids: BTreeSet<Id>, superseded: BTreeSet<Id>) -> Head {
    let heads: Vec<Id> = ids.difference(&superseded).copied().collect();
    match heads.as_slice() {
        [] => Head::Missing,
        [head] => Head::Unique(*head),
        _ => Head::Forked(heads),
    }
}

pub fn profile_head<P>(facts: &P, person: Id) -> Result<Head>
where
    P: TriblePattern,
{
    profile_head_of(facts, person, EntryKind::Person)
}

pub fn profile_head_of<P>(facts: &P, person: Id, kind: EntryKind) -> Result<Head>
where
    P: TriblePattern,
{
    let tag = kind.profile_tag();
    let ids: BTreeSet<Id> = find!(
        id: Id,
        pattern!(facts, [{ ?id @
            metadata::tag: tag,
            profile::of: person,
            metadata::name: _?label,
        }])
    )
    .collect();
    let superseded = find!(
        predecessor: Id,
        pattern!(facts, [{ _?id @
            metadata::tag: tag,
            profile::of: person,
            metadata::name: _?label,
            metadata::supersedes: ?predecessor,
        }])
    )
    .collect();
    Ok(head_from(ids, superseded))
}

pub fn lifecycle_head<P>(facts: &P, person: Id) -> Result<Head>
where
    P: TriblePattern,
{
    lifecycle_head_of(facts, person, EntryKind::Person)
}

pub fn lifecycle_head_of<P>(facts: &P, person: Id, kind: EntryKind) -> Result<Head>
where
    P: TriblePattern,
{
    let tag = kind.lifecycle_tag();
    let ids: BTreeSet<Id> = find!(
        id: Id,
        pattern!(facts, [{ ?id @
            metadata::tag: tag,
            lifecycle::of: person,
            lifecycle::retired: _?retired,
        }])
    )
    .collect();
    let superseded = find!(
        predecessor: Id,
        pattern!(facts, [{ _?id @
            metadata::tag: tag,
            lifecycle::of: person,
            lifecycle::retired: _?retired,
            metadata::supersedes: ?predecessor,
        }])
    )
    .collect();
    Ok(head_from(ids, superseded))
}

pub fn group_head<P>(facts: &P, group_id: Id) -> Result<Head>
where
    P: TriblePattern,
{
    let ids: BTreeSet<Id> = find!(
        id: Id,
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_GROUP_SNAPSHOT,
            group::snapshot_of: group_id,
            metadata::name: _?name,
        }])
    )
    .collect();
    let superseded = find!(
        predecessor: Id,
        pattern!(facts, [{ _?id @
            metadata::tag: &KIND_GROUP_SNAPSHOT,
            group::snapshot_of: group_id,
            metadata::name: _?name,
            metadata::supersedes: ?predecessor,
        }])
    )
    .collect();
    Ok(head_from(ids, superseded))
}

fn pair(first: Id, second: Id) -> Result<(Id, Id)> {
    if first == second {
        bail!("identity pair endpoints are equal");
    }
    Ok(if first < second {
        (first, second)
    } else {
        (second, first)
    })
}

pub fn identity_head<P>(facts: &P, first: Id, second: Id) -> Result<Head>
where
    P: TriblePattern,
{
    let (low, high) = pair(first, second)?;
    let ids: BTreeSet<Id> = find!(
        id: Id,
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_IDENTITY_VERDICT,
            identity::low: low,
            identity::high: high,
            identity::same: _?same,
        }])
    )
    .collect();
    let superseded = find!(
        predecessor: Id,
        pattern!(facts, [{ _?id @
            metadata::tag: &KIND_IDENTITY_VERDICT,
            identity::low: low,
            identity::high: high,
            identity::same: _?same,
            metadata::supersedes: ?predecessor,
        }])
    )
    .collect();
    Ok(head_from(ids, superseded))
}

fn load_text_from<Store>(reader: &Store, handle: TextHandle) -> Result<String>
where
    Store: BlobStoreGet + ?Sized,
{
    let view: View<str> = reader
        .get(handle)
        .with_context(|| format!("read Relations text payload {}", hex::encode(handle.raw)))?;
    Ok(view.to_string())
}

pub fn read_text<Store>(reader: &Store, handle: TextHandle) -> Result<String>
where
    Store: BlobStoreGet + ?Sized,
{
    load_text_from(reader, handle)
}

pub fn profile_input<Store>(reader: &Store, snapshot: &ProfileSnapshot) -> Result<ProfileInput>
where
    Store: BlobStoreGet + ?Sized,
{
    let read_all = |handles: &[TextHandle]| -> Result<Vec<String>> {
        handles
            .iter()
            .map(|&handle| load_text_from(reader, handle))
            .collect()
    };
    Ok(ProfileInput {
        label: load_text_from(reader, snapshot.label)?,
        aliases: read_all(&snapshot.aliases)?,
        affinities: read_all(&snapshot.affinities)?,
        first_name: snapshot
            .first_name
            .map(|handle| load_text_from(reader, handle))
            .transpose()?,
        last_name: snapshot
            .last_name
            .map(|handle| load_text_from(reader, handle))
            .transpose()?,
        display_name: snapshot
            .display_name
            .map(|handle| load_text_from(reader, handle))
            .transpose()?,
        note: snapshot
            .note
            .map(|handle| load_text_from(reader, handle))
            .transpose()?,
        teams_user_ids: read_all(&snapshot.teams_user_ids)?,
        emails: read_all(&snapshot.emails)?,
        phones: read_all(&snapshot.phones)?,
        company: snapshot
            .company
            .map(|handle| load_text_from(reader, handle))
            .transpose()?,
        position: snapshot
            .position
            .map(|handle| load_text_from(reader, handle))
            .transpose()?,
        profile_urls: read_all(&snapshot.profile_urls)?,
    })
}

pub fn current_profile<P>(facts: &P, person: Id) -> Result<ProfileSnapshot>
where
    P: TriblePattern,
{
    current_profile_of(facts, person, EntryKind::Person)
}

pub fn current_profile_of<P>(facts: &P, person: Id, kind: EntryKind) -> Result<ProfileSnapshot>
where
    P: TriblePattern,
{
    let noun = kind.noun();
    match profile_head_of(facts, person, kind)? {
        Head::Unique(id) => profile_snapshot_of(facts, id, kind),
        Head::Missing => bail!("{noun} {person:x} has no profile"),
        Head::Forked(heads) => bail!(
            "{noun} {person:x} profile is forked across {} heads: {}",
            heads.len(),
            heads
                .iter()
                .map(|id| format!("{id:x}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn person_is_retired<P>(facts: &P, person: Id) -> Result<bool>
where
    P: TriblePattern,
{
    match lifecycle_head(facts, person)? {
        Head::Unique(id) => Ok(lifecycle_snapshot(facts, id)?.retired),
        Head::Missing => bail!("person {person:x} has no lifecycle"),
        Head::Forked(heads) => bail!(
            "person {person:x} lifecycle is forked across {} heads",
            heads.len()
        ),
    }
}

/// The affinity that marks a colony zooid in Relations.
pub const ZOOID_AFFINITY: &str = "zooid";

/// Whether any current profile head of `person` carries [`ZOOID_AFFINITY`].
/// A fork is grow-only evidence: one head carrying it is enough. An
/// affinity text that is not here does not match.
pub fn carries_zooid_affinity<Store, P>(reader: &Store, facts: &P, person: Id) -> Result<bool>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    let heads = match profile_head(facts, person)? {
        Head::Unique(head) => vec![head],
        Head::Forked(heads) => heads,
        Head::Missing => Vec::new(),
    };
    for head in heads {
        for affinity in profile_snapshot(facts, head)?.affinities {
            if read_text(reader, affinity)
                .is_ok_and(|text| text.trim().eq_ignore_ascii_case(ZOOID_AFFINITY))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Every person carrying [`ZOOID_AFFINITY`] who is not recorded as retired.
/// A missing or forked lifecycle is not evidence of retirement, so such a
/// person is kept.
pub fn live_zooids<Store, P>(reader: &Store, facts: &P) -> Result<Vec<Id>>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    let mut zooids = Vec::new();
    for person in person_anchors(facts) {
        if person_is_retired(facts, person).unwrap_or(false) {
            continue;
        }
        if carries_zooid_affinity(reader, facts, person)? {
            zooids.push(person);
        }
    }
    Ok(zooids)
}

pub fn current_group<P>(facts: &P, group_id: Id) -> Result<GroupSnapshot>
where
    P: TriblePattern,
{
    match group_head(facts, group_id)? {
        Head::Unique(id) => group_snapshot(facts, id),
        Head::Missing => bail!("group {group_id:x} has no snapshot"),
        Head::Forked(heads) => bail!(
            "group {group_id:x} is forked across {} heads: {}",
            heads.len(),
            heads
                .iter()
                .map(|id| format!("{id:x}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn lookup_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn profile_matches<Store>(reader: &Store, snapshot: &ProfileSnapshot, key: &str) -> Result<bool>
where
    Store: BlobStoreGet + ?Sized,
{
    if lookup_key(&load_text_from(reader, snapshot.label)?) == key {
        return Ok(true);
    }
    for &alias in &snapshot.aliases {
        if lookup_key(&load_text_from(reader, alias)?) == key {
            return Ok(true);
        }
    }
    Ok(false)
}

fn id_candidates(input: &str, anchors: &BTreeSet<Id>) -> (bool, BTreeSet<Id>) {
    if !input.is_empty() && input.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        if input.len() == 32 {
            let exact = Id::from_hex(input)
                .filter(|id| anchors.contains(id))
                .into_iter()
                .collect();
            return (true, exact);
        }
        if input.len() < 32 {
            return (
                false,
                anchors
                    .iter()
                    .copied()
                    .filter(|id| format!("{id:x}").starts_with(input))
                    .collect(),
            );
        }
    }
    (false, BTreeSet::new())
}

fn selector_outcome(
    settled: BTreeSet<Id>,
    forked: BTreeSet<Id>,
    retired: BTreeSet<Id>,
) -> SelectorOutcome {
    if !forked.is_empty() {
        return SelectorOutcome::Forked {
            forked: forked.into_iter().collect(),
            settled: settled.into_iter().collect(),
        };
    }
    match settled.into_iter().collect::<Vec<_>>().as_slice() {
        [] if !retired.is_empty() => SelectorOutcome::Invalid(format!(
            "the only match is retired and therefore not addressable: {} (`relations unretire` \
             to restore it)",
            retired
                .iter()
                .map(|id| format!("{id:x}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        [] => SelectorOutcome::Missing,
        [id] => SelectorOutcome::Unique(*id),
        ids => SelectorOutcome::Ambiguous(ids.to_vec()),
    }
}

/// Whether a lifecycle track disqualifies its anchor from selection.
///
/// A fork is only indeterminate when its heads disagree. When every head says
/// the same thing, the answer is settled no matter which head eventually wins,
/// so the fork cannot change the verdict — the same reasoning
/// [`IdentityComponents::from_facts`] already applies to agreement-only
/// verdict forks. `None` means genuinely indeterminate.
fn retired_verdict<P>(facts: &P, person: Id, state: &Head, kind: EntryKind) -> Result<Option<bool>>
where
    P: TriblePattern,
{
    match state {
        Head::Unique(id) => Ok(Some(lifecycle_snapshot_of(facts, *id, kind)?.retired)),
        Head::Forked(heads) => {
            let values: BTreeSet<bool> = heads
                .iter()
                .map(|id| Ok(lifecycle_snapshot_of(facts, *id, kind)?.retired))
                .collect::<Result<_>>()?;
            Ok(match values.len() {
                1 => values.into_iter().next(),
                _ => None,
            })
        }
        Head::Missing => bail!("{} {person:x} has no lifecycle snapshot", kind.noun()),
    }
}

/// Resolve a person selector against current profile and lifecycle state.
///
/// Two rules keep an unreconciled anchor from vetoing an unrelated one:
///
/// 1. **Disqualification precedes fork reporting.** Unless `include_retired`,
///    a retired anchor is dropped outright and never contributes a fork —
///    whether its own state is settled is simply not a question the selector
///    asks about a candidate it has already ruled out.
/// 2. **A fork blocks only when it could change the answer.** The only thing
///    lifecycle decides here is addressability, so a lifecycle fork whose
///    heads all agree is settled for this purpose regardless of which head
///    eventually wins. Heads that disagree are indeterminate and do block.
///
/// A profile fork always blocks a surviving candidate: the label match itself
/// is then head-dependent.
pub fn resolve_person<Store, P>(
    reader: &Store,
    facts: &P,
    input: &str,
    include_retired: bool,
) -> Result<SelectorOutcome>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    resolve_entry(reader, facts, input, include_retired, &[EntryKind::Person])
}

/// Resolve an organisation selector (label, alias, exact id, or id prefix)
/// under the same rules as [`resolve_person`].
pub fn resolve_organisation<Store, P>(
    reader: &Store,
    facts: &P,
    input: &str,
    include_retired: bool,
) -> Result<SelectorOutcome>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    resolve_entry(
        reader,
        facts,
        input,
        include_retired,
        &[EntryKind::Organisation],
    )
}

/// Resolve one selector across the profile-carrying entry `kinds` at once,
/// under the rules of [`resolve_person`]. A label shared by a person and an
/// organisation is ambiguous, never silently one of them.
pub fn resolve_entry<Store, P>(
    reader: &Store,
    facts: &P,
    input: &str,
    include_retired: bool,
    kinds: &[EntryKind],
) -> Result<SelectorOutcome>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    let input = input.trim();
    if input.is_empty() {
        return Ok(SelectorOutcome::Invalid("empty selector".to_owned()));
    }
    let anchors: Vec<(Id, EntryKind)> = kinds
        .iter()
        .flat_map(|&kind| {
            entry_anchors(facts, kind)
                .into_iter()
                .map(move |id| (id, kind))
        })
        .collect();
    let all: BTreeSet<Id> = anchors.iter().map(|(id, _)| *id).collect();
    let normalized = input.to_ascii_lowercase();
    let (exact_id, id_matches) = id_candidates(&normalized, &all);
    if exact_id && id_matches.is_empty() {
        return Ok(SelectorOutcome::Missing);
    }

    let key = lookup_key(input);
    let mut settled = BTreeSet::new();
    let mut forked = BTreeSet::new();
    let mut retired = BTreeSet::new();
    for (person, kind) in anchors {
        if exact_id && !id_matches.contains(&person) {
            continue;
        }
        let noun = kind.noun();
        let profile_state = profile_head_of(facts, person, kind)?;
        if matches!(profile_state, Head::Missing) {
            bail!("{noun} {person:x} has no profile snapshot");
        }
        let label_matches = id_matches.contains(&person)
            || match &profile_state {
                Head::Unique(id) => {
                    profile_matches(reader, &profile_snapshot_of(facts, *id, kind)?, &key)?
                }
                Head::Forked(heads) => {
                    let mut matches = false;
                    for head in heads {
                        let snapshot = profile_snapshot_of(facts, *head, kind)?;
                        if profile_matches(reader, &snapshot, &key)? {
                            matches = true;
                            break;
                        }
                    }
                    matches
                }
                Head::Missing => bail!("{noun} {person:x} has no profile snapshot"),
            };
        if !id_matches.contains(&person) && (exact_id || !label_matches) {
            continue;
        }

        // Disqualification comes BEFORE fork reporting. A retired anchor is
        // not a candidate, so its internal consistency cannot bear on whether
        // the selector resolves — otherwise a fork on a retired legacy anchor
        // vetoes a live, perfectly settled match that merely shares its name.
        // A retired person carrying a multi-head profile fork must not poison a
        // live group that happens to share its selector.
        let lifecycle_state = lifecycle_head_of(facts, person, kind)?;
        let retired_state = retired_verdict(facts, person, &lifecycle_state, kind)?;
        if !include_retired && retired_state == Some(true) {
            retired.insert(person);
            continue;
        }

        // Past this point the anchor really is in the running, so an unsettled
        // track on it genuinely blocks: fail closed rather than pick a head.
        if matches!(profile_state, Head::Forked(_)) || retired_state.is_none() {
            forked.insert(person);
            continue;
        }
        settled.insert(person);
    }
    Ok(selector_outcome(settled, forked, retired))
}

pub fn resolve_group<Store, P>(reader: &Store, facts: &P, input: &str) -> Result<SelectorOutcome>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    let input = input.trim();
    if input.is_empty() {
        return Ok(SelectorOutcome::Invalid("empty selector".to_owned()));
    }
    let anchors = group_anchors(facts);
    let normalized = input.to_ascii_lowercase();
    let (exact_id, id_matches) = id_candidates(&normalized, &anchors);
    if exact_id && id_matches.is_empty() {
        return Ok(SelectorOutcome::Missing);
    }
    let key = lookup_key(input);
    let mut settled = BTreeSet::new();
    let mut forked = BTreeSet::new();
    for group_id in anchors {
        if exact_id && !id_matches.contains(&group_id) {
            continue;
        }
        let state = group_head(facts, group_id)?;
        let label_matches = id_matches.contains(&group_id)
            || match &state {
                Head::Unique(id) => {
                    let snapshot = group_snapshot(facts, *id)?;
                    lookup_key(&load_text_from(reader, snapshot.name)?) == key
                }
                Head::Forked(heads) => {
                    let mut matches = false;
                    for head in heads {
                        let snapshot = group_snapshot(facts, *head)?;
                        if lookup_key(&load_text_from(reader, snapshot.name)?) == key {
                            matches = true;
                            break;
                        }
                    }
                    matches
                }
                Head::Missing => bail!("group {group_id:x} has no snapshot"),
            };
        if !id_matches.contains(&group_id) && (exact_id || !label_matches) {
            continue;
        }
        match state {
            Head::Unique(_) => {
                settled.insert(group_id);
            }
            Head::Forked(_) => {
                forked.insert(group_id);
            }
            Head::Missing => bail!("group {group_id:x} has no snapshot"),
        }
    }
    Ok(selector_outcome(settled, forked, BTreeSet::new()))
}

/// The person and group anchors a Compass `@label` addresses: every
/// addressable person whose current profile label or alias, and every group
/// whose current name, equals `label` under [`lookup_key`].
///
/// These are the same textual keys Orient's attention has always matched,
/// so an id or id prefix is never a mention. Every head of a forked profile or
/// group is consulted, as Orient does, so a concurrent edit cannot silence a
/// mention; a person whose lifecycle heads all say retired is not
/// addressable. An anchor whose snapshot track cannot be read is skipped
/// rather than failing the write that mentions something else; payload reads
/// still propagate, so an acquiring reader can fetch them.
pub fn mention_targets<Store, P>(reader: &Store, facts: &P, label: &str) -> Result<BTreeSet<Id>>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    let key = lookup_key(label);
    let mut targets = BTreeSet::new();
    if key.is_empty() {
        return Ok(targets);
    }
    let heads_of = |head: Head| match head {
        Head::Unique(id) => vec![id],
        Head::Forked(ids) => ids,
        Head::Missing => Vec::new(),
    };
    for person in person_anchors(facts) {
        let Ok(profiles) = profile_head(facts, person).map(heads_of) else {
            continue;
        };
        let mut matched = false;
        'heads: for head in profiles {
            let Ok(snapshot) = profile_snapshot(facts, head) else {
                continue;
            };
            for handle in std::iter::once(snapshot.label).chain(snapshot.aliases) {
                if lookup_key(&load_text_from(reader, handle)?) == key {
                    matched = true;
                    break 'heads;
                }
            }
        }
        if !matched {
            continue;
        }
        let retired = match lifecycle_head(facts, person) {
            Ok(Head::Missing) => false,
            Ok(head) => heads_of(head)
                .into_iter()
                .all(|id| lifecycle_snapshot(facts, id).is_ok_and(|snapshot| snapshot.retired)),
            Err(_) => false,
        };
        if !retired {
            targets.insert(person);
        }
    }
    for group_id in group_anchors(facts) {
        let Ok(heads) = group_head(facts, group_id).map(heads_of) else {
            continue;
        };
        for head in heads {
            let Ok(snapshot) = group_snapshot(facts, head) else {
                continue;
            };
            if lookup_key(&load_text_from(reader, snapshot.name)?) == key {
                targets.insert(group_id);
                break;
            }
        }
    }
    Ok(targets)
}

/// Settled semantic relation between two exact person anchors.
///
/// `Unknown` means there is no settled path proving either outcome. Mixed
/// verdict forks and contradictions are errors rather than a fourth value:
/// callers must keep that evidence visible instead of silently treating it as
/// absence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityRelation {
    Same,
    Distinct,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct IdentityComponents {
    parent: HashMap<Id, Id>,
    distinct_components: BTreeSet<(Id, Id)>,
    contradictions: BTreeSet<Id>,
    poisoned_components: BTreeSet<Id>,
    forked_pairs: BTreeSet<(Id, Id)>,
    mixed_forked_pairs: BTreeSet<(Id, Id)>,
}

fn find_root(parent: &HashMap<Id, Id>, mut id: Id) -> Id {
    while let Some(&next) = parent.get(&id) {
        if next == id {
            break;
        }
        id = next;
    }
    id
}

fn union_roots(parent: &mut HashMap<Id, Id>, first: Id, second: Id) {
    let first = find_root(parent, first);
    let second = find_root(parent, second);
    if first == second {
        return;
    }
    let (low, high) = if first < second {
        (first, second)
    } else {
        (second, first)
    };
    parent.insert(high, low);
}

impl IdentityComponents {
    pub fn from_facts<P>(facts: &P) -> Result<Self>
    where
        P: TriblePattern,
    {
        let people = person_anchors(facts);
        let mut parent: HashMap<Id, Id> = people.iter().map(|&id| (id, id)).collect();
        let pairs: BTreeSet<(Id, Id)> = find!(
            (low: Id, high: Id),
            pattern!(facts, [
                { _?verdict @
                    metadata::tag: &KIND_IDENTITY_VERDICT,
                    identity::low: ?low,
                    identity::high: ?high,
                    identity::same: _?same,
                },
                { ?low @ metadata::tag: &KIND_PERSON_ID },
                { ?high @ metadata::tag: &KIND_PERSON_ID },
            ])
        )
        .filter(|(low, high)| low < high)
        .collect();

        let mut same = Vec::new();
        let mut distinct = Vec::new();
        let mut forked_pairs = BTreeSet::new();
        let mut mixed_forked_pairs = BTreeSet::new();
        for (low, high) in pairs {
            match identity_head(facts, low, high)? {
                Head::Unique(id) => {
                    let verdict = identity_verdict(facts, id)?;
                    if verdict.same {
                        same.push((low, high));
                    } else {
                        distinct.push((low, high));
                    }
                }
                Head::Forked(heads) => {
                    forked_pairs.insert((low, high));
                    let values: BTreeSet<bool> = heads
                        .into_iter()
                        .map(|id| Ok(identity_verdict(facts, id)?.same))
                        .collect::<Result<_>>()?;
                    if values.len() == 1 && values.contains(&true) {
                        same.push((low, high));
                    } else if values.len() == 1 && values.contains(&false) {
                        distinct.push((low, high));
                    } else {
                        mixed_forked_pairs.insert((low, high));
                    }
                }
                Head::Missing => unreachable!("pair came from a verdict"),
            }
        }

        // Agreement-only lineage forks are still reported through
        // `forked_pairs`, but their semantic value is settled and may safely
        // participate in equality. Only mixed same/distinct forks are
        // semantically indeterminate.
        for (low, high) in same {
            union_roots(&mut parent, low, high);
        }

        let mut contradictions = BTreeSet::new();
        let mut distinct_components = BTreeSet::new();
        for (low, high) in distinct {
            let low_root = find_root(&parent, low);
            let high_root = find_root(&parent, high);
            if low_root == high_root {
                contradictions.insert(low_root);
            } else if low_root < high_root {
                distinct_components.insert((low_root, high_root));
            } else {
                distinct_components.insert((high_root, low_root));
            }
        }
        let mut poisoned_components = BTreeSet::new();
        for &(low, high) in &mixed_forked_pairs {
            let low_root = find_root(&parent, low);
            let high_root = find_root(&parent, high);
            if low_root == high_root {
                poisoned_components.insert(low_root);
            }
        }
        Ok(Self {
            parent,
            distinct_components,
            contradictions,
            poisoned_components,
            forked_pairs,
            mixed_forked_pairs,
        })
    }

    fn root(&self, person: Id) -> Result<Id> {
        if !self.parent.contains_key(&person) {
            bail!("unknown person anchor {person:x}");
        }
        Ok(find_root(&self.parent, person))
    }

    pub fn component(&self, person: Id) -> Result<BTreeSet<Id>> {
        let root = self.root(person)?;
        if self.contradictions.contains(&root) || self.poisoned_components.contains(&root) {
            bail!("identity component containing {person:x} is contradictory or unsettled");
        }
        Ok(self
            .parent
            .keys()
            .copied()
            .filter(|&candidate| find_root(&self.parent, candidate) == root)
            .collect())
    }

    /// Resolve the settled semantic relation without replacing either exact
    /// anchor with a canonical representative. Distinctness is lifted through
    /// settled same-person components in the same way as equality.
    /// An unobserved anchor still identifies itself; absent Relations evidence
    /// does not establish equality or distinctness with any other anchor.
    pub fn relation(&self, first: Id, second: Id) -> Result<IdentityRelation> {
        let first_root = find_root(&self.parent, first);
        let second_root = find_root(&self.parent, second);
        if self.contradictions.contains(&first_root)
            || self.contradictions.contains(&second_root)
            || self.poisoned_components.contains(&first_root)
            || self.poisoned_components.contains(&second_root)
        {
            bail!("identity comparison touches a contradictory or unsettled component");
        }
        if first_root == second_root {
            return Ok(IdentityRelation::Same);
        }
        let roots = if first_root < second_root {
            (first_root, second_root)
        } else {
            (second_root, first_root)
        };
        if self.mixed_forked_pairs.iter().any(|&(low, high)| {
            let pair_roots = {
                let low = find_root(&self.parent, low);
                let high = find_root(&self.parent, high);
                if low < high {
                    (low, high)
                } else {
                    (high, low)
                }
            };
            pair_roots == roots
        }) {
            bail!("identity comparison is unsettled by a forked pair verdict");
        }
        if self.distinct_components.contains(&roots) {
            Ok(IdentityRelation::Distinct)
        } else {
            Ok(IdentityRelation::Unknown)
        }
    }

    /// Boolean projection for callers that only care whether identity is
    /// settled as the same person.
    pub fn equivalent(&self, first: Id, second: Id) -> Result<bool> {
        Ok(self.relation(first, second)? == IdentityRelation::Same)
    }

    pub fn forked_pairs(&self) -> &BTreeSet<(Id, Id)> {
        &self.forked_pairs
    }

    pub fn mixed_forked_pairs(&self) -> &BTreeSet<(Id, Id)> {
        &self.mixed_forked_pairs
    }
}

/// Addressable groups containing `person`, where settled same-person
/// components participate in equality but the exact input anchor remains the
/// caller's attribution identity.
pub fn groups_for_member<P>(facts: &P, person: Id) -> Result<BTreeSet<Id>>
where
    P: TriblePattern,
{
    let identities = IdentityComponents::from_facts(facts)?;
    let mut groups = BTreeSet::new();
    for group_id in group_anchors(facts) {
        let snapshot = current_group(facts, group_id)?;
        for member in snapshot.members {
            if identities.equivalent(person, member)? {
                groups.insert(group_id);
                break;
            }
        }
    }
    Ok(groups)
}

/// Pure, fork-visible profile projection for read-only consumers such as the
/// GORBIE widgets. One malformed anchor cannot erase the rest of a roster:
/// its failure is retained as an explicit row value instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileView {
    Current { snapshot: Id, value: ProfileInput },
    Forked(Vec<Id>),
    Invalid(String),
}

pub fn person_profile_views<Store, P>(reader: &Store, facts: &P) -> Vec<(Id, ProfileView)>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    person_anchors(facts)
        .into_iter()
        .map(|person| {
            let state = match profile_head(facts, person) {
                Ok(Head::Unique(snapshot)) => profile_snapshot(facts, snapshot)
                    .and_then(|profile| profile_input(reader, &profile))
                    .map(|value| ProfileView::Current { snapshot, value })
                    .unwrap_or_else(|error| ProfileView::Invalid(error.to_string())),
                Ok(Head::Forked(heads)) => ProfileView::Forked(heads),
                Ok(Head::Missing) => {
                    ProfileView::Invalid(format!("person {person:x} has no profile snapshot"))
                }
                Err(error) => ProfileView::Invalid(error.to_string()),
            };
            (person, state)
        })
        .collect()
}

/// Every current person profile, preserving exact anchor identity. Retired
/// people are included or excluded explicitly; a fork remains an error.
pub fn current_people<Store, P>(
    reader: &Store,
    facts: &P,
    include_retired: bool,
) -> Result<Vec<(Id, ProfileInput)>>
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    let mut people = Vec::new();
    for person in person_anchors(facts) {
        if !include_retired && person_is_retired(facts, person)? {
            continue;
        }
        let profile = current_profile(facts, person)?;
        people.push((person, profile_input(reader, &profile)?));
    }
    people.sort_by(|(left_id, left), (right_id, right)| {
        lookup_key(&left.label)
            .cmp(&lookup_key(&right.label))
            .then_with(|| left_id.cmp(right_id))
    });
    Ok(people)
}

/// Return all canonical identity pairs and their fork-visible heads.
pub fn identity_heads<P>(facts: &P) -> Result<BTreeMap<(Id, Id), Head>>
where
    P: TriblePattern,
{
    let pairs: BTreeSet<(Id, Id)> = find!(
        (low: Id, high: Id),
        pattern!(facts, [
            { _?verdict @
                metadata::tag: &KIND_IDENTITY_VERDICT,
                identity::low: ?low,
                identity::high: ?high,
                identity::same: _?same,
            },
            { ?low @ metadata::tag: &KIND_PERSON_ID },
            { ?high @ metadata::tag: &KIND_PERSON_ID },
        ])
    )
    .filter(|(low, high)| low < high)
    .collect();
    pairs
        .into_iter()
        .map(|pair| Ok((pair, identity_head(facts, pair.0, pair.1)?)))
        .collect()
}

// ── ties ────────────────────────────────────────────────────────────────
//
// A tie is one claim, "subject <relation> object [as role]", between two
// Relations entries. Its id derives from exactly that claim, so asserting it
// twice converges on one entity and each assertion's sources accumulate on
// it. Validity, sources, observation time and author are annotations on the
// tie id; ending a tie appends `metadata::finished_at`, never a deletion.
// Readers query ties at the point of use and display every fact they find:
// a second `finished_at` or an unknown extra fact is shown or ignored, never
// refused.

/// Build the intrinsic identity core of one tie. Its root is the tie id.
pub fn tie_fragment(
    subject: Id,
    relation: impl Into<String>,
    object: Id,
    role: Option<String>,
) -> Result<Fragment> {
    if subject == object {
        bail!("a tie needs two different entries");
    }
    let relation = required_text(relation, "relation")?;
    let role = optional_text(role, "role")?;
    let mut fragment = Fragment::empty();
    let relation = fragment.put(relation);
    let role = role.map(|role| fragment.put(role));
    fragment += entity! {
        metadata::tag: &KIND_TIE,
        tie::subject: &subject,
        tie::relation: relation,
        tie::object: &object,
        tie::role?: role,
    };
    Ok(fragment)
}

/// Additive annotations on one tie id. Every field accumulates: a second
/// publication with other sources adds them beside the first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TieAnnotation {
    pub sources: Vec<String>,
    pub started_at: Option<ObservedAt>,
    pub finished_at: Option<ObservedAt>,
    pub created_at: Option<ObservedAt>,
    pub author: Option<Id>,
}

pub fn tie_annotation_fragment(tie: Id, annotation: TieAnnotation) -> Result<Fragment> {
    let sources = text_set(annotation.sources, "source")?;
    Ok(entity! { ExclusiveId::force_ref(&tie) @
        legacy::source*: sources.iter().map(String::as_str),
        metadata::started_at?: annotation.started_at,
        metadata::finished_at?: annotation.finished_at,
        metadata::created_at?: annotation.created_at,
        tie::author?: annotation.author.as_ref(),
    })
}

/// One decodable tie claim as found in the facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TieClaim {
    pub id: Id,
    pub subject: Id,
    pub relation: TextHandle,
    pub object: Id,
}

/// Every tie claim with `anchor` as subject or object, sorted by id. A tie
/// entity carrying more than one relation or endpoint (open world) yields one
/// claim per combination rather than a refusal.
pub fn ties_touching<P>(facts: &P, anchor: Id) -> Vec<TieClaim>
where
    P: TriblePattern,
{
    let outgoing = find!(
        (id: Id, relation: TextHandle, object: Id),
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_TIE,
            tie::subject: anchor,
            tie::relation: ?relation,
            tie::object: ?object,
        }])
    )
    .map(|(id, relation, object)| (id, anchor, relation.raw, object, relation));
    let incoming = find!(
        (id: Id, subject: Id, relation: TextHandle),
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_TIE,
            tie::subject: ?subject,
            tie::relation: ?relation,
            tie::object: anchor,
        }])
    )
    .map(|(id, subject, relation)| (id, subject, relation.raw, anchor, relation));
    let claims: BTreeMap<_, TextHandle> = outgoing
        .chain(incoming)
        .map(|(id, subject, raw, object, relation)| ((id, subject, raw, object), relation))
        .collect();
    claims
        .into_iter()
        .map(|((id, subject, _, object), relation)| TieClaim {
            id,
            subject,
            relation,
            object,
        })
        .collect()
}

/// Every tie claim carried by tie entity `tie`.
pub fn tie_claims<P>(facts: &P, tie: Id) -> Vec<TieClaim>
where
    P: TriblePattern,
{
    let mut claims: Vec<TieClaim> = find!(
        (subject: Id, relation: TextHandle, object: Id),
        pattern!(facts, [{ tie @
            metadata::tag: &KIND_TIE,
            tie::subject: ?subject,
            tie::relation: ?relation,
            tie::object: ?object,
        }])
    )
    .map(|(subject, relation, object)| TieClaim {
        id: tie,
        subject,
        relation,
        object,
    })
    .collect();
    claims.sort_by(|left, right| {
        (left.subject, left.relation.raw, left.object).cmp(&(
            right.subject,
            right.relation.raw,
            right.object,
        ))
    });
    claims
}

/// Every role on a tie, sorted (one for an intrinsic tie that has a role).
pub fn tie_roles<P>(facts: &P, tie: Id) -> Vec<TextHandle>
where
    P: TriblePattern,
{
    sorted_handles(find!(v: TextHandle, pattern!(facts, [{ tie @ tie::role: ?v }])))
}

/// Every `metadata::started_at` annotation on `entity`, in order.
pub fn started_at<P>(facts: &P, entity: Id) -> Vec<ObservedAt>
where
    P: TriblePattern,
{
    sorted_observations(find!(
        v: ObservedAt,
        pattern!(facts, [{ entity @ metadata::started_at: ?v }])
    ))
}

/// Every `metadata::finished_at` annotation on `entity`, in order.
pub fn finished_at<P>(facts: &P, entity: Id) -> Vec<ObservedAt>
where
    P: TriblePattern,
{
    sorted_observations(find!(
        v: ObservedAt,
        pattern!(facts, [{ entity @ metadata::finished_at: ?v }])
    ))
}

/// Every author a tie names, in id order.
pub fn tie_authors<P>(facts: &P, tie: Id) -> Vec<Id>
where
    P: TriblePattern,
{
    sorted_ids(find!(v: Id, pattern!(facts, [{ tie @ tie::author: ?v }])))
}

/// Every tie entity id.
pub fn tie_ids<P>(facts: &P) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    ids_of_kind(facts, KIND_TIE)
}

/// Resolve a tie by exact id or unique id prefix.
pub fn resolve_tie<P>(facts: &P, input: &str) -> SelectorOutcome
where
    P: TriblePattern,
{
    let input = input.trim().to_ascii_lowercase();
    if input.is_empty() {
        return SelectorOutcome::Invalid("empty selector".to_owned());
    }
    if !input.bytes().all(|byte| byte.is_ascii_hexdigit()) || input.len() > 32 {
        return SelectorOutcome::Invalid("a tie is selected by its id or an id prefix".to_owned());
    }
    let (_, matches) = id_candidates(&input, &tie_ids(facts));
    selector_outcome(matches, BTreeSet::new(), BTreeSet::new())
}

/// A human label for any Relations anchor: the current profile label of a
/// person or organisation, a group's current name, or its id when neither is
/// settled. Display only; never a key.
pub fn entry_label<Store, P>(reader: &Store, facts: &P, anchor: Id) -> String
where
    Store: BlobStoreGet + ?Sized,
    P: TriblePattern,
{
    for kind in entry_kinds(facts, anchor) {
        if let Ok(profile) = current_profile_of(facts, anchor, kind) {
            if let Ok(label) = load_text_from(reader, profile.label) {
                return label;
            }
        }
    }
    if let Ok(Head::Unique(snapshot)) = group_head(facts, anchor) {
        if let Ok(snapshot) = group_snapshot(facts, snapshot) {
            if let Ok(name) = load_text_from(reader, snapshot.name) {
                return name;
            }
        }
    }
    format!("{anchor:x}")
}

// ── calendar values for tie validity ────────────────────────────────────

fn period_start(year: i32, month: u8, day: u8, raw: &str) -> Result<hifitime::Epoch> {
    hifitime::Epoch::maybe_from_gregorian_utc(year, month, day, 0, 0, 0, 0)
        .map_err(|_| anyhow!("invalid date {raw:?}"))
}

/// UTC midnight of the calendar day after the day `midnight` starts. Read
/// through the calendar, not by adding 86 400 s, so a leap-second day still
/// ends at the next midnight.
fn next_midnight(midnight: hifitime::Epoch) -> hifitime::Epoch {
    let (year, month, day, ..) = (midnight + hifitime::Duration::from_days(1.5)).to_gregorian_utc();
    hifitime::Epoch::from_gregorian_utc_at_midnight(year, month, day)
}

fn last_nanosecond_before(next: hifitime::Epoch) -> hifitime::Epoch {
    next - hifitime::Duration::from_truncated_nanoseconds(1)
}

/// Parse one calendar value into the interval it denotes: `YYYY` (the whole
/// year), `YYYY-MM` (the month), `YYYY-MM-DD` (the day), or
/// `YYYY-MM-DDTHH:MM[:SS][Z]` (an instant, UTC).
fn parse_period(raw: &str) -> Result<(hifitime::Epoch, hifitime::Epoch)> {
    let text = raw.trim();
    let invalid = || {
        anyhow!("invalid time {raw:?}; expected YYYY, YYYY-MM, YYYY-MM-DD or YYYY-MM-DDTHH:MM[:SS] (UTC)")
    };
    if text.contains('T') || text.contains(' ') {
        let interval = crate::care::parse_time(text).map_err(|_| invalid())?;
        return interval
            .try_from_inline::<(hifitime::Epoch, hifitime::Epoch)>()
            .map_err(|_| invalid());
    }
    let parts: Vec<&str> = text.split('-').collect();
    let number = |value: &str| value.parse::<u32>().map_err(|_| invalid());
    let year = parts
        .first()
        .ok_or_else(invalid)?
        .parse::<i32>()
        .map_err(|_| invalid())?;
    match parts.as_slice() {
        [_] => Ok((
            period_start(year, 1, 1, raw)?,
            last_nanosecond_before(period_start(year + 1, 1, 1, raw)?),
        )),
        [_, month] => {
            let month = number(month)?;
            if !(1..=12).contains(&month) {
                return Err(invalid());
            }
            let (next_year, next_month) = if month == 12 {
                (year + 1, 1)
            } else {
                (year, month + 1)
            };
            Ok((
                period_start(year, month as u8, 1, raw)?,
                last_nanosecond_before(period_start(next_year, next_month as u8, 1, raw)?),
            ))
        }
        [_, month, day] => {
            let start = period_start(year, number(month)? as u8, number(day)? as u8, raw)?;
            Ok((start, last_nanosecond_before(next_midnight(start))))
        }
        _ => Err(invalid()),
    }
}

/// Parse a validity bound: one calendar value (see above) or `A..B`, the
/// span from the start of `A` to the end of `B`. Coarse values keep their
/// uncertainty: "since 2019" is the whole of 2019, not its first instant.
pub fn parse_when(raw: &str) -> Result<ObservedAt> {
    let (from, to) = match raw.split_once("..") {
        Some((from, to)) => (parse_period(from)?.0, parse_period(to)?.1),
        None => parse_period(raw)?,
    };
    if to < from {
        bail!("time {raw:?} ends before it starts");
    }
    (from, to)
        .try_to_inline()
        .map_err(|error| anyhow!("encode time {raw:?}: {error:?}"))
}

/// Render a validity value the way [`parse_when`] reads it: a whole year,
/// month or day as `YYYY`, `YYYY-MM` or `YYYY-MM-DD`, otherwise as UTC.
pub fn format_when(value: ObservedAt) -> String {
    let Ok((from, to)) = value.try_from_inline::<(hifitime::Epoch, hifitime::Epoch)>() else {
        return "?".to_owned();
    };
    let (year, month, day, hour, minute, second, nanos) = from.to_gregorian_utc();
    if (hour, minute, second, nanos) == (0, 0, 0, 0) {
        let whole = |next: Result<hifitime::Epoch>| {
            next.map(|next| last_nanosecond_before(next) == to)
                .unwrap_or(false)
        };
        if month == 1 && day == 1 && whole(period_start(year + 1, 1, 1, "")) {
            return format!("{year:04}");
        }
        let (next_year, next_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        if day == 1 && whole(period_start(next_year, next_month, 1, "")) {
            return format!("{year:04}-{month:02}");
        }
        if whole(Ok(next_midnight(from))) {
            return format!("{year:04}-{month:02}-{day:02}");
        }
    }
    crate::care::format_time(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fixture {
        facts: RefCell<TribleSet>,
        blobs: RefCell<MemoryBlobStore>,
    }

    struct FixtureView {
        facts: TribleSet,
        reader: MemoryBlobStoreSnapshot,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                facts: RefCell::new(TribleSet::new()),
                blobs: RefCell::new(MemoryBlobStore::new()),
            }
        }

        fn publish(&self, fragment: Fragment) {
            let (facts, blobs) = fragment.into_facts_and_blobs();
            *self.facts.borrow_mut() += facts;
            self.blobs.borrow_mut().union(blobs);
        }

        fn view(&self) -> FixtureView {
            FixtureView {
                facts: self.facts.borrow().clone(),
                reader: self.blobs.borrow_mut().snapshot().unwrap(),
            }
        }
    }

    fn profile(label: &str) -> ProfileInput {
        ProfileInput {
            label: label.to_owned(),
            ..ProfileInput::default()
        }
    }

    fn publish_person(fixture: &Fixture, person: Id, label: &str) {
        let (fragment, _, _) = person_fragment(person, profile(label)).unwrap();
        fixture.publish(fragment);
    }

    fn observed_at(day: u8) -> ObservedAt {
        let epoch = hifitime::Epoch::from_gregorian_utc(2026, 8, day, 12, 0, 0, 0);
        (epoch, epoch).try_to_inline().unwrap()
    }

    #[test]
    fn profile_identity_is_order_independent_and_semantic() {
        let person = genid().id;
        let predecessor_a = genid().id;
        let predecessor_b = genid().id;
        let mut first = profile(" Ada ");
        first.aliases = vec!["Countess".into(), "Ada".into(), "Countess".into()];
        first.emails = vec!["ada@example.test".into(), "a@example.test".into()];
        let mut second = profile("Ada");
        second.aliases = vec!["Ada".into(), "Countess".into()];
        second.emails = vec!["a@example.test".into(), "ada@example.test".into()];

        let a = profile_fragment(person, first, &[predecessor_b, predecessor_a]).unwrap();
        let b = profile_fragment(person, second, &[predecessor_a, predecessor_b]).unwrap();
        assert_eq!(a.root(), b.root());

        let mut changed = profile("Ada Lovelace");
        changed.aliases = vec!["Ada".into(), "Countess".into()];
        changed.emails = vec!["a@example.test".into(), "ada@example.test".into()];
        let changed = profile_fragment(person, changed, &[predecessor_a, predecessor_b]).unwrap();
        assert_ne!(a.root(), changed.root());
    }

    #[test]
    fn later_provenance_union_does_not_move_or_fork_snapshot_heads() {
        let fixture = Fixture::new();
        let person = genid().id;
        let group = genid().id;
        publish_person(&fixture, person, "Ada");
        let (group_fragment, _) = group_create_fragment(group, "crew").unwrap();
        fixture.publish(group_fragment);

        let before = fixture.view();
        let profile_before = profile_head(&before.facts, person).unwrap();
        let lifecycle_before = lifecycle_head(&before.facts, person).unwrap();
        let group_before = group_head(&before.facts, group).unwrap();

        fixture.publish(
            person_provenance_fragment(
                person,
                vec!["linkedin".into(), "mail".into()],
                &[observed_at(9)],
            )
            .unwrap(),
        );
        fixture.publish(
            person_provenance_fragment(
                person,
                vec!["mail".into(), "crm".into()],
                &[observed_at(8)],
            )
            .unwrap(),
        );
        fixture.publish(group_provenance_fragment(group, &[observed_at(10)]));

        let after = fixture.view();
        assert_eq!(profile_head(&after.facts, person).unwrap(), profile_before);
        assert_eq!(
            lifecycle_head(&after.facts, person).unwrap(),
            lifecycle_before
        );
        assert_eq!(group_head(&after.facts, group).unwrap(), group_before);
        assert_eq!(
            person_sources(&after.reader, &after.facts, person).unwrap(),
            vec!["crm", "linkedin", "mail"]
        );
    }

    #[test]
    fn creation_observation_minimum_is_order_and_duplicate_independent() {
        let person = genid().id;
        let early = observed_at(8);
        let late = observed_at(10);
        let first = person_provenance_fragment(
            person,
            vec!["mail".into(), "crm".into(), "mail".into()],
            &[late, early, late],
        )
        .unwrap();
        let second =
            person_provenance_fragment(person, vec!["crm".into(), "mail".into()], &[early, late])
                .unwrap();
        assert_eq!(first.facts(), second.facts());

        let facts = first.into_facts();
        assert_eq!(creation_observations(&facts, person), vec![early, late]);
        assert_eq!(earliest_creation_observation(&facts, person), Some(early));
    }

    #[test]
    fn same_identity_verdict_is_symmetric_and_fork_visible() {
        let a = genid().id;
        let b = genid().id;
        let same_ab = identity_verdict_fragment(a, b, true, &[]).unwrap();
        let same_ba = identity_verdict_fragment(b, a, true, &[]).unwrap();
        assert_eq!(same_ab.root(), same_ba.root());

        let distinct = identity_verdict_fragment(a, b, false, &[]).unwrap();
        let mut facts = same_ab.into_facts();
        facts += distinct;
        assert!(matches!(
            identity_head(&facts, a, b).unwrap(),
            Head::Forked(_)
        ));
    }

    #[test]
    fn profile_forks_remain_visible() {
        let fixture = Fixture::new();
        let person = genid().id;
        publish_person(&fixture, person, "Ada");
        let view = fixture.view();
        let first_head = match profile_head(&view.facts, person).unwrap() {
            Head::Unique(id) => id,
            other => panic!("expected unique profile, got {other:?}"),
        };
        fixture.publish(profile_fragment(person, profile("Ada One"), &[first_head]).unwrap());
        fixture.publish(profile_fragment(person, profile("Ada Two"), &[first_head]).unwrap());
        let view = fixture.view();
        assert!(matches!(
            profile_head(&view.facts, person).unwrap(),
            Head::Forked(_)
        ));
        assert!(matches!(
            person_profile_views(&view.reader, &view.facts).as_slice(),
            [(id, ProfileView::Forked(heads))] if *id == person && heads.len() == 2
        ));
    }

    #[test]
    fn native_views_ignore_preserved_legacy_rows_and_unrelated_facts() {
        let fixture = Fixture::new();
        let person = genid().id;
        let other = genid().id;
        publish_person(&fixture, person, "Ada");
        publish_person(&fixture, other, "Other");
        let group = genid().id;
        let (group_fragment, initial_group) = group_create_fragment(group, "crew").unwrap();
        fixture.publish(group_fragment);
        fixture
            .publish(group_snapshot_fragment(group, "crew", &[person], &[initial_group]).unwrap());

        let before = fixture.view();
        let profile_before = profile_head(&before.facts, person).unwrap();
        let lifecycle_before = lifecycle_head(&before.facts, person).unwrap();
        let group_before = group_head(&before.facts, group).unwrap();

        let old_group_snapshot = genid().id;
        let unrelated = genid().id;
        let missing_legacy_note: TextHandle = Inline::new([0xEE; 32]);
        let mut legacy = Fragment::empty();
        let old_group_name = legacy.put("historical crew".to_owned());
        legacy += entity! { ExclusiveId::force_ref(&person) @
            metadata::name: old_group_name,
            metadata::description: missing_legacy_note,
            legacy::same_as: &other,
        };
        legacy += entity! { ExclusiveId::force_ref(&old_group_snapshot) @
            group::snapshot_of: &group,
            metadata::name: old_group_name,
            group::member: &other,
        };
        legacy += entity! {
            metadata::tag: &crate::schemas::relations::KIND_RETIRE_ID,
            legacy::subject: &person,
            metadata::created_at: observed_at(11),
        };
        legacy += entity! { ExclusiveId::force_ref(&unrelated) @
            metadata::tag: &genid().id,
        };
        fixture.publish(legacy);

        let after = fixture.view();
        assert_eq!(profile_head(&after.facts, person).unwrap(), profile_before);
        assert_eq!(
            lifecycle_head(&after.facts, person).unwrap(),
            lifecycle_before
        );
        assert_eq!(group_head(&after.facts, group).unwrap(), group_before);
        assert_eq!(
            profile_input(
                &after.reader,
                &current_profile(&after.facts, person).unwrap()
            )
            .unwrap()
            .label,
            "Ada"
        );
        assert!(!person_is_retired(&after.facts, person).unwrap());
        assert_eq!(
            current_group(&after.facts, group).unwrap().members,
            vec![person]
        );
        assert_eq!(
            groups_for_member(&after.facts, person).unwrap(),
            [group].into()
        );
        assert!(identity_heads(&after.facts).unwrap().is_empty());
        assert!(group_snapshot(&after.facts, old_group_snapshot).is_err());
        assert!(profile_snapshot(&after.facts, person).is_err());
    }

    #[test]
    fn lookup_is_computed_and_ambiguity_is_visible() {
        let fixture = Fixture::new();
        let ada = genid().id;
        let other = genid().id;
        let mut first = profile("Ada");
        first.aliases = vec!["Countess".into()];
        let (fragment, _, _) = person_fragment(ada, first).unwrap();
        fixture.publish(fragment);
        publish_person(&fixture, other, "Other");
        let view = fixture.view();
        assert_eq!(
            resolve_person(&view.reader, &view.facts, "  COUNTESS ", false).unwrap(),
            SelectorOutcome::Unique(ada)
        );

        let third = genid().id;
        publish_person(&fixture, third, "countess");
        let view = fixture.view();
        assert_eq!(
            resolve_person(&view.reader, &view.facts, "countess", false).unwrap(),
            SelectorOutcome::Ambiguous(vec![ada.min(third), ada.max(third)])
        );
    }

    #[test]
    fn exact_ids_do_not_require_unrelated_or_selected_label_blobs() {
        let fixture = Fixture::new();
        let resident = fixture.view().reader;
        let ada = genid().id;
        let other = genid().id;
        let group = genid().id;
        publish_person(&fixture, ada, "Ada");
        publish_person(&fixture, other, "Other");
        let (fragment, _) = group_create_fragment(group, "crew").unwrap();
        fixture.publish(fragment);
        let facts = fixture.facts.borrow();

        assert_eq!(
            resolve_person(&resident, &*facts, &format!("{ada:x}"), false).unwrap(),
            SelectorOutcome::Unique(ada),
        );
        assert_eq!(
            resolve_group(&resident, &*facts, &format!("{group:x}")).unwrap(),
            SelectorOutcome::Unique(group),
        );
        // Name selection still needs the names; an opaque ID is not a label.
        assert!(resolve_person(&resident, &*facts, "Ada", false).is_err());
    }

    /// Fork one person's profile track into two un-superseded heads that both
    /// still answer to `alias`.
    fn fork_profile(fixture: &Fixture, person: Id, label: &str, alias: &str, base: Id) {
        for note in ["left", "right"] {
            let mut input = profile(label);
            input.aliases = vec![alias.to_owned()];
            input.note = Some(note.to_owned());
            fixture.publish(profile_fragment(person, input, &[base]).unwrap());
        }
    }

    /// A broadcast outage in miniature. A retired legacy anchor answering to a
    /// shared selector carried a profile fork, and that fork vetoed the live,
    /// entirely settled candidate.
    #[test]
    fn a_retired_anchor_is_dropped_before_its_fork_can_block_the_selector() {
        let fixture = Fixture::new();
        let legacy = genid().id;
        let live = genid().id;

        let mut input = profile("legacy shared");
        input.aliases = vec!["shared".into()];
        let (fragment, profile_id, lifecycle_id) = person_fragment(legacy, input).unwrap();
        fixture.publish(fragment);
        fork_profile(&fixture, legacy, "legacy shared", "shared", profile_id);
        fixture.publish(lifecycle_fragment(legacy, true, &[lifecycle_id]));

        publish_person(&fixture, live, "shared");
        let view = fixture.view();
        assert!(matches!(
            profile_head(&view.facts, legacy).unwrap(),
            Head::Forked(_)
        ));
        assert!(person_is_retired(&view.facts, legacy).unwrap());

        // Retirement disqualifies before the fork is ever consulted.
        assert_eq!(
            resolve_person(&view.reader, &view.facts, "shared", false).unwrap(),
            SelectorOutcome::Unique(live)
        );
        // An operator who explicitly asks for retired anchors still sees the
        // fork, and sees which side of the tie is the blocker.
        assert_eq!(
            resolve_person(&view.reader, &view.facts, "shared", true).unwrap(),
            SelectorOutcome::Forked {
                forked: vec![legacy],
                settled: vec![live],
            }
        );
    }

    /// The fail-closed half of the rule: a fork on a candidate that really is
    /// in the running must keep blocking, and must be named as the blocker.
    #[test]
    fn a_forked_live_anchor_still_blocks_and_is_named_as_the_blocker() {
        let fixture = Fixture::new();
        let forked_person = genid().id;
        let settled_person = genid().id;

        let mut input = profile("ada");
        input.aliases = vec!["countess".into()];
        let (fragment, profile_id, _) = person_fragment(forked_person, input).unwrap();
        fixture.publish(fragment);
        fork_profile(&fixture, forked_person, "ada", "countess", profile_id);
        publish_person(&fixture, settled_person, "countess");

        let view = fixture.view();
        let outcome = resolve_person(&view.reader, &view.facts, "countess", false).unwrap();
        assert_eq!(
            outcome,
            SelectorOutcome::Forked {
                forked: vec![forked_person],
                settled: vec![settled_person],
            }
        );
        assert_eq!(
            outcome.candidates(),
            sorted_ids([forked_person, settled_person])
        );

        let message = resolve_person(&view.reader, &view.facts, "countess", false)
            .unwrap()
            .require_unique("person", "countess")
            .unwrap_err()
            .to_string();
        // The blocker is named as the blocker, and the innocent match is not
        // listed as though it were one.
        assert!(
            message.contains(&format!("state on {forked_person:x}")),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "not selectable while the fork stands: {settled_person:x}"
            )),
            "{message}"
        );
    }

    /// A fork only blocks when it could change the answer. The one thing a
    /// lifecycle track decides for a selector is addressability, so heads that
    /// agree are settled for that purpose however the fork later resolves.
    #[test]
    fn a_lifecycle_fork_blocks_only_when_its_heads_disagree() {
        let fixture = Fixture::new();
        let person = genid().id;
        let (fragment, _, active) = person_fragment(person, profile("ada")).unwrap();
        fixture.publish(fragment);

        let resolve = |view: &FixtureView| resolve_person(&view.reader, &view.facts, "ada", false);
        // The two un-superseded lifecycle heads, split by what they claim.
        let split = |fixture: &Fixture| -> (Vec<Id>, Vec<Id>) {
            let facts = fixture.view().facts;
            let heads = match lifecycle_head(&facts, person).unwrap() {
                Head::Forked(heads) => heads,
                other => panic!("expected a fork, got {other:?}"),
            };
            heads
                .into_iter()
                .partition(|id| lifecycle_snapshot(&facts, *id).unwrap().retired)
        };

        // Two windows disagree about whether Ada is retired: indeterminate.
        fixture.publish(lifecycle_fragment(person, true, &[active]));
        fixture.publish(lifecycle_fragment(person, false, &[active]));
        let (says_retired, says_active) = split(&fixture);
        let view = fixture.view();
        assert!(matches!(
            resolve(&view).unwrap(),
            SelectorOutcome::Forked { .. }
        ));

        // Still two heads, but now both say active: Ada is addressable.
        fixture.publish(lifecycle_fragment(person, false, &says_retired));
        let view = fixture.view();
        assert!(matches!(
            lifecycle_head(&view.facts, person).unwrap(),
            Head::Forked(_)
        ));
        assert_eq!(resolve(&view).unwrap(), SelectorOutcome::Unique(person));

        // Disagreement returns: indeterminate again.
        fixture.publish(lifecycle_fragment(person, true, &says_active));
        let view = fixture.view();
        assert!(matches!(
            resolve(&view).unwrap(),
            SelectorOutcome::Forked { .. }
        ));

        // Both heads now say retired: disqualified, and reported as retired
        // rather than as a fork.
        let (_, still_active) = split(&fixture);
        fixture.publish(lifecycle_fragment(person, true, &still_active));
        let view = fixture.view();
        assert!(matches!(
            lifecycle_head(&view.facts, person).unwrap(),
            Head::Forked(_)
        ));
        match resolve(&view).unwrap() {
            SelectorOutcome::Invalid(reason) => {
                assert!(reason.contains("retired"), "{reason}");
                assert!(reason.contains(&format!("{person:x}")), "{reason}");
            }
            other => panic!("expected a retired verdict, got {other:?}"),
        }
        assert_eq!(
            resolve_person(&view.reader, &view.facts, "ada", true).unwrap(),
            SelectorOutcome::Unique(person)
        );
    }

    #[test]
    fn lifecycle_has_no_clock_arbitration() {
        let fixture = Fixture::new();
        let person = genid().id;
        publish_person(&fixture, person, "Ada");
        let view = fixture.view();
        let initial = match lifecycle_head(&view.facts, person).unwrap() {
            Head::Unique(id) => id,
            _ => panic!("expected lifecycle head"),
        };
        fixture.publish(lifecycle_fragment(person, true, &[initial]));
        fixture.publish(lifecycle_fragment(person, false, &[initial]));
        let view = fixture.view();
        assert!(matches!(
            lifecycle_head(&view.facts, person).unwrap(),
            Head::Forked(_)
        ));
        assert!(person_is_retired(&view.facts, person).is_err());
    }

    #[test]
    fn group_reconciliation_carries_the_predecessor_member_union() {
        let fixture = Fixture::new();
        let first = genid().id;
        let second = genid().id;
        publish_person(&fixture, first, "First");
        publish_person(&fixture, second, "Second");

        let group = genid().id;
        let (initial, initial_id) = group_create_fragment(group, "crew").unwrap();
        fixture.publish(initial);
        let left = group_snapshot_fragment(group, "crew", &[first], &[initial_id]).unwrap();
        let left_id = left.root().unwrap();
        fixture.publish(left);
        let right = group_snapshot_fragment(group, "crew", &[second], &[initial_id]).unwrap();
        let right_id = right.root().unwrap();
        fixture.publish(right);

        let fork = fixture.view();
        assert!(matches!(
            group_head(&fork.facts, group).unwrap(),
            Head::Forked(_)
        ));

        let reconciled =
            reconcile_group_fragment(&fork.facts, group, "crew", &[right_id, left_id]).unwrap();
        let mut settled = fork.facts.clone();
        settled += reconciled.facts().clone();
        let head = match group_head(&settled, group).unwrap() {
            Head::Unique(head) => head,
            other => panic!("expected reconciled group head, got {other:?}"),
        };
        assert_eq!(
            group_snapshot(&settled, head).unwrap().members,
            vec![first.min(second), first.max(second)]
        );
    }

    #[test]
    fn settled_same_components_apply_to_group_membership_without_rewriting_persona() {
        let fixture = Fixture::new();
        let configured_persona = genid().id;
        let equivalent_anchor = genid().id;
        publish_person(&fixture, configured_persona, "Configured");
        publish_person(&fixture, equivalent_anchor, "Imported");
        let group_id = genid().id;
        let (group_fragment, _) = group_create_fragment(group_id, "crew").unwrap();
        fixture.publish(group_fragment);
        let view = fixture.view();
        let group_head = match group_head(&view.facts, group_id).unwrap() {
            Head::Unique(id) => id,
            _ => panic!("expected group head"),
        };
        fixture.publish(
            group_snapshot_fragment(group_id, "crew", &[equivalent_anchor], &[group_head]).unwrap(),
        );
        fixture.publish(
            identity_verdict_fragment(configured_persona, equivalent_anchor, true, &[]).unwrap(),
        );
        let view = fixture.view();
        let identities = IdentityComponents::from_facts(&view.facts).unwrap();
        assert!(identities
            .equivalent(configured_persona, equivalent_anchor)
            .unwrap());
        assert_eq!(
            groups_for_member(&view.facts, configured_persona).unwrap(),
            BTreeSet::from([group_id])
        );
        assert_eq!(configured_persona, configured_persona);
    }

    #[test]
    fn unobserved_anchors_are_reflexive_without_inventing_identity_evidence() {
        let absent = genid().id;
        let other_absent = genid().id;
        let known = genid().id;
        let facts = entity! {
            ExclusiveId::force_ref(&known) @ metadata::tag: &KIND_PERSON_ID
        };
        let identities = IdentityComponents::from_facts(facts.facts()).unwrap();

        assert_eq!(
            identities.relation(absent, absent).unwrap(),
            IdentityRelation::Same
        );
        assert!(identities.equivalent(absent, absent).unwrap());
        for other in [other_absent, known] {
            assert_eq!(
                identities.relation(absent, other).unwrap(),
                IdentityRelation::Unknown
            );
            assert_eq!(
                identities.relation(other, absent).unwrap(),
                IdentityRelation::Unknown
            );
            assert!(!identities.equivalent(absent, other).unwrap());
        }
        // Comparison does not declare the anchor or relax component selection.
        assert!(identities.component(absent).is_err());
        assert_eq!(
            identities.component(known).unwrap(),
            BTreeSet::from([known])
        );
    }

    #[test]
    fn distinctness_propagates_through_settled_same_components() {
        let fixture = Fixture::new();
        let a = genid().id;
        let b = genid().id;
        let c = genid().id;
        let unknown = genid().id;
        publish_person(&fixture, a, "A");
        publish_person(&fixture, b, "B");
        publish_person(&fixture, c, "C");
        publish_person(&fixture, unknown, "Unknown");
        fixture.publish(identity_verdict_fragment(a, b, false, &[]).unwrap());
        fixture.publish(identity_verdict_fragment(b, c, true, &[]).unwrap());

        let view = fixture.view();
        let identities = IdentityComponents::from_facts(&view.facts).unwrap();
        assert_eq!(identities.relation(b, c).unwrap(), IdentityRelation::Same);
        assert_eq!(
            identities.relation(a, c).unwrap(),
            IdentityRelation::Distinct
        );
        assert_eq!(
            identities.relation(a, unknown).unwrap(),
            IdentityRelation::Unknown
        );
        assert!(!identities.equivalent(a, c).unwrap());
    }

    #[test]
    fn transitive_same_and_distinct_is_a_visible_contradiction() {
        let fixture = Fixture::new();
        let a = genid().id;
        let b = genid().id;
        let c = genid().id;
        publish_person(&fixture, a, "A");
        publish_person(&fixture, b, "B");
        publish_person(&fixture, c, "C");
        fixture.publish(identity_verdict_fragment(a, b, true, &[]).unwrap());
        fixture.publish(identity_verdict_fragment(b, c, true, &[]).unwrap());
        fixture.publish(identity_verdict_fragment(a, c, false, &[]).unwrap());
        let view = fixture.view();
        let identities = IdentityComponents::from_facts(&view.facts).unwrap();
        assert!(identities.equivalent(a, c).is_err());
        assert!(identities.equivalent(a, a).is_err());
        let absent = genid().id;
        assert!(identities.relation(a, absent).is_err());
        assert!(identities.relation(absent, a).is_err());
    }

    #[test]
    fn mixed_fork_inside_a_settled_component_poisons_that_component() {
        let fixture = Fixture::new();
        let a = genid().id;
        let b = genid().id;
        let c = genid().id;
        publish_person(&fixture, a, "A");
        publish_person(&fixture, b, "B");
        publish_person(&fixture, c, "C");
        fixture.publish(identity_verdict_fragment(a, b, true, &[]).unwrap());
        fixture.publish(identity_verdict_fragment(b, c, true, &[]).unwrap());
        fixture.publish(identity_verdict_fragment(a, c, true, &[]).unwrap());
        fixture.publish(identity_verdict_fragment(a, c, false, &[]).unwrap());

        let view = fixture.view();
        let identities = IdentityComponents::from_facts(&view.facts).unwrap();
        let pair = if a < c { (a, c) } else { (c, a) };
        assert!(identities.mixed_forked_pairs().contains(&pair));
        assert!(identities.equivalent(a, b).is_err());
        assert!(identities.equivalent(a, a).is_err());
        let absent = genid().id;
        assert!(identities.relation(a, absent).is_err());
        assert!(identities.relation(absent, a).is_err());
        assert!(identities.component(c).is_err());
    }

    #[test]
    fn mixed_fork_wins_over_distinctness_between_the_same_components() {
        let fixture = Fixture::new();
        let a = genid().id;
        let b = genid().id;
        let c = genid().id;
        publish_person(&fixture, a, "A");
        publish_person(&fixture, b, "B");
        publish_person(&fixture, c, "C");
        fixture.publish(identity_verdict_fragment(a, b, false, &[]).unwrap());
        fixture.publish(identity_verdict_fragment(b, c, true, &[]).unwrap());
        fixture.publish(identity_verdict_fragment(a, c, true, &[]).unwrap());
        fixture.publish(identity_verdict_fragment(a, c, false, &[]).unwrap());

        let view = fixture.view();
        let identities = IdentityComponents::from_facts(&view.facts).unwrap();
        assert!(identities.relation(a, b).is_err());
        assert!(identities.relation(a, c).is_err());
    }

    #[test]
    fn agreement_only_verdict_fork_is_semantically_settled_but_diagnostic() {
        let fixture = Fixture::new();
        let a = genid().id;
        let b = genid().id;
        publish_person(&fixture, a, "A");
        publish_person(&fixture, b, "B");
        let initial = identity_verdict_fragment(a, b, true, &[]).unwrap();
        let initial_id = initial.root().unwrap();
        fixture.publish(initial);
        fixture.publish(identity_verdict_fragment(a, b, true, &[initial_id]).unwrap());
        let detour = identity_verdict_fragment(a, b, false, &[initial_id]).unwrap();
        let detour_id = detour.root().unwrap();
        fixture.publish(detour);
        fixture.publish(identity_verdict_fragment(a, b, true, &[detour_id]).unwrap());

        let view = fixture.view();
        let identities = IdentityComponents::from_facts(&view.facts).unwrap();
        let pair = if a < b { (a, b) } else { (b, a) };
        assert!(identities.forked_pairs().contains(&pair));
        assert!(!identities.mixed_forked_pairs().contains(&pair));
        assert!(identities.equivalent(a, b).unwrap());
    }

    mod foreign {
        use triblespace::prelude::*;
        attributes! {
            /// A fact some other writer attaches to a tie; this reader does
            /// not model it. Test-only anchor minted with `trible genid` on
            /// 2026-11-16.
            "972362D0D07A446B49A959BEC4088DCE" as rating: inlineencodings::Handle<blobencodings::UTF8String>;
        }
    }

    #[test]
    fn a_tie_is_its_claim_and_annotations_accumulate_beside_it() {
        let fixture = Fixture::new();
        let ada = genid().id;
        let bank = genid().id;
        publish_person(&fixture, ada, "Ada");
        let (org, _, _) = organisation_fragment(bank, profile("Bremer Aufbau-Bank")).unwrap();
        fixture.publish(org);

        let core = tie_fragment(ada, " works at ", bank, Some("CEO".to_owned())).unwrap();
        let tie = core.root().unwrap();
        let again = tie_fragment(ada, "works at", bank, Some("CEO".to_owned())).unwrap();
        assert_eq!(again.root(), Some(tie), "trimmed text is the same claim");
        assert_ne!(
            tie_fragment(ada, "works at", bank, None).unwrap().root(),
            Some(tie)
        );
        assert_ne!(
            tie_fragment(bank, "works at", ada, Some("CEO".to_owned()))
                .unwrap()
                .root(),
            Some(tie),
            "direction is part of the claim"
        );
        assert!(tie_fragment(ada, "knows", ada, None).is_err());
        assert!(tie_fragment(ada, "  ", bank, None).is_err());

        fixture.publish(core);
        fixture.publish(again);
        fixture.publish(
            tie_annotation_fragment(
                tie,
                TieAnnotation {
                    sources: vec!["a".into()],
                    finished_at: Some(parse_when("2023").unwrap()),
                    ..TieAnnotation::default()
                },
            )
            .unwrap(),
        );
        // A concurrent second ending, a second role and an unmodelled fact
        // are all open-world additions: shown or ignored, never refused.
        fixture.publish(
            tie_annotation_fragment(
                tie,
                TieAnnotation {
                    sources: vec!["b".into()],
                    finished_at: Some(parse_when("2024").unwrap()),
                    ..TieAnnotation::default()
                },
            )
            .unwrap(),
        );
        let mut extra = Fragment::empty();
        let role = extra.put("Chair".to_owned());
        let rating = extra.put("five stars".to_owned());
        extra += entity! { ExclusiveId::force_ref(&tie) @
            tie::role: role,
            foreign::rating: rating,
        };
        fixture.publish(extra);

        let view = fixture.view();
        let claims = ties_touching(&view.facts, ada);
        assert_eq!(claims.len(), 1);
        assert_eq!(ties_touching(&view.facts, bank), claims);
        assert_eq!(claims[0].id, tie);
        assert_eq!((claims[0].subject, claims[0].object), (ada, bank));
        assert_eq!(
            read_text(&view.reader, claims[0].relation).unwrap(),
            "works at"
        );
        assert_eq!(tie_claims(&view.facts, tie), claims);
        let roles: Vec<String> = tie_roles(&view.facts, tie)
            .into_iter()
            .map(|role| read_text(&view.reader, role).unwrap())
            .collect();
        assert_eq!(sorted_strings(roles), vec!["CEO", "Chair"]);
        assert_eq!(
            sources_of(&view.reader, &view.facts, tie).unwrap(),
            vec!["a", "b"]
        );
        let ends: Vec<String> = finished_at(&view.facts, tie)
            .into_iter()
            .map(format_when)
            .collect();
        assert_eq!(ends, vec!["2023", "2024"]);
        assert_eq!(
            entry_label(&view.reader, &view.facts, bank),
            "Bremer Aufbau-Bank"
        );
        assert_eq!(
            entry_kinds(&view.facts, bank),
            vec![EntryKind::Organisation]
        );
        // Organisation snapshots stay out of the person reader and the
        // legacy person catalogue check.
        assert_eq!(person_anchors(&view.facts), BTreeSet::from([ada]));
        assert_eq!(profile_head(&view.facts, bank).unwrap(), Head::Missing);
        assert!(matches!(
            resolve_person(&view.reader, &view.facts, "Bremer Aufbau-Bank", true).unwrap(),
            SelectorOutcome::Missing
        ));
        assert_eq!(
            resolve_entry(
                &view.reader,
                &view.facts,
                "bremer aufbau-bank",
                false,
                &EntryKind::ALL
            )
            .unwrap(),
            SelectorOutcome::Unique(bank)
        );
        assert_eq!(
            resolve_tie(&view.facts, &format!("{tie:x}")[..6]),
            SelectorOutcome::Unique(tie)
        );
    }

    #[test]
    fn a_label_shared_by_a_person_and_an_organisation_is_ambiguous() {
        let fixture = Fixture::new();
        let person = genid().id;
        let organisation = genid().id;
        publish_person(&fixture, person, "Mercury");
        fixture.publish(
            organisation_fragment(organisation, profile("Mercury"))
                .unwrap()
                .0,
        );
        let view = fixture.view();
        assert!(matches!(
            resolve_entry(&view.reader, &view.facts, "mercury", false, &EntryKind::ALL).unwrap(),
            SelectorOutcome::Ambiguous(ids) if ids.len() == 2
        ));
        assert_eq!(
            resolve_organisation(&view.reader, &view.facts, "mercury", false).unwrap(),
            SelectorOutcome::Unique(organisation)
        );
    }

    #[test]
    fn calendar_values_keep_their_granularity() {
        for (raw, shown) in [
            ("2019", "2019"),
            ("2019-03", "2019-03"),
            ("2019-12", "2019-12"),
            ("2024-02-29", "2024-02-29"),
            // A leap-second day still reads back as one whole day.
            ("2016-12-31", "2016-12-31"),
            ("2019-03-01T12:30", "2019-03-01T12:30Z"),
        ] {
            assert_eq!(format_when(parse_when(raw).unwrap()), shown, "{raw}");
        }
        let (from, to): (hifitime::Epoch, hifitime::Epoch) = parse_when("2019..2020-06")
            .unwrap()
            .try_from_inline()
            .unwrap();
        assert_eq!(
            from,
            hifitime::Epoch::from_gregorian_utc_at_midnight(2019, 1, 1)
        );
        assert_eq!(
            to + hifitime::Duration::from_truncated_nanoseconds(1),
            hifitime::Epoch::from_gregorian_utc_at_midnight(2020, 7, 1)
        );
        for bad in ["", "19x", "2019-13", "2019-02-30", "2020..2019"] {
            assert!(parse_when(bad).is_err(), "{bad}");
        }
    }
}
