//! `relations` — authored people, addressable groups, and explicit identity
//! adjudication in one union-only native collection.
//!
//! Stable person/group anchors never accumulate mutable scalar facts. Every
//! change publishes one intrinsic full-state snapshot with explicit
//! predecessors. Concurrent publications therefore become visible forks;
//! reconciliation is another monotonic child, never deletion, a mutable head,
//! or clock-based arbitration.

#[cfg(test)]
use crate::storage::FactView;
use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use crate::clock;
#[cfg(test)]
use crate::collection_names::open_configured;
use crate::relations::{
    self, EntryKind, GroupSnapshot, Head, IdentityComponents, ProfileInput, ProfileSnapshot,
    SelectorOutcome, TieAnnotation,
};
use crate::schemas::relations::DEFAULT_SCOPE_ID;
use crate::storage::{FactArchive, FacultyStore, Storage};
use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{Collection, CollectionSnapshotExt, CollectionStoreExt};
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::*;

type RelationsReader = crate::storage::AcquiringReader<<FacultyStore as SnapshotSource>::Snapshot>;

struct RelationsStorage<'a> {
    pile: &'a mut FacultyStore,
    signer: &'a SigningKey,
    collection: Collection<SimpleArchive>,
    facts: &'a FactArchive,
    reader: &'a RelationsReader,
}

impl RelationsStorage<'_> {
    fn with_view<T>(
        &self,
        f: impl FnOnce(&FactArchive, &RelationsReader) -> Result<T>,
    ) -> Result<T> {
        f(self.facts, self.reader)
    }

    /// Build one typed local update. `None` is a genuine no-op and writes no
    /// collection record.
    fn update<T>(
        &mut self,
        f: impl FnOnce(&FactArchive, &RelationsReader) -> Result<(Option<Fragment>, T)>,
    ) -> Result<T> {
        let (fragment, result) = f(self.facts, self.reader)?;
        if let Some(fragment) = fragment {
            self.pile
                .commit(self.collection, self.signer, fragment)
                .context("commit authored Relations fragment")?;
            drop(
                pollster::block_on(crate::storage::ensure_downstream(
                    self.pile,
                    self.collection,
                    self.signer,
                ))
                .context(
                    "Relations facts were committed, but ensuring their derived views failed",
                )?,
            );
        }
        Ok(result)
    }
}

fn fmt_id(id: Id) -> String {
    format!("{id:x}")
}

fn now_observation() -> Result<relations::ObservedAt> {
    clock::point_now()
}

fn resolve_person_anchor(
    reader: &RelationsReader,
    facts: &FactArchive,
    selector: &str,
    include_retired: bool,
) -> Result<Id> {
    match relations::resolve_person(reader, facts, selector, include_retired)? {
        SelectorOutcome::Unique(id) => Ok(id),
        // Reconciliation operations may deliberately address the one stable
        // anchor whose profile/lifecycle happens to have several heads — but
        // only when it is the sole claimant, so no settled match is displaced.
        SelectorOutcome::Forked {
            ref forked,
            ref settled,
        } if forked.len() == 1 && settled.is_empty() => Ok(forked[0]),
        outcome => outcome.require_unique("person", selector),
    }
}

/// Resolve a selector across the profile-carrying `kinds`, with the same
/// single-forked-claimant allowance as [`resolve_person_anchor`].
fn resolve_entry_anchor(
    reader: &RelationsReader,
    facts: &FactArchive,
    selector: &str,
    include_retired: bool,
    kinds: &[EntryKind],
) -> Result<(Id, EntryKind)> {
    let noun = match kinds {
        [kind] => kind.noun(),
        _ => "entry",
    };
    let id = match relations::resolve_entry(reader, facts, selector, include_retired, kinds)? {
        SelectorOutcome::Unique(id) => id,
        SelectorOutcome::Forked {
            ref forked,
            ref settled,
        } if forked.len() == 1 && settled.is_empty() => forked[0],
        outcome => outcome.require_unique(noun, selector)?,
    };
    let kind = relations::entry_kinds(facts, id)
        .into_iter()
        .find(|kind| kinds.contains(kind))
        .expect("a resolved entry carries one of the requested kinds");
    Ok((id, kind))
}

/// Resolve one end of a tie. A live entry wins; only when no live entry
/// matches may the selector name a retired one, because a tie is an
/// observation about history and the people and organisations in history
/// include retired ones. The first error is kept so a selector that matches
/// nothing still reports the live-only reason.
fn resolve_tie_end(reader: &RelationsReader, facts: &FactArchive, selector: &str) -> Result<Id> {
    match resolve_entry_anchor(reader, facts, selector, false, &EntryKind::ALL) {
        Ok((id, _)) => Ok(id),
        Err(live) => resolve_entry_anchor(reader, facts, selector, true, &EntryKind::ALL)
            .map(|(id, _)| id)
            .map_err(|_| live),
    }
}

fn resolve_group_anchor(
    reader: &RelationsReader,
    facts: &FactArchive,
    selector: &str,
) -> Result<Id> {
    match relations::resolve_group(reader, facts, selector)? {
        SelectorOutcome::Unique(id) => Ok(id),
        SelectorOutcome::Forked {
            ref forked,
            ref settled,
        } if forked.len() == 1 && settled.is_empty() => Ok(forked[0]),
        outcome => outcome.require_unique("group", selector),
    }
}

fn head_ids(head: Head, subject: &str) -> Result<Vec<Id>> {
    match head {
        Head::Missing => bail!("{subject} has no snapshot"),
        Head::Unique(id) => Ok(vec![id]),
        Head::Forked(ids) => Ok(ids),
    }
}

fn resolve_head_selector(raw: &str, heads: &[Id], label: &str) -> Result<Id> {
    let raw = raw.trim().to_ascii_lowercase();
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) || raw.len() > 32 {
        bail!("invalid {label} head selector '{raw}'");
    }
    let matches: Vec<Id> = heads
        .iter()
        .copied()
        .filter(|id| format!("{id:x}").starts_with(&raw))
        .collect();
    match matches.as_slice() {
        [] => bail!("'{raw}' is not a current {label} head"),
        [id] => Ok(*id),
        _ => bail!("'{raw}' matches multiple current {label} heads"),
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProfileField {
    Aliases,
    Affinities,
    FirstName,
    LastName,
    DisplayName,
    Note,
    TeamsUserIds,
    Emails,
    Phones,
    Company,
    Position,
    ProfileUrls,
}

/// Fields absent from a patch remain untouched. A present repeated field
/// replaces the whole set, including an explicitly empty set. Clear applies
/// only to optional/repeated fields and conflicts with a simultaneous replacement.
#[derive(Clone, Debug, Default)]
pub struct ProfilePatch {
    pub label: Option<String>,
    pub aliases: Option<Vec<String>>,
    pub affinities: Option<Vec<String>>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub display_name: Option<String>,
    pub note: Option<String>,
    pub teams_user_ids: Option<Vec<String>>,
    pub emails: Option<Vec<String>>,
    pub phones: Option<Vec<String>>,
    pub company: Option<String>,
    pub position: Option<String>,
    pub profile_urls: Option<Vec<String>>,
    pub clear: Vec<ProfileField>,
}

fn replacement_conflict(
    clears: &HashSet<ProfileField>,
    field: ProfileField,
    replacement_present: bool,
) -> Result<()> {
    if clears.contains(&field) && replacement_present {
        bail!("clearing {field:?} conflicts with its replacement");
    }
    Ok(())
}

fn apply_profile_patch(input: &mut ProfileInput, patch: ProfilePatch) -> Result<bool> {
    let before = input.clone();
    let clears: HashSet<ProfileField> = patch.clear.into_iter().collect();
    if let Some(label) = patch.label {
        input.label = label;
    }
    macro_rules! scalar {
        ($field:ident, $variant:ident) => {
            replacement_conflict(&clears, ProfileField::$variant, patch.$field.is_some())?;
            if clears.contains(&ProfileField::$variant) {
                input.$field = None;
            } else if let Some(value) = patch.$field {
                input.$field = Some(value);
            }
        };
    }
    macro_rules! repeated {
        ($field:ident, $variant:ident) => {
            replacement_conflict(&clears, ProfileField::$variant, patch.$field.is_some())?;
            if clears.contains(&ProfileField::$variant) {
                input.$field.clear();
            } else if let Some(value) = patch.$field {
                input.$field = value;
            }
        };
    }
    repeated!(aliases, Aliases);
    repeated!(affinities, Affinities);
    scalar!(first_name, FirstName);
    scalar!(last_name, LastName);
    scalar!(display_name, DisplayName);
    scalar!(note, Note);
    repeated!(teams_user_ids, TeamsUserIds);
    repeated!(emails, Emails);
    repeated!(phones, Phones);
    scalar!(company, Company);
    scalar!(position, Position);
    repeated!(profile_urls, ProfileUrls);
    Ok(*input != before)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PeopleFilter {
    #[default]
    Active,
    All,
    Retired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddedPerson {
    pub person: Id,
    pub profile: Id,
    pub lifecycle: Id,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileUpdate {
    pub person: Id,
    pub previous: Id,
    pub current: Option<Id>,
    pub provenance_added: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddedGroup {
    pub group: Id,
    pub snapshot: Id,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileReconciliation {
    Settled(Id),
    Reconciled { heads: usize, successor: Id },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleChange {
    Unchanged(Id),
    Changed { person: Id, successor: Id },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupAddition {
    Already(Id),
    Changed { old: Id, new: Id },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupRemoval {
    Absent(Id),
    Changed { old: Id, new: Id },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupRename {
    Unchanged(String),
    Changed { old: Id, new: Id },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupReconciliation {
    Settled(Id),
    Reconciled { heads: usize, successor: Id },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdentityChange {
    Settled(Id),
    Changed {
        first: Id,
        second: Id,
        successor: Id,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddedOrganisation {
    pub organisation: Id,
    pub profile: Id,
    pub lifecycle: Id,
}

/// One tie publication: the tie id, its endpoints, and whether the view
/// already held that exact claim (then only annotations were added).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddedTie {
    pub tie: Id,
    pub subject: Id,
    pub object: Id,
    pub known: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TieRequest {
    /// Subject entry: person or organisation label, alias, id, or id prefix.
    pub subject: String,
    /// Relation label, read from subject to object ("works at").
    pub relation: String,
    /// Object entry, resolved like `subject`.
    pub object: String,
    pub role: Option<String>,
    /// Validity start, as [`relations::parse_when`] reads it.
    pub since: Option<String>,
    /// Validity end, as [`relations::parse_when`] reads it.
    pub until: Option<String>,
    /// Provenance: URLs or notes (repeatable, accumulating).
    pub sources: Vec<String>,
    /// Optional active Relations person for cooperative attribution.
    pub persona: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndedTie {
    pub tie: Id,
    pub until: String,
}

fn resolve_author(
    reader: &RelationsReader,
    facts: &FactArchive,
    persona: Option<&str>,
) -> Result<Option<Id>> {
    persona
        .map(|persona| {
            relations::resolve_person(reader, facts, persona, false)?
                .require_unique("persona", persona)
        })
        .transpose()
}

fn add_organisation(
    storage: &mut RelationsStorage<'_>,
    profile: ProfileInput,
    source: Vec<String>,
) -> Result<AddedOrganisation> {
    storage.update(|facts, reader| {
        // An organisation anchor is extrinsic, so re-running an import would
        // mint a duplicate; refuse a label or alias that already names one.
        for key in std::iter::once(&profile.label).chain(profile.aliases.iter()) {
            match relations::resolve_organisation(reader, facts, key, true)? {
                SelectorOutcome::Missing | SelectorOutcome::Invalid(_) => {}
                outcome => bail!(
                    "organisation '{key}' already exists: {} (use `relations org set`)",
                    outcome
                        .candidates()
                        .into_iter()
                        .map(fmt_id)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
        let organisation = genid().id;
        let (mut fragment, profile_id, lifecycle_id) =
            relations::organisation_fragment(organisation, profile)?;
        fragment +=
            relations::person_provenance_fragment(organisation, source, &[now_observation()?])?;
        Ok((
            Some(fragment),
            AddedOrganisation {
                organisation,
                profile: profile_id,
                lifecycle: lifecycle_id,
            },
        ))
    })
}

fn add_tie(storage: &mut RelationsStorage<'_>, request: TieRequest) -> Result<AddedTie> {
    let since = request
        .since
        .as_deref()
        .map(relations::parse_when)
        .transpose()?;
    let until = request
        .until
        .as_deref()
        .map(relations::parse_when)
        .transpose()?;
    let now = now_observation()?;
    storage.update(|facts, reader| {
        let subject = resolve_tie_end(reader, facts, &request.subject)?;
        let object = resolve_tie_end(reader, facts, &request.object)?;
        let author = resolve_author(reader, facts, request.persona.as_deref())?;
        let mut fragment =
            relations::tie_fragment(subject, request.relation, object, request.role)?;
        let tie = fragment
            .root()
            .expect("tie fragment has one intrinsic root");
        // Reported, never decisive: the same claim converges on this id.
        let known = !relations::tie_claims(facts, tie).is_empty();
        fragment += relations::tie_annotation_fragment(
            tie,
            TieAnnotation {
                sources: request.sources,
                started_at: since,
                finished_at: until,
                created_at: Some(now),
                author,
            },
        )?;
        Ok((
            Some(fragment),
            AddedTie {
                tie,
                subject,
                object,
                known,
            },
        ))
    })
}

fn end_tie(
    storage: &mut RelationsStorage<'_>,
    selector: String,
    until: String,
    sources: Vec<String>,
    persona: Option<String>,
) -> Result<EndedTie> {
    let finished = relations::parse_when(&until)?;
    storage.update(|facts, reader| {
        let tie = relations::resolve_tie(facts, &selector).require_unique("tie", &selector)?;
        let author = resolve_author(reader, facts, persona.as_deref())?;
        let fragment = relations::tie_annotation_fragment(
            tie,
            TieAnnotation {
                sources,
                finished_at: Some(finished),
                author,
                ..TieAnnotation::default()
            },
        )?;
        Ok((
            Some(fragment),
            EndedTie {
                tie,
                until: relations::format_when(finished),
            },
        ))
    })
}

fn print_ties(
    mut output: &mut String,
    reader: &RelationsReader,
    facts: &FactArchive,
    anchor: Id,
) -> Result<()> {
    let claims = relations::ties_touching(facts, anchor);
    if claims.is_empty() {
        return Ok(());
    }
    writeln!(&mut output, "ties: {}", claims.len())?;
    for claim in claims {
        writeln!(
            &mut output,
            "- {}: {} ({}) --{}--> {} ({})",
            fmt_id(claim.id),
            relations::entry_label(reader, facts, claim.subject),
            fmt_id(claim.subject),
            relations::read_text(reader, claim.relation)?,
            relations::entry_label(reader, facts, claim.object),
            fmt_id(claim.object),
        )?;
        for role in relations::tie_roles(facts, claim.id) {
            writeln!(
                &mut output,
                "  role: {}",
                relations::read_text(reader, role)?
            )?;
        }
        for since in relations::started_at(facts, claim.id) {
            writeln!(&mut output, "  since: {}", relations::format_when(since))?;
        }
        for until in relations::finished_at(facts, claim.id) {
            writeln!(&mut output, "  until: {}", relations::format_when(until))?;
        }
        for source in relations::sources_of(reader, facts, claim.id)? {
            writeln!(&mut output, "  source: {source}")?;
        }
        for author in relations::tie_authors(facts, claim.id) {
            writeln!(
                &mut output,
                "  by: {}",
                relations::entry_label(reader, facts, author)
            )?;
        }
    }
    Ok(())
}

fn list_ties(storage: &mut RelationsStorage<'_>, selector: String) -> Result<String> {
    let mut output = String::new();
    storage.with_view(|facts, reader| {
        let (anchor, kind) = resolve_entry_anchor(reader, facts, &selector, true, &EntryKind::ALL)?;
        writeln!(
            &mut output,
            "{}: {} {}",
            kind.noun(),
            fmt_id(anchor),
            relations::entry_label(reader, facts, anchor)
        )?;
        print_ties(&mut output, reader, facts, anchor)?;
        if relations::ties_touching(facts, anchor).is_empty() {
            writeln!(&mut output, "No ties.")?;
        }
        Ok(output)
    })
}

fn add_person(
    storage: &mut RelationsStorage<'_>,
    profile: ProfileInput,
    id: Option<Id>,
    source: Vec<String>,
) -> Result<AddedPerson> {
    let person = id.unwrap_or_else(|| genid().id);
    let (mut fragment, profile_id, lifecycle_id) = relations::person_fragment(person, profile)?;
    fragment += relations::person_provenance_fragment(person, source, &[now_observation()?])?;
    storage.update(|_, _| {
        Ok((
            Some(fragment),
            AddedPerson {
                person,
                profile: profile_id,
                lifecycle: lifecycle_id,
            },
        ))
    })
}

fn set_profile(
    storage: &mut RelationsStorage<'_>,
    kind: EntryKind,
    person: String,
    source: Vec<String>,
    patch: ProfilePatch,
) -> Result<ProfileUpdate> {
    storage.update(|facts, reader| {
        let person = match kind {
            EntryKind::Person => resolve_person_anchor(reader, facts, &person, true)?,
            _ => resolve_entry_anchor(reader, facts, &person, true, &[kind])?.0,
        };
        let current = relations::current_profile_of(facts, person, kind)?;
        let mut value = relations::profile_input(reader, &current)?;
        let changed = apply_profile_patch(&mut value, patch)?;

        let mut fragment = Fragment::empty();
        let new = if changed {
            let profile = relations::profile_fragment_of(kind, person, value, &[current.id])?;
            let id = profile.root().expect("profile snapshot root");
            fragment += profile;
            Some(id)
        } else {
            None
        };
        let provenance_added = !source.is_empty();
        if provenance_added {
            fragment += relations::person_provenance_fragment(person, source, &[])?;
        }
        let publish = (!fragment.facts().is_empty()).then_some(fragment);
        Ok((
            publish,
            ProfileUpdate {
                person,
                previous: current.id,
                current: new,
                provenance_added,
            },
        ))
    })
}

fn reconcile_profile(
    storage: &mut RelationsStorage<'_>,
    person_selector: String,
    base: Option<String>,
    patch: ProfilePatch,
) -> Result<ProfileReconciliation> {
    storage.update(|facts, reader| {
        let person = resolve_person_anchor(reader, facts, &person_selector, true)?;
        let heads = head_ids(relations::profile_head(facts, person)?, "person profile")?;
        let snapshots: Vec<(ProfileSnapshot, ProfileInput)> = heads
            .iter()
            .map(|&id| {
                let snapshot = relations::profile_snapshot(facts, id)?;
                let input = relations::profile_input(reader, &snapshot)?;
                Ok((snapshot, input))
            })
            .collect::<Result<_>>()?;
        let base_id = if let Some(base) = base {
            resolve_head_selector(&base, &heads, "profile")?
        } else {
            let first = &snapshots[0].1;
            if snapshots.iter().skip(1).any(|(_, value)| value != first) {
                bail!("profile heads disagree; choose the intended value with --base <head>");
            }
            snapshots[0].0.id
        };
        let mut value = snapshots
            .iter()
            .find(|(snapshot, _)| snapshot.id == base_id)
            .map(|(_, value)| value.clone())
            .expect("selected current head");
        let changed = apply_profile_patch(&mut value, patch)?;
        if heads.len() == 1 && !changed {
            return Ok((None, ProfileReconciliation::Settled(base_id)));
        }
        let fragment = relations::profile_fragment(person, value, &heads)?;
        let successor = fragment.root().expect("profile snapshot root");
        Ok((
            Some(fragment),
            ProfileReconciliation::Reconciled {
                heads: heads.len(),
                successor,
            },
        ))
    })
}

fn lifecycle_state(
    facts: &FactArchive,
    person: Id,
    kind: EntryKind,
) -> Result<(Vec<Id>, Option<bool>)> {
    match relations::lifecycle_head_of(facts, person, kind)? {
        Head::Missing => bail!("{} {} has no lifecycle", kind.noun(), fmt_id(person)),
        Head::Unique(id) => Ok((
            vec![id],
            Some(relations::lifecycle_snapshot_of(facts, id, kind)?.retired),
        )),
        Head::Forked(ids) => Ok((ids, None)),
    }
}

fn set_retired(
    storage: &mut RelationsStorage<'_>,
    kind: EntryKind,
    selector: String,
    retired: bool,
) -> Result<LifecycleChange> {
    storage.update(|facts, reader| {
        let person = match kind {
            EntryKind::Person => resolve_person_anchor(reader, facts, &selector, true)?,
            _ => resolve_entry_anchor(reader, facts, &selector, true, &[kind])?.0,
        };
        let (heads, current) = lifecycle_state(facts, person, kind)?;
        if current == Some(retired) {
            return Ok((None, LifecycleChange::Unchanged(person)));
        }
        let fragment = relations::lifecycle_fragment_of(kind, person, retired, &heads);
        let successor = fragment.root().expect("lifecycle snapshot root");
        Ok((
            Some(fragment),
            LifecycleChange::Changed { person, successor },
        ))
    })
}

fn print_values(mut output: &mut String, label: &str, values: &[String]) -> Result<()> {
    for value in values {
        writeln!(&mut output, "{label}: {value}")?;
    }
    Ok(())
}

fn print_profile(mut output: &mut String, id: Id, input: &ProfileInput) -> Result<()> {
    writeln!(&mut output, "profile: {}", fmt_id(id))?;
    writeln!(&mut output, "label: {}", input.label)?;
    print_values(&mut output, "alias", &input.aliases)?;
    print_values(&mut output, "affinity", &input.affinities)?;
    if let Some(value) = &input.first_name {
        writeln!(&mut output, "first_name: {value}")?;
    }
    if let Some(value) = &input.last_name {
        writeln!(&mut output, "last_name: {value}")?;
    }
    if let Some(value) = &input.display_name {
        writeln!(&mut output, "display_name: {value}")?;
    }
    if let Some(value) = &input.company {
        writeln!(&mut output, "company: {value}")?;
    }
    if let Some(value) = &input.position {
        writeln!(&mut output, "position: {value}")?;
    }
    print_values(&mut output, "teams_user_id", &input.teams_user_ids)?;
    print_values(&mut output, "email", &input.emails)?;
    print_values(&mut output, "phone", &input.phones)?;
    print_values(&mut output, "profile_url", &input.profile_urls)?;
    if let Some(value) = &input.note {
        writeln!(&mut output, "note:\n{value}")?;
    }
    Ok(())
}

/// Show one person or organisation, with its ties: `kinds` are the entry
/// kinds the selector may name.
fn show_entry(
    storage: &mut RelationsStorage<'_>,
    selector: String,
    kinds: &[EntryKind],
) -> Result<String> {
    let mut output = String::new();
    storage.with_view(|facts, reader| {
        let (person, kind) = resolve_entry_anchor(reader, facts, &selector, true, kinds)?;
        writeln!(&mut output, "{}: {}", kind.noun(), fmt_id(person))?;
        print_values(
            &mut output,
            "source",
            &relations::person_sources(reader, facts, person)?,
        )?;
        let observations = relations::creation_observations(facts, person);
        if !observations.is_empty() {
            writeln!(&mut output, "creation_observations: {}", observations.len())?;
        }
        match relations::profile_head_of(facts, person, kind)? {
            Head::Missing => writeln!(&mut output, "profile: missing")?,
            Head::Unique(id) => {
                let snapshot = relations::profile_snapshot_of(facts, id, kind)?;
                print_profile(
                    &mut output,
                    id,
                    &relations::profile_input(reader, &snapshot)?,
                )?;
            }
            Head::Forked(ids) => {
                writeln!(&mut output, "profile_fork: {} heads", ids.len())?;
                for id in ids {
                    let snapshot = relations::profile_snapshot_of(facts, id, kind)?;
                    print_profile(
                        &mut output,
                        id,
                        &relations::profile_input(reader, &snapshot)?,
                    )?;
                }
            }
        }
        match relations::lifecycle_head_of(facts, person, kind)? {
            Head::Missing => writeln!(&mut output, "lifecycle: missing")?,
            Head::Unique(id) => writeln!(
                &mut output,
                "retired: {}\nlifecycle: {}",
                relations::lifecycle_snapshot_of(facts, id, kind)?.retired,
                fmt_id(id)
            )?,
            Head::Forked(ids) => {
                writeln!(&mut output, "lifecycle_fork: {} heads", ids.len())?;
                for id in ids {
                    writeln!(
                        &mut output,
                        "- {} retired={}",
                        fmt_id(id),
                        relations::lifecycle_snapshot_of(facts, id, kind)?.retired
                    )?;
                }
            }
        }
        print_ties(&mut output, reader, facts, person)?;
        Ok(output)
    })
}

fn list_people(
    storage: &mut RelationsStorage<'_>,
    limit: usize,
    all: bool,
    retired_only: bool,
) -> Result<String> {
    list_entries(storage, EntryKind::Person, limit, all, retired_only)
}

fn list_entries(
    storage: &mut RelationsStorage<'_>,
    kind: EntryKind,
    limit: usize,
    all: bool,
    retired_only: bool,
) -> Result<String> {
    let mut output = String::new();
    storage.with_view(|facts, reader| {
        let mut rows = Vec::new();
        for person in relations::entry_anchors(facts, kind) {
            let lifecycle = relations::lifecycle_head_of(facts, person, kind)?;
            let retired = match &lifecycle {
                Head::Unique(id) => {
                    Some(relations::lifecycle_snapshot_of(facts, *id, kind)?.retired)
                }
                Head::Missing | Head::Forked(_) => None,
            };
            if retired_only && retired != Some(true) {
                continue;
            }
            if !all && !retired_only && retired == Some(true) {
                continue;
            }
            let profile = relations::profile_head_of(facts, person, kind)?;
            let (label, marker) = match profile {
                Head::Unique(id) => {
                    let snapshot = relations::profile_snapshot_of(facts, id, kind)?;
                    (relations::read_text(reader, snapshot.label)?, String::new())
                }
                Head::Forked(ids) => (
                    "<forked profile>".to_owned(),
                    format!(" [profile fork: {} heads]", ids.len()),
                ),
                Head::Missing => ("<missing profile>".to_owned(), " [invalid]".to_owned()),
            };
            let lifecycle_marker = match lifecycle {
                Head::Forked(ids) => format!(" [lifecycle fork: {} heads]", ids.len()),
                _ if retired == Some(true) => " [retired]".to_owned(),
                _ => String::new(),
            };
            rows.push((
                relations::lookup_key(&label),
                person,
                label,
                marker,
                lifecycle_marker,
            ));
        }
        rows.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        if rows.is_empty() {
            match kind {
                EntryKind::Person => writeln!(&mut output, "No people.")?,
                EntryKind::Organisation => writeln!(&mut output, "No organisations.")?,
            }
        }
        for (_, person, label, marker, lifecycle) in rows.into_iter().take(limit) {
            writeln!(
                &mut output,
                "[{}] {label}{marker}{lifecycle}",
                fmt_id(person)
            )?;
        }
        Ok(output)
    })
}

fn create_group(storage: &mut RelationsStorage<'_>, name: String) -> Result<AddedGroup> {
    storage.update(|facts, reader| {
        match relations::resolve_group(reader, facts, &name)? {
            SelectorOutcome::Missing => {}
            outcome => {
                let existing = outcome.require_unique("group", &name)?;
                bail!("group '{}' already resolves to {}", name, fmt_id(existing));
            }
        }
        let group = genid().id;
        let (mut fragment, snapshot) = relations::group_create_fragment(group, name)?;
        fragment += relations::group_provenance_fragment(group, &[now_observation()?]);
        Ok((Some(fragment), AddedGroup { group, snapshot }))
    })
}

fn add_group_member(
    storage: &mut RelationsStorage<'_>,
    group_selector: String,
    person_selector: String,
) -> Result<GroupAddition> {
    storage.update(|facts, reader| {
        let group = resolve_group_anchor(reader, facts, &group_selector)?;
        let person = resolve_person_anchor(reader, facts, &person_selector, true)?;
        let current = relations::current_group(facts, group)?;
        let identities = IdentityComponents::from_facts(facts)?;
        for &member in &current.members {
            if identities.equivalent(person, member)? {
                return Ok((None, GroupAddition::Already(person)));
            }
        }
        let mut members = current.members.clone();
        members.push(person);
        let name = relations::read_text(reader, current.name)?;
        let old = current.id;
        let fragment = relations::group_snapshot_fragment(group, name, &members, &[old])?;
        let new = fragment.root().expect("group snapshot root");
        Ok((Some(fragment), GroupAddition::Changed { old, new }))
    })
}

fn remove_group_member(
    storage: &mut RelationsStorage<'_>,
    group_selector: String,
    person_selector: String,
) -> Result<GroupRemoval> {
    storage.update(|facts, reader| {
        let group = resolve_group_anchor(reader, facts, &group_selector)?;
        let person = resolve_person_anchor(reader, facts, &person_selector, true)?;
        let current = relations::current_group(facts, group)?;
        let identities = IdentityComponents::from_facts(facts)?;
        let mut members = Vec::new();
        for member in current.members.iter().copied() {
            if !identities.equivalent(person, member)? {
                members.push(member);
            }
        }
        if members.len() == current.members.len() {
            return Ok((None, GroupRemoval::Absent(person)));
        }
        let name = relations::read_text(reader, current.name)?;
        let old = current.id;
        let fragment = relations::group_snapshot_fragment(group, name, &members, &[old])?;
        let new = fragment.root().expect("group snapshot root");
        Ok((Some(fragment), GroupRemoval::Changed { old, new }))
    })
}

fn rename_group(
    storage: &mut RelationsStorage<'_>,
    group_selector: String,
    name: String,
) -> Result<GroupRename> {
    storage.update(|facts, reader| {
        let group = resolve_group_anchor(reader, facts, &group_selector)?;
        let current = relations::current_group(facts, group)?;
        let old_name = relations::read_text(reader, current.name)?;
        if old_name.trim() == name.trim() {
            return Ok((None, GroupRename::Unchanged(old_name)));
        }
        let old = current.id;
        let fragment = relations::group_snapshot_fragment(group, name, &current.members, &[old])?;
        let new = fragment.root().expect("group snapshot root");
        Ok((Some(fragment), GroupRename::Changed { old, new }))
    })
}

fn reconcile_group(
    storage: &mut RelationsStorage<'_>,
    selector: String,
    explicit_name: Option<String>,
) -> Result<GroupReconciliation> {
    storage.update(|facts, reader| {
        let group = resolve_group_anchor(reader, facts, &selector)?;
        let heads = head_ids(relations::group_head(facts, group)?, "group")?;
        if heads.len() == 1 && explicit_name.is_none() {
            return Ok((None, GroupReconciliation::Settled(heads[0])));
        }
        if heads.len() == 1 {
            bail!("group has one head; use `relations group rename` to change its name");
        }
        let snapshots: Vec<GroupSnapshot> = heads
            .iter()
            .map(|&id| relations::group_snapshot(facts, id))
            .collect::<Result<_>>()?;
        let name = if let Some(name) = explicit_name {
            name
        } else {
            let names: BTreeSet<String> = snapshots
                .iter()
                .map(|snapshot| relations::read_text(reader, snapshot.name))
                .collect::<Result<_>>()?;
            if names.len() != 1 {
                bail!("group heads disagree on the name; provide --name");
            }
            names.into_iter().next().expect("one name")
        };
        // The core helper is the single authority for the multi-parent join:
        // every immediate predecessor member is retained.
        let fragment = relations::reconcile_group_fragment(facts, group, name, &heads)?;
        let successor = fragment.root().expect("group snapshot root");
        Ok((
            Some(fragment),
            GroupReconciliation::Reconciled {
                heads: heads.len(),
                successor,
            },
        ))
    })
}

fn print_group_snapshot(
    mut output: &mut String,
    reader: &RelationsReader,
    facts: &FactArchive,
    snapshot: GroupSnapshot,
) -> Result<()> {
    writeln!(&mut output, "snapshot: {}", fmt_id(snapshot.id))?;
    writeln!(
        &mut output,
        "name: {}",
        relations::read_text(reader, snapshot.name)?
    )?;
    for member in snapshot.members {
        let label = relations::current_profile(facts, member)
            .and_then(|profile| relations::read_text(reader, profile.label))
            .unwrap_or_else(|_| "<unsettled profile>".to_owned());
        writeln!(&mut output, "member: {} {label}", fmt_id(member))?;
    }
    Ok(())
}

fn show_group(storage: &mut RelationsStorage<'_>, selector: String) -> Result<String> {
    let mut output = String::new();
    storage.with_view(|facts, reader| {
        let group = resolve_group_anchor(reader, facts, &selector)?;
        writeln!(&mut output, "group: {}", fmt_id(group))?;
        match relations::group_head(facts, group)? {
            Head::Missing => writeln!(&mut output, "snapshot: missing")?,
            Head::Unique(id) => print_group_snapshot(
                &mut output,
                reader,
                facts,
                relations::group_snapshot(facts, id)?,
            )?,
            Head::Forked(ids) => {
                writeln!(&mut output, "group_fork: {} heads", ids.len())?;
                for id in ids {
                    print_group_snapshot(
                        &mut output,
                        reader,
                        facts,
                        relations::group_snapshot(facts, id)?,
                    )?;
                }
            }
        }
        Ok(output)
    })
}

fn list_groups(storage: &mut RelationsStorage<'_>) -> Result<String> {
    let mut output = String::new();
    storage.with_view(|facts, reader| {
        let mut rows = Vec::new();
        for group in relations::group_anchors(facts) {
            match relations::group_head(facts, group)? {
                Head::Unique(id) => {
                    let snapshot = relations::group_snapshot(facts, id)?;
                    rows.push((
                        relations::read_text(reader, snapshot.name)?,
                        group,
                        format!("{} members", snapshot.members.len()),
                    ));
                }
                Head::Forked(ids) => rows.push((
                    "<forked group>".to_owned(),
                    group,
                    format!("fork: {} heads", ids.len()),
                )),
                Head::Missing => {
                    rows.push(("<missing group>".to_owned(), group, "invalid".to_owned()))
                }
            }
        }
        rows.sort_by(|left, right| {
            relations::lookup_key(&left.0)
                .cmp(&relations::lookup_key(&right.0))
                .then_with(|| left.1.cmp(&right.1))
        });
        if rows.is_empty() {
            writeln!(&mut output, "No groups.")?;
        }
        for (name, group, state) in rows {
            writeln!(&mut output, "[{}] {name} ({state})", fmt_id(group))?;
        }
        Ok(output)
    })
}

fn resolve_identity(
    storage: &mut RelationsStorage<'_>,
    first: String,
    second: String,
    same: bool,
) -> Result<IdentityChange> {
    storage.update(|facts, reader| {
        let first = resolve_person_anchor(reader, facts, &first, true)?;
        let second = resolve_person_anchor(reader, facts, &second, true)?;
        if first == second {
            bail!("an identity verdict requires two different person anchors");
        }
        let predecessors = match relations::identity_head(facts, first, second)? {
            Head::Missing => Vec::new(),
            Head::Unique(id) => {
                if relations::identity_verdict(facts, id)?.same == same {
                    return Ok((None, IdentityChange::Settled(id)));
                }
                vec![id]
            }
            Head::Forked(ids) => ids,
        };
        let fragment = relations::identity_verdict_fragment(first, second, same, &predecessors)?;
        let successor = fragment.root().expect("identity verdict root");
        Ok((
            Some(fragment),
            IdentityChange::Changed {
                first,
                second,
                successor,
            },
        ))
    })
}

fn list_identities(storage: &mut RelationsStorage<'_>) -> Result<String> {
    let mut output = String::new();
    storage.with_view(|facts, _| {
        let heads = relations::identity_heads(facts)?;
        if heads.is_empty() {
            writeln!(&mut output, "No identity verdicts.")?;
        }
        for ((low, high), head) in heads {
            match head {
                Head::Missing => unreachable!("listed pair has a verdict"),
                Head::Unique(id) => writeln!(
                    &mut output,
                    "{} {} {} [{}]",
                    fmt_id(low),
                    if relations::identity_verdict(facts, id)?.same {
                        "same-as"
                    } else {
                        "distinct-from"
                    },
                    fmt_id(high),
                    fmt_id(id)
                )?,
                Head::Forked(ids) => {
                    writeln!(
                        &mut output,
                        "{} ? {} [fork: {} heads]",
                        fmt_id(low),
                        fmt_id(high),
                        ids.len()
                    )?;
                    for id in ids {
                        writeln!(
                            &mut output,
                            "- {} {}",
                            fmt_id(id),
                            if relations::identity_verdict(facts, id)?.same {
                                "same-as"
                            } else {
                                "distinct-from"
                            }
                        )?;
                    }
                }
            }
        }
        Ok(output)
    })
}

/// Configured Relations operations. Each call maintains the exact configured
/// collection then observes one immutable fact/payload boundary.
#[derive(Clone, Debug)]
pub struct Relations {
    storage: Storage,
}

impl Relations {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    fn with_relations<T>(
        &self,
        read_only: bool,
        execute: impl FnOnce(&mut RelationsStorage<'_>) -> Result<T>,
    ) -> Result<T> {
        self.storage.with_store(|pile, signer, runtime| {
            let collection = self.storage.open_collection_read(
                pile,
                DEFAULT_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            if !read_only {
                self.storage.open_collection_write(
                    pile,
                    DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                    runtime,
                )?;
            }
            with_relations_view(pile, signer, runtime, collection, read_only, execute)
        })
    }

    pub fn add(
        &self,
        profile: ProfileInput,
        id: Option<Id>,
        sources: &[String],
    ) -> Result<AddedPerson> {
        self.with_relations(false, |storage| {
            add_person(storage, profile, id, sources.to_vec())
        })
    }
    pub fn set(
        &self,
        person: &str,
        patch: ProfilePatch,
        sources: &[String],
    ) -> Result<ProfileUpdate> {
        self.with_relations(false, |storage| {
            set_profile(
                storage,
                EntryKind::Person,
                person.to_owned(),
                sources.to_vec(),
                patch,
            )
        })
    }
    pub fn reconcile(
        &self,
        person: &str,
        base: Option<&str>,
        patch: ProfilePatch,
    ) -> Result<ProfileReconciliation> {
        self.with_relations(false, |storage| {
            reconcile_profile(storage, person.to_owned(), base.map(str::to_owned), patch)
        })
    }
    pub fn list(&self, limit: usize, filter: PeopleFilter) -> Result<String> {
        self.with_relations(true, |storage| {
            list_people(
                storage,
                limit,
                filter == PeopleFilter::All,
                filter == PeopleFilter::Retired,
            )
        })
    }
    /// Show a person or an organisation, with its ties.
    pub fn show(&self, entry: &str) -> Result<String> {
        self.with_relations(true, |storage| {
            show_entry(storage, entry.to_owned(), &EntryKind::ALL)
        })
    }
    pub fn retire(&self, person: &str) -> Result<LifecycleChange> {
        self.with_relations(false, |storage| {
            set_retired(storage, EntryKind::Person, person.to_owned(), true)
        })
    }
    pub fn unretire(&self, person: &str) -> Result<LifecycleChange> {
        self.with_relations(false, |storage| {
            set_retired(storage, EntryKind::Person, person.to_owned(), false)
        })
    }
    pub fn org_add(&self, profile: ProfileInput, sources: &[String]) -> Result<AddedOrganisation> {
        self.with_relations(false, |storage| {
            add_organisation(storage, profile, sources.to_vec())
        })
    }
    pub fn org_set(
        &self,
        organisation: &str,
        patch: ProfilePatch,
        sources: &[String],
    ) -> Result<ProfileUpdate> {
        self.with_relations(false, |storage| {
            set_profile(
                storage,
                EntryKind::Organisation,
                organisation.to_owned(),
                sources.to_vec(),
                patch,
            )
        })
    }
    pub fn org_list(&self, limit: usize, filter: PeopleFilter) -> Result<String> {
        self.with_relations(true, |storage| {
            list_entries(
                storage,
                EntryKind::Organisation,
                limit,
                filter == PeopleFilter::All,
                filter == PeopleFilter::Retired,
            )
        })
    }
    pub fn org_show(&self, organisation: &str) -> Result<String> {
        self.with_relations(true, |storage| {
            show_entry(storage, organisation.to_owned(), &[EntryKind::Organisation])
        })
    }
    pub fn org_retire(&self, organisation: &str) -> Result<LifecycleChange> {
        self.with_relations(false, |storage| {
            set_retired(
                storage,
                EntryKind::Organisation,
                organisation.to_owned(),
                true,
            )
        })
    }
    pub fn org_unretire(&self, organisation: &str) -> Result<LifecycleChange> {
        self.with_relations(false, |storage| {
            set_retired(
                storage,
                EntryKind::Organisation,
                organisation.to_owned(),
                false,
            )
        })
    }
    pub fn tie_add(&self, request: TieRequest) -> Result<AddedTie> {
        self.with_relations(false, |storage| add_tie(storage, request))
    }
    pub fn tie_end(
        &self,
        tie: &str,
        until: &str,
        sources: &[String],
        persona: Option<&str>,
    ) -> Result<EndedTie> {
        self.with_relations(false, |storage| {
            end_tie(
                storage,
                tie.to_owned(),
                until.to_owned(),
                sources.to_vec(),
                persona.map(str::to_owned),
            )
        })
    }
    /// Every tie with `entry` as subject or object.
    pub fn ties(&self, entry: &str) -> Result<String> {
        self.with_relations(true, |storage| list_ties(storage, entry.to_owned()))
    }
    pub fn group_create(&self, name: &str) -> Result<AddedGroup> {
        self.with_relations(false, |storage| create_group(storage, name.to_owned()))
    }
    pub fn group_add(&self, group: &str, person: &str) -> Result<GroupAddition> {
        self.with_relations(false, |storage| {
            add_group_member(storage, group.to_owned(), person.to_owned())
        })
    }
    pub fn group_remove(&self, group: &str, person: &str) -> Result<GroupRemoval> {
        self.with_relations(false, |storage| {
            remove_group_member(storage, group.to_owned(), person.to_owned())
        })
    }
    pub fn group_rename(&self, group: &str, name: &str) -> Result<GroupRename> {
        self.with_relations(false, |storage| {
            rename_group(storage, group.to_owned(), name.to_owned())
        })
    }
    pub fn group_reconcile(&self, group: &str, name: Option<&str>) -> Result<GroupReconciliation> {
        self.with_relations(false, |storage| {
            reconcile_group(storage, group.to_owned(), name.map(str::to_owned))
        })
    }
    pub fn group_list(&self) -> Result<String> {
        self.with_relations(true, list_groups)
    }
    pub fn group_show(&self, group: &str) -> Result<String> {
        self.with_relations(true, |storage| show_group(storage, group.to_owned()))
    }
    pub fn identity_resolve(
        &self,
        first: &str,
        second: &str,
        same: bool,
    ) -> Result<IdentityChange> {
        self.with_relations(false, |storage| {
            resolve_identity(storage, first.to_owned(), second.to_owned(), same)
        })
    }
    pub fn identity_list(&self) -> Result<String> {
        self.with_relations(true, list_identities)
    }
}

fn with_relations_view<T>(
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    collection: Collection<SimpleArchive>,
    read_only: bool,
    execute: impl FnOnce(&mut RelationsStorage<'_>) -> Result<T>,
) -> Result<T> {
    let facts_succinct = pile.attach::<SuccinctArchiveBlob>(collection, ())?;
    let facts_rank9 =
        pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(collection, facts_succinct)?;
    // Mutation preparation first derives this key's own commits into the
    // views, so an update reads what this key already wrote. It prepares on
    // the resident views as they then stand: another writer's pending update
    // that nobody has derived yet is simply not seen, and nothing here
    // reports it, because there is no complete state to wait for. A read
    // attaches what is there and never maintains, whoever the signer is.
    if !read_only {
        runtime
            .block_on(async {
                crate::storage::tolerate_own_lag(
                    pile.maintain_attached(facts_succinct, signer).await,
                )?;
                crate::storage::tolerate_own_lag(pile.maintain_attached(facts_rank9, signer).await)
            })
            .context("maintain Relations fact collection")?;
    }
    let reader = pile
        .snapshot()
        .context("freeze resident Relations fact collection")?;
    let reader = crate::storage::AcquiringReader::new(reader, Arc::clone(runtime));
    let observed = reader
        .attached_acquiring(facts_rank9)
        .context("observe Relations Rank9 projection")?;
    let view = crate::storage::acquire_attached_facts(&observed)
        .context("read Relations Rank9 projection")?;
    // Only exact payload gets may acquire here. Facts, records, proofs,
    // and their interpretation instant remain those of this observation.
    // Dispatch stays outside block_on: exact byte reads use the retained runtime.
    let payload_reader = reader;
    let mut storage = RelationsStorage {
        pile,
        signer,
        collection,
        facts: &view,
        reader: &payload_reader,
    };
    let result = execute(&mut storage)?;
    drop(storage);
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FactRead;
    use crate::storage::{
        initialize_signer, load_signer, open_pile_strict_as, open_store_as, runtime,
    };
    use std::fs;
    use triblespace::core::blob::encodings::UnknownBlob;
    use triblespace::core::blob::Bytes;
    use triblespace::core::collection::{
        CollectionRead, CollectionRecord, CollectionRecordSelector,
    };
    use triblespace::core::repo::StorageClose;

    fn profile(label: &str) -> ProfileInput {
        ProfileInput {
            label: label.to_owned(),
            aliases: vec!["old alias".to_owned()],
            emails: vec!["old@example.test".to_owned()],
            first_name: Some("Ada".to_owned()),
            ..ProfileInput::default()
        }
    }

    #[test]
    fn successive_profile_actions_advance_the_view_a_reader_prepares() {
        let directory = tempfile::tempdir().unwrap();
        let pile_path = directory.path().join("eager-relations.pile");
        let key = directory.path().join("eager-relations.key");
        fs::File::create(&pile_path).unwrap();
        initialize_signer(&pile_path, Some(&key)).unwrap();
        let storage = Storage::new(pile_path.clone(), Some(key));
        let (succinct, rank9) = storage
            .with_pile(|pile, signer| {
                let source = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
                let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
                Ok((
                    succinct,
                    pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?,
                ))
            })
            .unwrap();
        let observe = || {
            storage
                .with_pile(|pile, signer| {
                    let snapshot = pollster::block_on(async {
                        drop(pile.maintain_attached(succinct, signer).await?);
                        pile.maintain_attached(rank9, signer).await
                    })?;
                    Ok(snapshot.read_facts(rank9)?)
                })
                .unwrap()
        };
        let faculty = Relations::with_storage(storage.clone());
        let added = faculty.add(profile("Ada"), None, &[]).unwrap();
        let old_facts = observe();
        assert_eq!(
            relations::current_profile(&old_facts, added.person)
                .unwrap()
                .id,
            added.profile
        );
        let changed = faculty
            .set(
                &format!("{:x}", added.person),
                ProfilePatch {
                    company: Some("Analytical Engines".into()),
                    ..ProfilePatch::default()
                },
                &[],
            )
            .unwrap();
        let facts = observe();
        assert_eq!(
            Some(relations::current_profile(&facts, added.person).unwrap().id),
            changed.current
        );
        assert_eq!(
            relations::current_profile(&old_facts, added.person)
                .unwrap()
                .id,
            added.profile
        );
    }

    #[test]
    fn profile_patch_preserves_unspecified_and_replaces_sets() {
        let mut value = profile("ada");
        let changed = apply_profile_patch(
            &mut value,
            ProfilePatch {
                aliases: Some(vec!["Countess".to_owned(), "Enchantress".to_owned()]),
                company: Some("Analytical Engines".to_owned()),
                ..ProfilePatch::default()
            },
        )
        .unwrap();
        assert!(changed);
        assert_eq!(value.first_name.as_deref(), Some("Ada"));
        assert_eq!(value.emails, vec!["old@example.test"]);
        assert_eq!(value.aliases, vec!["Countess", "Enchantress"]);
        assert_eq!(value.company.as_deref(), Some("Analytical Engines"));
    }

    #[test]
    fn profile_patch_clear_is_explicit_and_conflicts_with_replacement() {
        let mut value = profile("ada");
        apply_profile_patch(
            &mut value,
            ProfilePatch {
                clear: vec![ProfileField::FirstName, ProfileField::Emails],
                ..ProfilePatch::default()
            },
        )
        .unwrap();
        assert_eq!(value.first_name, None);
        assert!(value.emails.is_empty());

        let error = apply_profile_patch(
            &mut value,
            ProfilePatch {
                emails: Some(vec!["new@example.test".to_owned()]),
                clear: vec![ProfileField::Emails],
                ..ProfilePatch::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("conflicts"));
    }

    #[test]
    fn non_writer_reads_and_prepares_pending_updates_from_their_bytes_without_publishing() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let runtime = Arc::new(runtime().unwrap());
        let owner = SigningKey::from_bytes(&[91; 32]);
        let reader = SigningKey::from_bytes(&[92; 32]);
        let mut pile = open_store_as(file.path(), owner.verifying_key()).unwrap();
        let source =
            crate::collection_names::open(&mut pile, DEFAULT_SCOPE_ID, owner.verifying_key())
                .unwrap();
        let succinct = pile.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
        let rank9 = pile
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
            .unwrap();
        let selectors = BTreeSet::from([
            CollectionRecordSelector::Collection(source.handle()),
            CollectionRecordSelector::Collection(succinct.handle()),
            CollectionRecordSelector::Collection(rank9.handle()),
        ]);
        let first = genid().id;
        let second = genid().id;
        pile.commit(
            source,
            &owner,
            relations::person_fragment(first, profile("Ada")).unwrap().0,
        )
        .unwrap();
        // The worker carries the newly authored person; a read attaches it.
        crate::storage::carry_facts(&mut pile, source, &owner);
        with_relations_view(&mut pile, &owner, &runtime, source, true, |storage| {
            assert!(list_people(storage, 20, false, false)?.contains("Ada"));
            Ok(())
        })
        .unwrap();

        let before = pile.snapshot().unwrap().select_records(&selectors).unwrap();
        with_relations_view(&mut pile, &reader, &runtime, source, true, |storage| {
            assert!(show_entry(storage, "Ada".to_owned(), &EntryKind::ALL)?.contains("Ada"));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            pile.snapshot().unwrap().select_records(&selectors).unwrap(),
            before
        );

        pile.commit(
            source,
            &owner,
            relations::person_fragment(second, profile("Grace"))
                .unwrap()
                .0,
        )
        .unwrap();
        // The second person is not attached yet: a reader reads its commit
        // from the bytes, and attaches nothing.
        let before = pile.snapshot().unwrap().select_records(&selectors).unwrap();
        with_relations_view(&mut pile, &reader, &runtime, source, true, |storage| {
            assert_eq!(
                relations::person_anchors(storage.facts),
                BTreeSet::from([first, second])
            );
            let list = list_people(storage, 20, false, false)?;
            assert!(list.contains("Ada"));
            assert!(list.contains("Grace"));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            pile.snapshot().unwrap().select_records(&selectors).unwrap(),
            before
        );

        // Preparation attaches only what the preparer wrote. A non-writer
        // owns nothing here, so it publishes nothing, and prepares on the same
        // read. The attachments' lag counts the unattached commit for whoever
        // asks, as this test does below.
        let mut mutation_prepared = false;
        with_relations_view(&mut pile, &reader, &runtime, source, false, |storage| {
            mutation_prepared = true;
            assert!(list_people(storage, 20, false, false)?.contains("Grace"));
            Ok(())
        })
        .unwrap();
        assert!(mutation_prepared);
        assert_eq!(
            pile.snapshot().unwrap().select_records(&selectors).unwrap(),
            before
        );
        let lag = crate::storage::FactLag::of(&pile.snapshot().unwrap(), source, succinct, rank9)
            .unwrap();
        assert_eq!(lag.succinct, 1);

        // Once the worker has carried the second person, the read is the
        // same, now from the attachment.
        crate::storage::carry_facts(&mut pile, source, &owner);
        with_relations_view(&mut pile, &owner, &runtime, source, true, |storage| {
            assert_eq!(
                relations::person_anchors(storage.facts),
                BTreeSet::from([first, second])
            );
            Ok(())
        })
        .unwrap();
        pile.close().unwrap();
    }

    #[test]
    fn native_collection_updates_expose_profile_forks() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("relations.pile");
        let key = directory.path().join("relations.key");
        fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        let signer = load_signer(&pile, Some(&key)).unwrap();
        let mut store = open_pile_strict_as(&pile, signer.verifying_key()).unwrap();
        let collection =
            open_configured(&mut store, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        let facts_succinct = store.attach::<SuccinctArchiveBlob>(collection, ()).unwrap();
        let facts_rank9 = store
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(collection, facts_succinct)
            .unwrap();

        let person = genid().id;
        let (fragment, initial, _) = relations::person_fragment(person, profile("Ada")).unwrap();
        store.commit(collection, &signer, fragment).unwrap();

        let left = relations::profile_fragment(person, profile("Ada Left"), &[initial]).unwrap();
        let right = relations::profile_fragment(person, profile("Ada Right"), &[initial]).unwrap();
        store.commit(collection, &signer, left).unwrap();
        store.commit(collection, &signer, right).unwrap();

        let reader = pollster::block_on(async {
            drop(store.ensure(collection, &signer).await?);
            drop(store.maintain_attached(facts_succinct, &signer).await?);
            store.maintain_attached(facts_rank9, &signer).await
        })
        .unwrap();
        let observed = reader.attached(facts_rank9).unwrap();
        let view = observed.facts().unwrap();
        match relations::profile_head(&view, person).unwrap() {
            Head::Forked(heads) => assert_eq!(heads.len(), 2),
            other => panic!("expected visible fork, got {other:?}"),
        }
        drop(view);
        drop(observed);
        drop(reader);
        store.close().unwrap();
    }

    #[test]
    fn snapshot_payload_reads_keep_relations_facts_frozen_and_publish_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("relations.pile");
        let key = directory.path().join("relations.key");
        fs::File::create(&path).unwrap();
        let signer = initialize_signer(&path, Some(&key)).unwrap();
        let runtime = Arc::new(runtime().unwrap());
        let mut pile = open_store_as(&path, signer.verifying_key()).unwrap();
        let collection =
            crate::collection_names::open(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key())
                .unwrap();
        let succinct = pile.attach::<SuccinctArchiveBlob>(collection, ()).unwrap();
        let rank9 = pile
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(collection, succinct)
            .unwrap();

        let person = genid().id;
        let (fragment, initial_profile, _) =
            relations::person_fragment(person, profile("Ada")).unwrap();
        let (facts, mut payloads) = fragment.into_facts_and_blobs();
        let label = relations::profile_snapshot(&facts, initial_profile)
            .unwrap()
            .label;
        // The source archive is present, but its text attachments are cold.
        let initial = pile.commit(collection, &signer, facts.into()).unwrap();
        let frozen = runtime
            .block_on(async {
                drop(pile.ensure(collection, &signer).await?);
                drop(pile.maintain_attached(succinct, &signer).await?);
                pile.maintain_attached(rank9, &signer).await
            })
            .unwrap();
        assert!(!frozen.contains_blob(label).unwrap());
        let observed = frozen.attached(rank9).unwrap();
        let view = observed.facts().unwrap();
        let reader = crate::storage::AcquiringReader::new(frozen.clone(), Arc::clone(&runtime));

        // Model bytes arriving in shared backing after the semantic snapshot.
        // Exact get may use them; it must not adopt the later person's facts.
        let payloads = payloads.snapshot().unwrap();
        for blob in payloads.blobs() {
            let blob = blob.unwrap();
            let bytes: Bytes = payloads.get(blob.handle).unwrap();
            pile.put::<UnknownBlob, _>(bytes).unwrap();
        }
        let later_person = genid().id;
        let (later, _, _) = relations::person_fragment(later_person, profile("Ada")).unwrap();
        pile.commit(collection, &signer, later).unwrap();

        let mut storage = RelationsStorage {
            pile: &mut pile,
            signer: &signer,
            collection,
            facts: &view,
            reader: &reader,
        };
        storage
            .with_view(|facts, reader| {
                assert_eq!(resolve_person_anchor(reader, facts, "Ada", false)?, person);
                assert_eq!(
                    resolve_person_anchor(reader, facts, "old alias", false)?,
                    person
                );
                assert_eq!(relations::person_anchors(facts), BTreeSet::from([person]));
                let current = relations::current_profile(facts, person)?;
                assert_eq!(relations::profile_input(reader, &current)?, profile("Ada"));
                Ok(())
            })
            .unwrap();
        let successor = storage
            .update(|facts, reader| {
                let current = relations::current_profile(facts, person)?;
                let mut input = relations::profile_input(reader, &current)?;
                input.note = Some("one snapshot-backed publication".to_owned());
                let fragment = relations::profile_fragment(person, input, &[current.id])?;
                let successor = fragment.root().unwrap();
                Ok((Some(fragment), successor))
            })
            .unwrap();
        assert_ne!(successor, initial_profile);
        drop(storage);

        let selectors = BTreeSet::from([CollectionRecordSelector::Collection(collection.handle())]);
        assert!(!frozen.contains_blob(label).unwrap());
        assert_eq!(
            frozen.select_records(&selectors).unwrap(),
            vec![CollectionRecord::Commit(initial)],
        );
        assert_eq!(
            relations::profile_head(&view, person).unwrap(),
            Head::Unique(initial_profile)
        );
        let after = pile.snapshot().unwrap();
        assert_eq!(after.select_records(&selectors).unwrap().len(), 3);
        assert_eq!(after.wants().unwrap().count(), 0);
        drop(after);
        drop(reader);
        drop(view);
        drop(observed);
        drop(frozen);
        pile.close().unwrap();
    }

    fn scratch_relations(directory: &tempfile::TempDir) -> Relations {
        let pile_path = directory.path().join("ties.pile");
        let key = directory.path().join("ties.key");
        fs::File::create(&pile_path).unwrap();
        initialize_signer(&pile_path, Some(&key)).unwrap();
        Relations::with_storage(Storage::new(pile_path, Some(key)))
    }

    fn organisation(label: &str, aliases: &[&str]) -> ProfileInput {
        ProfileInput {
            label: label.to_owned(),
            aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
            profile_urls: vec!["https://example.test/org".to_owned()],
            ..ProfileInput::default()
        }
    }

    fn tie_request(subject: &str, relation: &str, object: &str) -> TieRequest {
        TieRequest {
            subject: subject.to_owned(),
            relation: relation.to_owned(),
            object: object.to_owned(),
            ..TieRequest::default()
        }
    }

    #[test]
    fn organisations_are_their_own_addressable_entries() {
        let directory = tempfile::tempdir().unwrap();
        let faculty = scratch_relations(&directory);
        let person = faculty.add(profile("Ada"), None, &[]).unwrap().person;
        let added = faculty
            .org_add(
                organisation("Bremer Aufbau-Bank", &["BAB"]),
                &["https://example.test/source".to_owned()],
            )
            .unwrap();
        assert_ne!(added.organisation, person);

        let shown = faculty.org_show("bab").unwrap();
        assert!(shown.contains(&format!("organisation: {:x}", added.organisation)));
        assert!(shown.contains("label: Bremer Aufbau-Bank"));
        assert!(shown.contains("alias: BAB"));
        assert!(shown.contains("profile_url: https://example.test/org"));
        assert!(shown.contains("source: https://example.test/source"));
        assert!(shown.contains("retired: false"));
        // `relations show` reaches organisations and persons alike.
        assert!(faculty
            .show("Bremer Aufbau-Bank")
            .unwrap()
            .contains("organisation: "));
        assert!(faculty.show("Ada").unwrap().contains("person: "));
        // Persons and organisations never leak into each other's lists.
        assert!(faculty
            .org_list(50, PeopleFilter::Active)
            .unwrap()
            .contains("Bremer Aufbau-Bank"));
        assert!(!faculty
            .org_list(50, PeopleFilter::Active)
            .unwrap()
            .contains("Ada"));
        assert!(!faculty
            .list(50, PeopleFilter::Active)
            .unwrap()
            .contains("Bremer"));
        assert!(faculty.org_show("Ada").is_err());
        // An extrinsic anchor is never minted twice for one name.
        let error = faculty
            .org_add(organisation("bab", &[]), &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");

        let updated = faculty
            .org_set(
                "BAB",
                ProfilePatch {
                    note: Some("Förderbank des Landes Bremen".to_owned()),
                    ..ProfilePatch::default()
                },
                &[],
            )
            .unwrap();
        assert!(updated.current.is_some());
        assert!(faculty
            .org_show("BAB")
            .unwrap()
            .contains("Förderbank des Landes Bremen"));
        assert!(matches!(
            faculty.org_retire("BAB").unwrap(),
            LifecycleChange::Changed { .. }
        ));
        assert!(!faculty
            .org_list(50, PeopleFilter::Active)
            .unwrap()
            .contains("Bremer"));
        assert!(faculty
            .org_list(50, PeopleFilter::Retired)
            .unwrap()
            .contains("Bremer"));
    }

    #[test]
    fn a_repeated_tie_converges_with_both_sources_and_shows_from_both_ends() {
        let directory = tempfile::tempdir().unwrap();
        let faculty = scratch_relations(&directory);
        let person = faculty.add(profile("Ada"), None, &[]).unwrap().person;
        let bank = faculty
            .org_add(organisation("Bremer Aufbau-Bank", &["BAB"]), &[])
            .unwrap()
            .organisation;

        let mut request = tie_request("Ada", "works at", "BAB");
        request.role = Some("Geschäftsführerin".to_owned());
        request.since = Some("2019".to_owned());
        request.sources = vec!["https://example.test/a".to_owned()];
        request.persona = Some("Ada".to_owned());
        let first = faculty.tie_add(request.clone()).unwrap();
        assert_eq!((first.subject, first.object), (person, bank));
        assert!(!first.known);

        request.sources = vec!["https://example.test/b".to_owned()];
        request.persona = None;
        let second = faculty.tie_add(request).unwrap();
        assert_eq!(second.tie, first.tie, "the same claim has one identity");
        assert!(second.known);

        for end in ["Ada", "BAB"] {
            let listed = faculty.ties(end).unwrap();
            assert_eq!(listed.matches(&format!("{:x}:", first.tie)).count(), 1);
            assert!(listed.contains("ties: 1"), "{listed}");
            assert!(listed.contains("Ada"));
            assert!(listed.contains("--works at--> Bremer Aufbau-Bank"));
            assert!(listed.contains("role: Geschäftsführerin"));
            assert!(listed.contains("since: 2019"));
            assert!(listed.contains("source: https://example.test/a"));
            assert!(listed.contains("source: https://example.test/b"));
            assert!(listed.contains("by: Ada"));
        }
        // `show` carries the same ties.
        assert!(faculty
            .show("BAB")
            .unwrap()
            .contains("--works at--> Bremer Aufbau-Bank"));

        // A different role is a different claim.
        let mut other = tie_request("Ada", "works at", "BAB");
        other.role = Some("Prokuristin".to_owned());
        assert_ne!(faculty.tie_add(other).unwrap().tie, first.tie);
        assert!(faculty.ties("Ada").unwrap().contains("ties: 2"));
        // A tie needs two different entries, and both must exist.
        assert!(faculty.tie_add(tie_request("Ada", "knows", "Ada")).is_err());
        assert!(faculty
            .tie_add(tie_request("Ada", "knows", "Nobody"))
            .is_err());
    }

    #[test]
    fn organisation_to_organisation_ties_and_ending_keeps_the_tie_visible() {
        let directory = tempfile::tempdir().unwrap();
        let faculty = scratch_relations(&directory);
        faculty
            .org_add(
                organisation("Starthaus Bremen & Bremerhaven", &["Starthaus"]),
                &[],
            )
            .unwrap();
        faculty
            .org_add(organisation("Bremer Aufbau-Bank", &["BAB"]), &[])
            .unwrap();
        let mut request = tie_request("Starthaus", "is run by", "BAB");
        request.since = Some("2021-03".to_owned());
        let tie = faculty.tie_add(request).unwrap().tie;
        let prefix = format!("{tie:x}")[..10].to_owned();
        let ended = faculty
            .tie_end(
                &prefix,
                "2024-06-30",
                &["note: programme moved".to_owned()],
                None,
            )
            .unwrap();
        assert_eq!(ended.tie, tie);
        assert_eq!(ended.until, "2024-06-30");
        let listed = faculty.ties("Starthaus").unwrap();
        assert!(listed.contains(&format!("{tie:x}:")), "{listed}");
        assert!(listed.contains("since: 2021-03"));
        assert!(listed.contains("until: 2024-06-30"));
        assert!(listed.contains("source: note: programme moved"));
        assert!(faculty.tie_end("zz", "2024", &[], None).is_err());
    }

    #[test]
    fn a_tie_may_name_a_retired_entry_but_a_live_one_wins() {
        let directory = tempfile::tempdir().unwrap();
        let faculty = scratch_relations(&directory);
        let bank = faculty
            .org_add(organisation("Bremer Aufbau-Bank", &["BAB"]), &[])
            .unwrap()
            .organisation;
        let gone = faculty
            .org_add(organisation("Bremer Innovations-Agentur", &["BIA"]), &[])
            .unwrap()
            .organisation;
        faculty.org_retire("BIA").unwrap();

        // History keeps the retired agency addressable as a tie end.
        let added = faculty
            .tie_add(tie_request("BIA", "was merged into", "BAB"))
            .unwrap();
        assert_eq!((added.subject, added.object), (gone, bank));
        assert!(faculty
            .ties("BAB")
            .unwrap()
            .contains("Bremer Innovations-Agentur"));

        // A live entry with the same name is preferred over the retired one.
        let retired = faculty.add(profile("Ada"), None, &[]).unwrap().person;
        faculty.retire(&format!("{retired:x}")).unwrap();
        let live = faculty.add(profile("Ada"), None, &[]).unwrap().person;
        let second = faculty
            .tie_add(tie_request("Ada", "works at", "BAB"))
            .unwrap();
        assert_eq!(second.subject, live);

        // A name that matches nothing still fails.
        assert!(faculty
            .tie_add(tie_request("Nobody", "knows", "BAB"))
            .is_err());
    }
}
