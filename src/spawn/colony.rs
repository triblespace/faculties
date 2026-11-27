//! The colony at a glance: for each parent persona, what the existing
//! records say it is doing, which zooids it started have no end recorded,
//! which finished reports it has not acknowledged, and what it waits on
//! from JP.
//!
//! Nothing here is authored state. [`colony_view`] is one pure projection
//! over facts a caller already holds: the routed Spawn, Message, Relations
//! and Goals collections, the Goals status register, and a blob reader.
//! The CLI (`zooid colony`) and the viewer both call it; they differ only in
//! how they obtained those facts and in whether they can observe units.
//! Each role can be unavailable on its own, and the view says which.
//!
//! A persona is never called idle: silence, a missing end record, an
//! unavailable collection or an unobservable unit is no evidence of
//! idleness, so where nothing current says what a persona is doing, the
//! answer is `unknown`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{Context, Result};
use hifitime::Epoch;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::lww_register::{LwwIndex, LwwQuery};
use triblespace::core::collection::{Collection, CollectionStoreExt};
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::*;

use super::operations::{describe_end, is_running, spawns_in, RunView, Spawn, SpawnView};
use super::IntervalValue;
use crate::goals::operations::{goal_reading_in, goal_states_in, GoalState};
use crate::message::operations::{envelope_id, inbox, outbox, read_by, settled_identity};
use crate::relations::{self, IdentityComponents, SelectorOutcome};
use crate::storage::{AcquiringReader, FactArchive};

/// The Goals tag that marks a question waiting on JP.
pub const ASK_JP_TAG: &str = "ask-jp";
/// Which personas to show besides every spawner of a recorded run and every
/// resident zooid (see [`ParentSource::Roster`]): selectors (label, alias,
/// id or id prefix) resolved through Relations, retired personas included.
#[derive(Clone, Debug, Default)]
pub struct ColonyOptions {
    pub personas: Vec<String>,
}

/// One input a colony view reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColonyRole {
    Spawn,
    Message,
    Relations,
    Goals,
    /// Goals's maintained status register, without which no goal has a
    /// current status here.
    GoalsStatus,
}

impl ColonyRole {
    pub fn name(self) -> &'static str {
        match self {
            Self::Spawn => "Spawn",
            Self::Message => "Message",
            Self::Relations => "Relations",
            Self::Goals => "Goals",
            Self::GoalsStatus => "Goals status register",
        }
    }
}

/// The routed facts of each role, or why that role is unavailable. Missing
/// roles never shift another role into their place.
#[derive(Clone, Copy)]
pub struct ColonyRoles<'a> {
    pub spawn: Result<&'a FactArchive, &'a str>,
    pub message: Result<&'a FactArchive, &'a str>,
    pub relations: Result<&'a FactArchive, &'a str>,
    pub goals: Result<&'a FactArchive, &'a str>,
    /// `Err` when the register was not loaded, with the reason.
    pub goals_status: Result<&'a LwwQuery, &'a str>,
}

/// Why a parent is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParentSource {
    /// It is the recorded spawner of a run.
    Spawner,
    /// It is a resident zooid: a Relations person carrying the zooid
    /// affinity, not retired, and not the persona of any recorded spawn run
    /// (task zooids show under their spawner instead).
    Roster,
    /// It was named in [`ColonyOptions::personas`].
    Asked,
}

/// One goal as the board holds it now.
#[derive(Clone, Debug)]
pub struct GoalLine {
    pub goal: Id,
    pub title: String,
    pub status: String,
    /// The time of the newest current note, as Goals prints it.
    pub last_note: Option<String>,
}

/// One run of a zooid this persona started.
#[derive(Clone, Debug)]
pub struct WorkerLine {
    pub name: String,
    pub run: Id,
    pub started: Option<IntervalValue>,
    pub finished: Option<IntervalValue>,
    pub end: String,
    pub unit: Option<String>,
    /// The unit's state as the caller's unit observer read it; `None` when
    /// the caller has no observer or the run has ended. A run started on
    /// another host reads `inactive` here whatever it is doing there.
    pub unit_state_here: Option<String>,
    pub goal: Vec<Id>,
}

/// What the evidence supports saying about a persona, each with its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// A run it started has no end record and its unit is active here.
    WaitingOnZooids(usize),
    /// A finished zooid's report message to it is still unread.
    ReviewOutstanding(usize),
    /// Goals tagged ask-jp and with its tag are open.
    WaitingOnJp(usize),
    /// Goals tagged with its name are in `doing` on the board.
    Doing(usize),
}

/// One parent persona.
#[derive(Clone, Debug)]
pub struct ParentView {
    /// The settled identity's smallest anchor; `None` only for an asked
    /// selector that did not resolve.
    pub persona: Option<Id>,
    pub label: String,
    pub shown_because: Vec<ParentSource>,
    pub doing: Vec<GoalLine>,
    pub asks_jp: Vec<GoalLine>,
    /// Started, no end recorded. Whether it still runs is `unit_state_here`.
    pub open: Vec<WorkerLine>,
    /// Ended, and the report message to this persona is unread.
    pub unreviewed: Vec<WorkerLine>,
    /// Ended runs whose report message is not unread: acknowledged, or
    /// never sent (see `zooid show`).
    pub settled: usize,
    /// Every ended run that is not in [`Self::unreviewed`], newest end
    /// first: the `settled` and `review_unknown` runs, so a reader can open
    /// what each lived.
    pub ended: Vec<WorkerLine>,
    /// Ended runs whose review state cannot be read here (Message or
    /// Relations unavailable).
    pub review_unknown: usize,
    /// Newest outgoing message; `None` when it sent none or Message is
    /// unavailable (see [`ColonyView::unavailable`]).
    pub last_message: Option<IntervalValue>,
    /// Newest run start where it is the spawner.
    pub last_spawn: Option<IntervalValue>,
    /// Unread incoming envelopes; `None` when that cannot be read here.
    pub unread: Option<usize>,
    /// The persona is a task zooid's (named by its spawn).
    pub zooid: bool,
    pub evidence: Vec<Evidence>,
    /// What could not be read for this persona, in words.
    pub unreadable: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ColonyView {
    pub observed_at: IntervalValue,
    /// Whether the caller could observe unit states (this host's systemd).
    pub units_observed: bool,
    pub parents: Vec<ParentView>,
    /// Runs with no recorded spawner.
    pub unparented: Vec<WorkerLine>,
    /// Roles that could not be read at all, each with its reason.
    pub unavailable: Vec<(ColonyRole, String)>,
    /// What else could not be read, in words (the zooid roster, a selector).
    pub unreadable: Vec<String>,
}

impl ColonyView {
    pub fn available(&self, role: ColonyRole) -> bool {
        !self.unavailable.iter().any(|(missing, _)| *missing == role)
    }
}

fn start_key(value: Option<IntervalValue>) -> i128 {
    value
        .and_then(|value| value.try_from_inline::<(Epoch, Epoch)>().ok())
        .map(|(start, _)| start.to_tai_duration().total_nanoseconds())
        .unwrap_or(i128::MIN)
}

fn newest(values: impl IntoIterator<Item = IntervalValue>) -> Option<IntervalValue> {
    values
        .into_iter()
        .max_by_key(|value| start_key(Some(*value)))
}

fn worker_line(
    view: &SpawnView,
    run: &RunView,
    unit_state: Option<&dyn Fn(&str) -> String>,
) -> WorkerLine {
    let unit = run.unit.first().cloned();
    let unit_state_here = match (&unit, unit_state, run.ended()) {
        (Some(unit), Some(observe), false) => Some(observe(unit)),
        _ => None,
    };
    WorkerLine {
        name: view.name.clone(),
        run: run.id,
        started: run.started,
        finished: newest(run.finished.iter().copied()),
        end: describe_end(run),
        unit,
        unit_state_here,
        goal: view.goal.clone(),
    }
}

fn goal_line<R: BlobStoreGet>(
    reader: &R,
    goals: &FactArchive,
    register: &LwwQuery,
    state: &GoalState,
) -> GoalLine {
    let status = state.status.clone().unwrap_or_else(|| "todo".to_owned());
    match goal_reading_in(reader, goals, register, state.goal) {
        Ok(reading) => GoalLine {
            goal: state.goal,
            title: reading
                .titles
                .first()
                .cloned()
                .unwrap_or_else(|| "(no title here)".to_owned()),
            status,
            last_note: reading.notes.last().map(|note| note.at.clone()),
        },
        Err(error) => GoalLine {
            goal: state.goal,
            title: format!("(unreadable: {error:#})"),
            status,
            last_note: None,
        },
    }
}

/// Per-parent working set while the view is built; consumed before return.
#[derive(Default)]
struct Gathered<'s> {
    anchors: BTreeSet<Id>,
    shown_because: BTreeSet<ParentSource>,
    runs: Vec<(&'s SpawnView, &'s RunView)>,
}

/// The colony view, a pure projection over routed facts: see the module
/// documentation. `unit_state` reads one unit's state where the caller can
/// observe units (the CLI passes this host's systemd); with `None`, a run
/// with no end record says so and is never counted as running.
pub fn colony_view<R: BlobStoreGet>(
    roles: ColonyRoles<'_>,
    reader: &R,
    observed_at: IntervalValue,
    unit_state: Option<&dyn Fn(&str) -> String>,
    options: &ColonyOptions,
) -> ColonyView {
    let mut unavailable = Vec::new();
    let mut unreadable = Vec::new();
    for (role, missing) in [
        (ColonyRole::Spawn, roles.spawn.err()),
        (ColonyRole::Message, roles.message.err()),
        (ColonyRole::Relations, roles.relations.err()),
        (ColonyRole::Goals, roles.goals.err()),
        (ColonyRole::GoalsStatus, roles.goals_status.err()),
    ] {
        if let Some(reason) = missing {
            unavailable.push((role, reason.to_owned()));
        }
    }

    let spawns = match roles.spawn {
        Ok(facts) => match spawns_in(facts, reader) {
            Ok(spawns) => spawns,
            Err(error) => {
                unreadable.push(format!("Spawn runs: {error:#}"));
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    };
    let identities = roles
        .relations
        .ok()
        .map(|facts| IdentityComponents::from_facts(facts))
        .transpose()
        .unwrap_or_else(|error| {
            unreadable.push(format!("Relations identities: {error:#}"));
            None
        });
    // Every anchor Relations settles as the same person; the smallest one
    // keys the parent, so two anchors of one persona are one parent.
    let settled = |id: Id| -> Vec<Id> {
        match &identities {
            Some(identities) => settled_identity(identities, id),
            None => vec![id],
        }
    };
    let key = |id: Id| -> Id { settled(id).into_iter().min().unwrap_or(id) };

    let mut gathered: BTreeMap<Id, Gathered<'_>> = BTreeMap::new();
    let mut unparented = Vec::new();
    for view in &spawns {
        for run in &view.runs {
            if run.spawner.is_empty() {
                unparented.push(worker_line(view, run, unit_state));
            }
            for spawner in &run.spawner {
                let parent = gathered.entry(key(*spawner)).or_default();
                parent.anchors.insert(*spawner);
                parent.shown_because.insert(ParentSource::Spawner);
                parent.runs.push((view, run));
            }
        }
    }
    // A persona that is itself a zooid's is named by its spawn; its persona
    // is retired once its run settles.
    let zooid_names: BTreeMap<Id, String> = spawns
        .iter()
        .flat_map(|view| {
            view.runs
                .iter()
                .flat_map(move |run| run.persona.iter().map(move |id| (*id, view.name.clone())))
        })
        .collect();

    if let Ok(facts) = roles.relations {
        match relations::live_zooids(reader, facts) {
            Ok(members) => {
                for member in members {
                    if settled(member)
                        .iter()
                        .any(|id| zooid_names.contains_key(id))
                    {
                        continue;
                    }
                    let parent = gathered.entry(key(member)).or_default();
                    parent.anchors.insert(member);
                    parent.shown_because.insert(ParentSource::Roster);
                }
            }
            Err(error) => unreadable.push(format!("Relations zooid roster: {error:#}")),
        }
    }
    let mut unresolved = Vec::new();
    for selector in &options.personas {
        let found = match roles.relations {
            Ok(facts) => match relations::resolve_person(reader, facts, selector, true) {
                Ok(SelectorOutcome::Unique(id)) => Ok(id),
                Ok(outcome) => Err(format!("Relations resolves it as {outcome:?}")),
                Err(error) => Err(format!("{error:#}")),
            },
            Err(_) => Id::from_hex(selector.trim())
                .ok_or_else(|| "Relations is unavailable and it is not a full id".to_owned()),
        };
        match found {
            Ok(id) => {
                let parent = gathered.entry(key(id)).or_default();
                parent.anchors.insert(id);
                parent.shown_because.insert(ParentSource::Asked);
            }
            Err(reason) => unresolved.push((selector.clone(), reason)),
        }
    }

    let goal_states = match (roles.goals, roles.goals_status) {
        (Ok(goals), Ok(register)) => match goal_states_in(reader, goals, register) {
            Ok(states) => Some(states),
            Err(error) => {
                unreadable.push(format!("goals: {error:#}"));
                None
            }
        },
        _ => None,
    };

    let mut parents = Vec::new();
    for (persona, gathered) in gathered {
        let mut parent = ParentView {
            persona: Some(persona),
            label: format!("{persona:x}"),
            shown_because: gathered.shown_because.into_iter().collect(),
            doing: Vec::new(),
            asks_jp: Vec::new(),
            open: Vec::new(),
            unreviewed: Vec::new(),
            settled: 0,
            review_unknown: 0,
            ended: Vec::new(),
            last_message: None,
            last_spawn: None,
            unread: None,
            zooid: false,
            evidence: Vec::new(),
            unreadable: Vec::new(),
        };
        let mut mine: Vec<Id> = gathered
            .anchors
            .iter()
            .flat_map(|anchor| settled(*anchor))
            .collect();
        mine.sort();
        mine.dedup();

        if let Some(name) = mine.iter().find_map(|id| zooid_names.get(id)) {
            parent.label = name.clone();
            parent.zooid = true;
        } else if let Ok(facts) = roles.relations {
            let hex = |id: &Id| format!("{id:x}");
            if let Some(label) = gathered
                .anchors
                .iter()
                .map(|anchor| (anchor, relations::entry_label(reader, facts, *anchor)))
                .find(|(anchor, label)| *label != hex(anchor))
                .map(|(_, label)| label)
            {
                parent.label = label;
            }
        }

        let mut unread_ids: Option<BTreeSet<Id>> = None;
        if let Ok(messages) = roles.message {
            parent.last_message = newest(outbox(messages, &mine).iter().map(|envelope| envelope.4));
            if let Ok(relation_facts) = roles.relations {
                let ids: BTreeSet<Id> = inbox(messages, relation_facts, &mine)
                    .iter()
                    .map(envelope_id)
                    .filter(|id| !read_by(messages, *id, &mine))
                    .collect();
                parent.unread = Some(ids.len());
                unread_ids = Some(ids);
            }
        }

        parent.last_spawn = newest(gathered.runs.iter().filter_map(|(_, run)| run.started));
        for (view, run) in gathered.runs {
            if !run.ended() {
                parent.open.push(worker_line(view, run, unit_state));
            } else {
                match &unread_ids {
                    Some(unread) if run.message.iter().any(|id| unread.contains(id)) => {
                        parent.unreviewed.push(worker_line(view, run, unit_state))
                    }
                    Some(_) => {
                        parent.settled += 1;
                        parent.ended.push(worker_line(view, run, unit_state));
                    }
                    None => {
                        parent.review_unknown += 1;
                        parent.ended.push(worker_line(view, run, unit_state));
                    }
                }
            }
        }
        parent
            .open
            .sort_by_key(|line| std::cmp::Reverse(start_key(line.started)));
        parent
            .unreviewed
            .sort_by_key(|line| std::cmp::Reverse(start_key(line.finished)));
        parent
            .ended
            .sort_by_key(|line| std::cmp::Reverse(start_key(line.finished)));

        if let (Some(states), Ok(goals), Ok(register)) =
            (&goal_states, roles.goals, roles.goals_status)
        {
            for state in states {
                if !state.tags.iter().any(|tag| *tag == parent.label) {
                    continue;
                }
                let status = state.status.as_deref().unwrap_or("todo");
                if state.tags.iter().any(|tag| tag == ASK_JP_TAG) {
                    if status != "done" {
                        parent
                            .asks_jp
                            .push(goal_line(reader, goals, register, state));
                    }
                } else if status == "doing" {
                    parent.doing.push(goal_line(reader, goals, register, state));
                }
            }
        }

        let running = parent
            .open
            .iter()
            .filter(|line| line.unit_state_here.as_deref().is_some_and(is_running))
            .count();
        if running > 0 {
            parent.evidence.push(Evidence::WaitingOnZooids(running));
        }
        if !parent.unreviewed.is_empty() {
            parent
                .evidence
                .push(Evidence::ReviewOutstanding(parent.unreviewed.len()));
        }
        if !parent.asks_jp.is_empty() {
            parent
                .evidence
                .push(Evidence::WaitingOnJp(parent.asks_jp.len()));
        }
        if !parent.doing.is_empty() {
            parent.evidence.push(Evidence::Doing(parent.doing.len()));
        }
        parents.push(parent);
    }
    for (selector, reason) in unresolved {
        parents.push(ParentView {
            persona: None,
            label: selector.clone(),
            shown_because: vec![ParentSource::Asked],
            doing: Vec::new(),
            asks_jp: Vec::new(),
            open: Vec::new(),
            unreviewed: Vec::new(),
            settled: 0,
            review_unknown: 0,
            ended: Vec::new(),
            last_message: None,
            last_spawn: None,
            unread: None,
            zooid: false,
            evidence: Vec::new(),
            unreadable: vec![format!("persona {selector}: {reason}")],
        });
    }
    parents.sort_by(|a, b| a.label.cmp(&b.label));

    ColonyView {
        observed_at,
        units_observed: unit_state.is_some(),
        parents,
        unparented,
        unavailable,
        unreadable,
    }
}

/// Register the Succinct and Rank9 pair over one routed collection.
/// Registration only: a read never maintains, and nothing is written.
fn register(
    spawn: &Spawn,
    pile: &mut crate::storage::FacultyStore,
    signer: &ed25519_dalek::SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    scope: Id,
) -> Result<(
    Collection<triblespace::core::blob::encodings::simplearchive::SimpleArchive>,
    Collection<Rank9AcceleratedSuccinctArchiveBlob>,
)> {
    let source =
        spawn
            .storage()
            .open_collection_read(pile, scope, signer.verifying_key(), runtime)?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    Ok((source, rank9))
}

impl Spawn {
    /// The colony view through this storage: the four roles' routed facts
    /// from one frozen observation, as the viewer would hold them, and unit
    /// states from this host's launcher. Writes nothing.
    pub fn colony(&self, options: &ColonyOptions) -> Result<ColonyView> {
        let observed_at = crate::clock::point_now()?;
        self.storage().with_store(|pile, signer, runtime| {
            let scopes = [
                crate::schemas::spawn::DEFAULT_SCOPE_ID,
                crate::schemas::message::DEFAULT_SCOPE_ID,
                crate::schemas::relations::DEFAULT_SCOPE_ID,
                crate::schemas::compass::DEFAULT_SCOPE_ID,
            ];
            let registered: Vec<Result<_, String>> = scopes
                .iter()
                .map(|scope| {
                    register(self, pile, signer, runtime, *scope).map_err(|e| format!("{e:#}"))
                })
                .collect();
            let register_target = match &registered[3] {
                Ok((source, _)) => crate::goals::status_register_for_source(pile, *source)
                    .map_err(|e| format!("{e:#}")),
                Err(_) => Err("Goals is unavailable".to_owned()),
            };
            let reader = AcquiringReader::new(
                pile.snapshot().context("freeze the colony observation")?,
                runtime.clone(),
            );
            // Each role reads what is here: the view is the
            // colony as these foundations hold it, and each one the read
            // could not load is named among the unreadable sources.
            let labels = ["Spawn", "Message", "Relations", "Goals"];
            let mut notes = Vec::new();
            let mut unread_goals = None;
            let facts: Vec<Result<FactArchive, String>> = registered
                .iter()
                .zip(labels)
                .map(|(registered, label)| match registered {
                    Ok((_, rank9)) => crate::storage::acquire_facts_read(&reader, *rank9)
                        .map(|read| {
                            let (facts, unread) = read.into_parts();
                            notes.extend(crate::storage::unread_foundations_note(label, &unread));
                            if label == "Goals" {
                                unread_goals = Some(unread);
                            }
                            facts
                        })
                        .map_err(|e| format!("{e:#}")),
                    Err(error) => Err(error.clone()),
                })
                .collect();
            let status: Result<LwwQuery, String> = register_target.and_then(|target| {
                (|| -> Result<LwwQuery> {
                    let (status, unread) = reader
                        .attached_acquiring(target)?
                        .read_acquiring::<LwwIndex>()?
                        .into_parts();
                    // The status register stands on the Goals foundations:
                    // name it only for one the facts did not miss.
                    if unread.members().any(|member| {
                        !unread_goals
                            .as_ref()
                            .is_some_and(|goals| goals.contains(member))
                    }) {
                        notes.extend(crate::storage::unread_foundations_note(
                            "Goals status",
                            &unread,
                        ));
                    }
                    Ok(status.query()?)
                })()
                .map_err(|e| format!("{e:#}"))
            });
            let role = |index: usize| facts[index].as_ref().map_err(String::as_str);
            let roles = ColonyRoles {
                spawn: role(0),
                message: role(1),
                relations: role(2),
                goals: role(3),
                goals_status: status.as_ref().map_err(String::as_str),
            };
            let observe = |unit: &str| {
                self.unit_state(unit)
                    .unwrap_or_else(|error| format!("unreadable: {error:#}"))
            };
            let observe: &dyn Fn(&str) -> String = &observe;
            let mut view = colony_view(roles, &reader, observed_at, Some(observe), options);
            view.unreadable.extend(notes);
            Ok(view)
        })
    }
}

fn time(value: Option<IntervalValue>) -> String {
    value
        .map(crate::care::format_time)
        .unwrap_or_else(|| "never seen".to_owned())
}

fn evidence_text(evidence: &Evidence) -> String {
    match evidence {
        Evidence::WaitingOnZooids(n) => format!(
            "waiting on {n} running zooid(s) (a run with no end, unit active on this host)"
        ),
        Evidence::ReviewOutstanding(n) => format!(
            "{n} finished zooid report(s) unread (Message; unread means not acknowledged, not that nothing was done with it)"
        ),
        Evidence::WaitingOnJp(n) => format!("{n} open ask-jp goal(s) (Goals)"),
        Evidence::Doing(n) => format!("{n} goal(s) in doing with its tag (Goals)"),
    }
}

fn worker_text(out: &mut String, line: &WorkerLine, units_observed: bool) {
    let unit = match (&line.unit, &line.unit_state_here) {
        (Some(unit), Some(state)) => format!("{unit} {state} here"),
        (Some(unit), None) if line.finished.is_none() && !units_observed => {
            format!("{unit}, unit not observable here")
        }
        (Some(unit), None) => unit.clone(),
        _ => "no unit recorded".to_owned(),
    };
    let goal = line
        .goal
        .first()
        .map(|goal| format!("  goal {goal:x}"))
        .unwrap_or_default();
    writeln!(
        out,
        "    {}  started {}  {}  [{}]{}",
        line.name,
        time(line.started),
        if line.finished.is_some() {
            format!("ended {} ({})", time(line.finished), line.end)
        } else {
            "no end recorded".to_owned()
        },
        unit,
        goal
    )
    .unwrap();
}

/// The view as text. `state:` lists only what a record supports, or says
/// `unknown`; it never says idle.
pub fn render_colony(view: &ColonyView) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "colony at {} (Spawn runs, Goals, Message, Relations; {})",
        crate::care::format_time(view.observed_at),
        if view.units_observed {
            "unit states from this host's systemd"
        } else {
            "unit states not observable here"
        }
    )
    .unwrap();
    for (role, reason) in &view.unavailable {
        writeln!(out, "unavailable: {}: {reason}", role.name()).unwrap();
    }
    for source in &view.unreadable {
        writeln!(out, "unreadable: {source}").unwrap();
    }
    let messages = view.available(ColonyRole::Message);
    let mut quiet_zooids = 0;
    for parent in &view.parents {
        // An ended zooid that spawned helpers: its persona is retired, and
        // nothing of it is open. Counted, not shown.
        if parent.zooid && parent.open.is_empty() && parent.unreviewed.is_empty() {
            quiet_zooids += 1;
            continue;
        }
        let id = parent
            .persona
            .map(|id| format!(" ({id:x})"))
            .unwrap_or_default();
        let kind = if parent.zooid { " zooid" } else { "" };
        writeln!(out, "\n{}{}{}", parent.label, kind, id).unwrap();
        if parent.evidence.is_empty() {
            writeln!(
                out,
                "  state: unknown (no current record says what it is doing)"
            )
            .unwrap();
        }
        for evidence in &parent.evidence {
            writeln!(out, "  state: {}", evidence_text(evidence)).unwrap();
        }
        writeln!(
            out,
            "  last message sent: {}  last spawn: {}  unread: {}",
            if messages {
                time(parent.last_message)
            } else {
                "unknown (Message unavailable)".to_owned()
            },
            time(parent.last_spawn),
            parent
                .unread
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
        )
        .unwrap();
        for source in &parent.unreadable {
            writeln!(out, "  unreadable: {source}").unwrap();
        }
        for goal in &parent.doing {
            writeln!(
                out,
                "  doing  {:x}  {}  (last note {})",
                goal.goal,
                goal.title.chars().take(90).collect::<String>(),
                goal.last_note.as_deref().unwrap_or("none")
            )
            .unwrap();
        }
        for goal in &parent.asks_jp {
            writeln!(
                out,
                "  asks JP ({})  {:x}  {}",
                goal.status,
                goal.goal,
                goal.title.chars().take(90).collect::<String>()
            )
            .unwrap();
        }
        if !parent.open.is_empty() {
            writeln!(out, "  zooids with no end recorded:").unwrap();
            for line in &parent.open {
                worker_text(&mut out, line, view.units_observed);
            }
        }
        if !parent.unreviewed.is_empty() {
            writeln!(out, "  finished, report unread:").unwrap();
            for line in &parent.unreviewed {
                worker_text(&mut out, line, view.units_observed);
            }
        }
        writeln!(out, "  other ended runs: {}", parent.settled).unwrap();
        if parent.review_unknown > 0 {
            writeln!(
                out,
                "  ended runs whose review cannot be read here: {}",
                parent.review_unknown
            )
            .unwrap();
        }
    }
    if quiet_zooids > 0 {
        writeln!(
            out,
            "\n{quiet_zooids} ended zooid(s) that spawned helpers, none with an open run or unread report"
        )
        .unwrap();
    }
    if !view.unparented.is_empty() {
        writeln!(out, "\nruns with no recorded spawner:").unwrap();
        for line in &view.unparented {
            worker_text(&mut out, line, view.units_observed);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Recipient;
    use crate::relations::ProfileInput;
    use crate::spawn::{run_fragment, settlement_fragment, spawn_fragment, Exit, RunFacts};
    use triblespace::core::blob::encodings::succinctarchive::{
        OrderedUniverse, SuccinctArchive, UnionArchive,
    };
    use triblespace::core::blob::MemoryBlobStore;
    use triblespace::core::collection::lww_register::derive_element;

    fn at(seconds: u64) -> IntervalValue {
        crate::clock::point(Epoch::from_unix_seconds(1_800_000_000.0 + seconds as f64)).unwrap()
    }

    fn archive(facts: &TribleSet) -> FactArchive {
        UnionArchive::new(vec![SuccinctArchive::<OrderedUniverse>::from(facts)])
    }

    /// One collection's facts plus a shared blob store, built from fragments.
    #[derive(Default)]
    struct Fixture {
        spawn: TribleSet,
        message: TribleSet,
        relations: TribleSet,
        goals: TribleSet,
        blobs: MemoryBlobStore,
    }

    impl Fixture {
        fn add(set: &mut TribleSet, blobs: &mut MemoryBlobStore, fragment: Fragment) {
            let (facts, more) = fragment.into_facts_and_blobs();
            *set += facts;
            blobs.union(more);
        }

        fn person(&mut self, label: &str) -> Id {
            let id = *rngid();
            let (fragment, ..) = relations::person_fragment(
                id,
                ProfileInput {
                    label: label.to_owned(),
                    ..ProfileInput::default()
                },
            )
            .unwrap();
            Self::add(&mut self.relations, &mut self.blobs, fragment);
            id
        }

        /// A person carrying the zooid affinity; returns it and its
        /// lifecycle head.
        fn zooid(&mut self, label: &str) -> (Id, Id) {
            let id = *rngid();
            let (fragment, _, lifecycle) = relations::person_fragment(
                id,
                ProfileInput {
                    label: label.to_owned(),
                    affinities: vec![relations::ZOOID_AFFINITY.to_owned()],
                    ..ProfileInput::default()
                },
            )
            .unwrap();
            Self::add(&mut self.relations, &mut self.blobs, fragment);
            (id, lifecycle)
        }

        fn retire(&mut self, person: Id, lifecycle: Id) {
            let fragment = relations::lifecycle_fragment(person, true, &[lifecycle]);
            Self::add(&mut self.relations, &mut self.blobs, fragment);
        }

        /// A run of a new zooid; returns the run and the zooid's persona.
        fn run(&mut self, brief: &str, spawner: Id, started: u64) -> (Id, Id) {
            let persona = *rngid();
            (self.run_as(brief, spawner, started, persona), persona)
        }

        /// A run whose zooid persona is `persona`; returns the run.
        fn run_as(&mut self, brief: &str, spawner: Id, started: u64, persona: Id) -> Id {
            let (fragment, spawn, ..) = spawn_fragment(brief).unwrap();
            Self::add(&mut self.spawn, &mut self.blobs, fragment);
            let (fragment, run) = run_fragment(&RunFacts {
                spawn,
                started_at: at(started),
                persona,
                spawner,
                unit: format!("spawn-{started}.service"),
                workdir: "/tmp".to_owned(),
                effort: "high".to_owned(),
                prompt: brief.to_owned(),
                wake: None,
                provider: "claude".to_owned(),
                provider_reason: "asked".to_owned(),
                model: None,
                model_unset: Some("none".to_owned()),
                untinted: None,
                resumes: None,
                cover: None,
                resident: false,
            });
            Self::add(&mut self.spawn, &mut self.blobs, fragment);
            run
        }

        /// The run ends and its zooid messages the spawner its report.
        fn settle(&mut self, run: Id, zooid: Id, spawner: Id, finished: u64) -> Id {
            let (message, id) = crate::message::message_fragment(
                zooid,
                &Recipient::Person(spawner),
                "done",
                at(finished),
            );
            Self::add(&mut self.message, &mut self.blobs, message);
            let (fragment, _) = settlement_fragment(
                run,
                at(finished),
                &Exit {
                    code: Some(0),
                    result: Some("success".to_owned()),
                    signal: None,
                },
                Some("report"),
                None,
                &Default::default(),
            );
            Self::add(&mut self.spawn, &mut self.blobs, fragment);
            Self::add(
                &mut self.spawn,
                &mut self.blobs,
                entity! { ExclusiveId::force_ref(&run) @ crate::schemas::spawn::spawn::message: id }
                    .into(),
            );
            id
        }

        fn goal(&mut self, title: &str, tags: &[&str], status: &str) -> Id {
            let tags: Vec<String> = tags.iter().map(|tag| (*tag).to_owned()).collect();
            let (mut fragment, goal) =
                crate::goals::goal_fragment(title, tags.clone(), None, at(1)).unwrap();
            for tag in &tags {
                fragment.put::<blobencodings::UTF8String, _>(tag.clone());
            }
            fragment.put::<blobencodings::UTF8String, _>(status.to_owned());
            fragment += crate::goals::status_fragment(goal, status, None, at(2)).unwrap();
            Self::add(&mut self.goals, &mut self.blobs, fragment);
            goal
        }

        fn status(&self) -> LwwQuery {
            let element = derive_element(
                &self.goals.clone().to_blob(),
                crate::schemas::compass::board::status_of.id(),
                triblespace::core::metadata::created_at.id(),
            )
            .unwrap();
            LwwIndex::decode(&element).unwrap().query().unwrap()
        }
    }

    /// Which roles to withhold from the view.
    #[derive(Default, Clone, Copy)]
    struct Withhold {
        spawn: bool,
        message: bool,
        relations: bool,
        goals: bool,
    }

    fn view(
        fixture: &mut Fixture,
        withhold: Withhold,
        units: Option<&dyn Fn(&str) -> String>,
        personas: &[&str],
    ) -> ColonyView {
        let spawn = archive(&fixture.spawn);
        let message = archive(&fixture.message);
        let relations = archive(&fixture.relations);
        let goals = archive(&fixture.goals);
        let status = fixture.status();
        let reader = fixture.blobs.snapshot().unwrap();
        let pick = |facts, withheld: bool| if withheld { Err("withheld") } else { Ok(facts) };
        let roles = ColonyRoles {
            spawn: pick(&spawn, withhold.spawn),
            message: pick(&message, withhold.message),
            relations: pick(&relations, withhold.relations),
            goals: pick(&goals, withhold.goals),
            goals_status: if withhold.goals {
                Err("withheld")
            } else {
                Ok(&status)
            },
        };
        let options = ColonyOptions {
            personas: personas.iter().map(|p| (*p).to_owned()).collect(),
        };
        colony_view(roles, &reader, at(100), units, &options)
    }

    fn parent<'v>(view: &'v ColonyView, label: &str) -> &'v ParentView {
        view.parents
            .iter()
            .find(|parent| parent.label == label)
            .unwrap_or_else(|| panic!("no parent {label}: {view:#?}"))
    }

    /// parent-a spawned three zooids: one still running, one finished with its
    /// report unread, one finished and read. a has a doing goal and an
    /// open ask-jp goal; parent-b has nothing current.
    fn colony() -> (Fixture, Id) {
        let mut fixture = Fixture::default();
        let (a, _) = fixture.zooid("parent-a");
        fixture.zooid("parent-b");
        fixture.run("keep running", a, 10);
        let (unread_run, unread_zooid) = fixture.run("report unread", a, 11);
        fixture.settle(unread_run, unread_zooid, a, 20);
        let (read_run, read_zooid) = fixture.run("report read", a, 12);
        let read = fixture.settle(read_run, read_zooid, a, 21);
        let (fragment, _) = crate::message::read_fragment(read, a, Some(at(30)));
        Fixture::add(&mut fixture.message, &mut fixture.blobs, fragment);
        fixture.goal("a goal in doing", &["parent-a"], "doing");
        fixture.goal("a question for JP", &["parent-a", ASK_JP_TAG], "todo");
        fixture.goal("a done question", &["parent-a", ASK_JP_TAG], "done");
        fixture.goal("parent-b's todo", &["parent-b"], "todo");
        (fixture, a)
    }

    #[test]
    fn every_evidence_kind_from_facts_alone() {
        let (mut fixture, a) = colony();
        let active = |_: &str| "active".to_owned();
        let view = view(&mut fixture, Withhold::default(), Some(&active), &[]);
        assert!(view.unavailable.is_empty(), "{view:#?}");
        let a_view = parent(&view, "parent-a");
        assert_eq!(a_view.persona, Some(a));
        assert_eq!(
            a_view.evidence,
            vec![
                Evidence::WaitingOnZooids(1),
                Evidence::ReviewOutstanding(1),
                Evidence::WaitingOnJp(1),
                Evidence::Doing(1),
            ]
        );
        assert_eq!(a_view.settled, 1);
        assert_eq!(a_view.unread, Some(1));
        assert_eq!(a_view.asks_jp[0].title, "a question for JP");
        assert!(a_view.last_spawn.is_some());
        // parent-b is only on the roster and nothing current says what it does.
        let b = parent(&view, "parent-b");
        assert_eq!(b.shown_because, vec![ParentSource::Roster]);
        assert!(b.evidence.is_empty());
        let text = render_colony(&view);
        assert!(!text.to_lowercase().contains("idle"), "{text}");
        assert!(text.contains("waiting on 1 running zooid(s)"), "{text}");
    }

    #[test]
    fn no_unit_observer_never_reads_as_running() {
        let (mut fixture, _) = colony();
        let view = view(&mut fixture, Withhold::default(), None, &[]);
        let a = parent(&view, "parent-a");
        assert!(
            !a.evidence.contains(&Evidence::WaitingOnZooids(1)),
            "{a:#?}"
        );
        assert_eq!(a.open.len(), 1);
        assert!(a.open[0].unit_state_here.is_none());
        let text = render_colony(&view);
        assert!(text.contains("unit states not observable here"), "{text}");
        assert!(text.contains("unit not observable here"), "{text}");
    }

    #[test]
    fn an_inactive_unit_is_not_waiting() {
        let (mut fixture, _) = colony();
        let inactive = |_: &str| "inactive".to_owned();
        let view = view(&mut fixture, Withhold::default(), Some(&inactive), &[]);
        let a = parent(&view, "parent-a");
        assert!(!a
            .evidence
            .iter()
            .any(|e| matches!(e, Evidence::WaitingOnZooids(_))));
        assert_eq!(a.open[0].unit_state_here.as_deref(), Some("inactive"));
    }

    #[test]
    fn spawn_unavailable_keeps_the_roster_and_goals() {
        let (mut fixture, _) = colony();
        let withhold = Withhold {
            spawn: true,
            ..Withhold::default()
        };
        let view = view(&mut fixture, withhold, None, &[]);
        assert_eq!(
            view.unavailable,
            vec![(ColonyRole::Spawn, "withheld".to_owned())]
        );
        let a = parent(&view, "parent-a");
        assert!(a.open.is_empty() && a.unreviewed.is_empty());
        assert_eq!(
            a.evidence,
            vec![Evidence::WaitingOnJp(1), Evidence::Doing(1)]
        );
        assert!(render_colony(&view).contains("unavailable: Spawn: withheld"));
    }

    #[test]
    fn message_unavailable_leaves_review_unknown() {
        let (mut fixture, _) = colony();
        let withhold = Withhold {
            message: true,
            ..Withhold::default()
        };
        let view = view(&mut fixture, withhold, None, &[]);
        assert_eq!(
            view.unavailable,
            vec![(ColonyRole::Message, "withheld".to_owned())]
        );
        let a = parent(&view, "parent-a");
        assert_eq!((a.settled, a.review_unknown), (0, 2));
        assert_eq!(a.unread, None);
        assert!(!a
            .evidence
            .iter()
            .any(|e| matches!(e, Evidence::ReviewOutstanding(_))));
        let text = render_colony(&view);
        assert!(text.contains("unknown (Message unavailable)"), "{text}");
    }

    #[test]
    fn relations_unavailable_keeps_spawners_by_id() {
        let (mut fixture, a) = colony();
        let withhold = Withhold {
            relations: true,
            ..Withhold::default()
        };
        let view = view(&mut fixture, withhold, None, &[]);
        assert_eq!(
            view.unavailable,
            vec![(ColonyRole::Relations, "withheld".to_owned())]
        );
        // Only the spawner is known, by id, and Goals tags name labels.
        assert_eq!(view.parents.len(), 1, "{view:#?}");
        let only = &view.parents[0];
        assert_eq!(only.persona, Some(a));
        assert_eq!(only.label, format!("{a:x}"));
        assert_eq!(only.unread, None);
        assert_eq!(only.review_unknown, 2);
        assert!(only.last_message.is_none());
    }

    #[test]
    fn goals_unavailable_says_nothing_about_goals() {
        let (mut fixture, _) = colony();
        let withhold = Withhold {
            goals: true,
            ..Withhold::default()
        };
        let active = |_: &str| "active".to_owned();
        let view = view(&mut fixture, withhold, Some(&active), &[]);
        assert_eq!(
            view.unavailable,
            vec![
                (ColonyRole::Goals, "withheld".to_owned()),
                (ColonyRole::GoalsStatus, "withheld".to_owned()),
            ]
        );
        let a = parent(&view, "parent-a");
        assert_eq!(
            a.evidence,
            vec![Evidence::WaitingOnZooids(1), Evidence::ReviewOutstanding(1)]
        );
    }

    #[test]
    fn a_roster_member_with_no_spawn_history_is_a_parent() {
        let mut fixture = Fixture::default();
        let (c, _) = fixture.zooid("parent-c");
        let jp = fixture.person("jp");
        let view = view(&mut fixture, Withhold::default(), None, &["jp"]);
        let c_view = parent(&view, "parent-c");
        assert_eq!(c_view.persona, Some(c));
        assert_eq!(c_view.shown_because, vec![ParentSource::Roster]);
        assert!(c_view.evidence.is_empty());
        assert_eq!(c_view.unread, Some(0));
        let jp_view = parent(&view, "jp");
        assert_eq!(jp_view.persona, Some(jp));
        assert_eq!(jp_view.shown_because, vec![ParentSource::Asked]);
        let text = render_colony(&view);
        assert!(text.contains("parent-c"), "{text}");
        assert!(text.contains("state: unknown"), "{text}");
    }

    #[test]
    fn silence_is_unknown_never_idle() {
        let mut fixture = Fixture::default();
        fixture.zooid("quiet");
        let withhold = Withhold {
            message: true,
            goals: true,
            ..Withhold::default()
        };
        let view = view(&mut fixture, withhold, None, &[]);
        let text = render_colony(&view);
        assert!(!text.to_lowercase().contains("idle"), "{text}");
        assert!(text.contains("state: unknown"), "{text}");
        assert!(text.contains("unread: unknown"), "{text}");
    }

    #[test]
    fn the_roster_is_live_resident_zooids_only() {
        let mut fixture = Fixture::default();
        let (resident, _) = fixture.zooid("parent-a");
        let (gone, gone_lifecycle) = fixture.zooid("parent-r");
        fixture.retire(gone, gone_lifecycle);
        fixture.person("contact-x");
        // A task zooid carries the affinity too, but shows under its
        // spawner, not as a parent of its own.
        let (worker, _) = fixture.zooid("worker-b");
        fixture.run_as("a task", resident, 10, worker);
        let view = view(&mut fixture, Withhold::default(), None, &[]);
        let labels: Vec<&str> = view.parents.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, vec!["parent-a"], "{view:#?}");
        let a = parent(&view, "parent-a");
        assert_eq!(
            a.shown_because,
            vec![ParentSource::Spawner, ParentSource::Roster]
        );
        assert_eq!(a.open.len(), 1);
    }
}
