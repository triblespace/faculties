//! Alarm operations over one pile.
//!
//! Every call takes one frozen observation, decides from it, and publishes
//! each record as one self-contained commit. Rungs run BETWEEN store borrows:
//! a rung's transport may open the same pile itself (the Discord send and the
//! colony message do), so no store is held while one runs.
//!
//! `raise` feeds [`guards::decide`] a COMPLETE read of the alarm collection
//! (`storage::acquire_facts`): an observation that cannot read every selected
//! commit is retried a few times and then refused, never decided on, because
//! a raise in flight would slip a rate limit or start a second ladder. Even a
//! complete read serialises raises only on this host's view of the pile: two
//! hosts that have not synced each other's raises, or two processes deciding
//! at the same instant, can both start a ladder. Cross-host serialisation is
//! best-effort, and two such raises are two open alarms with a ladder each.
//! Display ([`Alarm::list`], [`Alarm::show`]) and the follower read
//! tolerantly and say how much they could not read.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{CollectionCommit, CollectionStoreExt};
use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::*;

use super::ack::{self, Detected};
use super::guards::{self, Decision};
use super::ladder::{self, Due, Next};
use super::rungs::{RungRunner, Subject};
use super::{
    ack_fragment, class_id, class_name, is_closed, note_fragment, raise_fragment, resolve_fragment,
    rung_name, step_fragment, steps, via_name, Ack, Note, Outcome, Raise, Resolve, Step, StepRow,
    TextHandle, SECOND,
};
use crate::relations::{self, Head};
use crate::schemas::alarm::{
    alarm as attrs, CLASSES, DEFAULT_SCOPE_ID, KIND_ACK, KIND_ALARM, KIND_FALSE_ALARM, KIND_NOTE,
    KIND_RESOLVE, KIND_TEST, VIA_RELAYED,
};
use crate::schemas::discord::DEFAULT_SCOPE_ID as DISCORD_SCOPE_ID;
use crate::schemas::message::local as message;
use crate::schemas::quota::quota as quota_attrs;
use crate::schemas::relations::DEFAULT_SCOPE_ID as RELATIONS_SCOPE_ID;
use crate::storage::{AcquiringReader, FactArchive, FacultySnapshot, FacultyStore, Storage};

/// The frozen reader every text of one observation is read through.
pub type Reader = AcquiringReader<FacultySnapshot>;

/// The affinity a Relations person must carry to raise an alarm.
pub const ZOOID_AFFINITY: &str = "zooid";

/// How often `raise` retries an observation it could not read completely,
/// and how long it waits between tries.
pub const COMPLETE_READ_ATTEMPTS: usize = 3;
pub const COMPLETE_READ_PAUSE: i128 = 2 * SECOND;

/// The clock every operation that dates a record asks, so a test can run a
/// whole ladder on a fake one. TAI nanoseconds.
pub trait Clock {
    fn now(&mut self) -> Result<i128>;
    /// Return at `at`, or earlier; the caller reads the clock again.
    fn sleep_until(&mut self, at: i128) -> Result<()>;
}

/// The machine's clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&mut self) -> Result<i128> {
        crate::clock::tai_nanoseconds_now()
    }

    fn sleep_until(&mut self, at: i128) -> Result<()> {
        let now = self.now()?;
        if at > now {
            let nanos = u64::try_from(at - now).unwrap_or(u64::MAX);
            std::thread::sleep(std::time::Duration::from_nanos(nanos));
        }
        Ok(())
    }
}

/// Configured Alarm operations. Inputs are literal: who raises, acks or
/// resolves is always passed in, never read from the environment here.
#[derive(Clone, Debug)]
pub struct Alarm {
    storage: Storage,
}

/// One raise as a caller asks for it, with people as Relations selectors.
#[derive(Clone, Debug)]
pub struct RaiseRequest<'a> {
    /// A live zooid: an active Relations person with the zooid affinity.
    pub raiser: &'a str,
    /// Whom it wakes: an active Relations person.
    pub for_person: &'a str,
    pub class: &'a str,
    pub host: &'a str,
    pub what: &'a str,
    pub decays: &'a str,
    pub evidence: &'a str,
    pub incident: Option<&'a str>,
    pub test: bool,
    /// Start a second ladder when the open one has a different class.
    pub separate: bool,
}

/// What one rung that ran recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct StepReport {
    pub step: Id,
    pub rung: Id,
    pub outcome: Outcome,
}

/// What became of a raise.
#[derive(Clone, Debug, PartialEq)]
pub enum Raised {
    /// A new alarm, and the rungs of R0 that ran at once.
    Alarm {
        alarm: Id,
        subject: Subject,
        steps: Vec<StepReport>,
    },
    /// The raise was folded into an open or recent alarm as a note.
    Noted { on: Id, note: Id, why: String },
}

/// What a raise would do, written nowhere.
#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub decision: Decision,
    pub subject: Subject,
    /// The ladder this alarm would run: (offset since raise, rung).
    pub ladder: Vec<(i128, Id)>,
}

/// One alarm as `list` shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Listed {
    pub alarm: Id,
    pub raised: i128,
    pub classes: Vec<String>,
    pub test: bool,
    pub raisers: Vec<String>,
    pub what: Vec<String>,
    pub open: bool,
    pub closed_at: Option<i128>,
    pub steps: usize,
    pub failed_steps: usize,
    /// An ack JP left on Discord that nothing has recorded yet: its via.
    pub pending_ack: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListView {
    pub now: i128,
    pub alarms: Vec<Listed>,
    /// Real alarms resolved as false in the last 7 days, per raiser.
    pub false_alarms: Vec<(String, usize)>,
    /// Commits of the alarm collection this observation could not read.
    pub unread: usize,
}

/// One record about an alarm, as `show` lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub at: i128,
    /// ack, resolve, false-alarm resolve, note.
    pub kind: &'static str,
    pub by: String,
    pub via: Option<String>,
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepLine {
    pub started: i128,
    pub rung: String,
    pub delivered: bool,
    pub failed: bool,
    pub host: Vec<String>,
    pub error: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShowView {
    pub subject: Subject,
    pub open: bool,
    pub incident: Vec<String>,
    pub steps: Vec<StepLine>,
    pub events: Vec<Event>,
    pub unread: usize,
}

/// What the follower reads on each turn.
#[derive(Clone, Debug, PartialEq)]
pub struct FollowState {
    pub open: bool,
    pub steps: Vec<StepRow>,
    /// An ack JP left on Discord that nothing recorded yet.
    pub detected: Option<Detected>,
    pub jp_in_voice: bool,
}

/// A written ack, resolve or note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recorded {
    pub alarm: Id,
    pub record: Id,
    /// The alarm was already closed before this record.
    pub was_closed: bool,
}

/// One frozen observation: the alarm facts, the Relations facts, and the
/// reader their texts are read through.
pub(crate) struct View {
    pub alarm: FactArchive,
    pub relations: FactArchive,
    /// The Discord facts, when the call reads them (ack detection).
    pub discord: Option<FactArchive>,
    pub reader: Reader,
    pub unread: usize,
}

impl Alarm {
    pub fn new(pile: std::path::PathBuf, key: Option<std::path::PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Resolve and check everything a raise needs, then decide, without
    /// writing anything.
    pub fn preview(&self, request: &RaiseRequest<'_>, clock: &mut dyn Clock) -> Result<Preview> {
        let now = clock.now()?;
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe_complete(&self.storage, pile, signer, runtime, clock)?;
            let (draft, mut subject) = draft(&view, request, now)?;
            let decision = guards::decide(&view.alarm, &draft, request.separate, now);
            // The id the raise would be written under, for display only.
            if let Ok((_, alarm)) = raise_fragment(&draft) {
                subject.alarm = alarm;
            }
            let ladder = ladder::schedule()
                .into_iter()
                .filter(|(at, rung)| !draft.test || (*at == 0 && ladder::R0.contains(rung)))
                .collect();
            Ok(Preview {
                decision,
                subject,
                ladder,
            })
        })
    }

    /// Raise an alarm: decide on a complete observation, publish the raise,
    /// then run R0 through `runner` and record each rung as a step. A raise
    /// the guards fold into another alarm becomes a note on it and runs no
    /// rung. The follower that runs the rest of the ladder is the caller's to
    /// start.
    pub fn raise(
        &self,
        request: &RaiseRequest<'_>,
        runner: &mut dyn RungRunner,
        clock: &mut dyn Clock,
    ) -> Result<Raised> {
        let now = clock.now()?;
        let decided = self.storage.with_store(|pile, signer, runtime| {
            let view = observe_complete(&self.storage, pile, signer, runtime, clock)?;
            let (draft, subject) = draft(&view, request, now)?;
            match guards::decide(&view.alarm, &draft, request.separate, now) {
                Decision::Refuse(why) => bail!("alarm not raised: {why}"),
                Decision::Note { on, why } => {
                    let text = format!(
                        "raised again by {} on {}: {} Decays: {} Evidence: {}",
                        subject.raiser_label, draft.host, draft.what, draft.decays, draft.evidence
                    );
                    let (fragment, note) = note_fragment(&Note {
                        alarm: on,
                        by: draft.raiser,
                        at: now,
                        text,
                    })?;
                    commit_alarm(&self.storage, pile, signer, runtime, fragment)?;
                    Ok(Err(Raised::Noted { on, note, why }))
                }
                Decision::Raise => {
                    let (fragment, alarm) = raise_fragment(&draft)?;
                    commit_alarm(&self.storage, pile, signer, runtime, fragment)?;
                    let subject = Subject { alarm, ..subject };
                    Ok(Ok(subject))
                }
            }
        })?;
        let subject = match decided {
            Err(noted) => return Ok(noted),
            Ok(subject) => subject,
        };
        let capabilities = runner.capabilities(&subject);
        let due = match ladder::next_action(
            subject.raised_at,
            now,
            &[],
            true,
            subject.test,
            &capabilities,
        ) {
            Next::Run(due) => due,
            _ => Vec::new(),
        };
        let steps = self.run_rungs(&subject, &due, runner, clock)?;
        Ok(Raised::Alarm {
            alarm: subject.alarm,
            subject,
            steps,
        })
    }

    /// Run each due rung and record it after it ran, one commit per step.
    /// A rung that fails is recorded failed and the next one still runs.
    pub fn run_rungs(
        &self,
        subject: &Subject,
        due: &[Due],
        runner: &mut dyn RungRunner,
        clock: &mut dyn Clock,
    ) -> Result<Vec<StepReport>> {
        let mut reports = Vec::new();
        for due in due {
            let started = clock.now()?;
            let result = runner.run(subject, *due);
            let finished = clock.now()?.max(started);
            let (fragment, step) = step_fragment(&Step {
                alarm: subject.alarm,
                rung: due.rung,
                host: runner.host().to_owned(),
                started,
                finished,
                outcome: result.outcome.clone(),
                discord_message: None,
                discord_message_id: result.discord_message_id.clone(),
                say_request: result.say_request,
            })?;
            self.storage.with_store(|pile, signer, runtime| {
                commit_alarm(&self.storage, pile, signer, runtime, fragment)
            })?;
            reports.push(StepReport {
                step,
                rung: due.rung,
                outcome: result.outcome,
            });
        }
        Ok(reports)
    }

    /// Record that `alarm` was seen. `by` is an active Relations person: JP,
    /// or the zooid relaying his word with [`VIA_RELAYED`], which needs
    /// `evidence` saying where he said it.
    pub fn ack(
        &self,
        alarm: &str,
        by: &str,
        via: Id,
        evidence: Option<&str>,
        clock: &mut dyn Clock,
    ) -> Result<Recorded> {
        if via_name(via).is_none() {
            bail!("via {via:x} is not one this build writes");
        }
        let evidence = evidence.map(str::trim).filter(|text| !text.is_empty());
        if via == VIA_RELAYED && evidence.is_none() {
            bail!("a relayed ack needs --evidence: JP's words, and where he said them");
        }
        let at = clock.now()?;
        self.record(alarm, by, |alarm, by| {
            ack_fragment(&Ack {
                alarm,
                by,
                via,
                at,
                discord_message: None,
                note: evidence.map(str::to_owned),
            })
        })
    }

    /// Record that the cause was dealt with, or that it was a false alarm.
    pub fn resolve(
        &self,
        alarm: &str,
        by: &str,
        reason: &str,
        false_alarm: bool,
        clock: &mut dyn Clock,
    ) -> Result<Recorded> {
        let at = clock.now()?;
        self.record(alarm, by, |alarm, by| {
            resolve_fragment(&Resolve {
                alarm,
                by,
                at,
                reason: reason.to_owned(),
                false_alarm,
            })
        })
    }

    /// Attach news to an alarm without restarting its ladder.
    pub fn note(
        &self,
        alarm: &str,
        by: &str,
        text: &str,
        clock: &mut dyn Clock,
    ) -> Result<Recorded> {
        let at = clock.now()?;
        self.record(alarm, by, |alarm, by| {
            note_fragment(&Note {
                alarm,
                by,
                at,
                text: text.to_owned(),
            })
        })
    }

    fn record(
        &self,
        alarm: &str,
        by: &str,
        build: impl FnOnce(Id, Id) -> Result<(Fragment, Id)>,
    ) -> Result<Recorded> {
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe(&self.storage, pile, signer, runtime, false, false)?;
            let alarm = resolve_alarm(&view.alarm, alarm)?;
            let by = resolve_active(&view, by)?;
            let was_closed = is_closed(&view.alarm, alarm);
            let (fragment, record) = build(alarm, by)?;
            commit_alarm(&self.storage, pile, signer, runtime, fragment)?;
            Ok(Recorded {
                alarm,
                record,
                was_closed,
            })
        })
    }

    /// Every alarm, newest first; `open_only` keeps the open ones.
    /// With JP's Discord user, an open alarm he acked on Discord that
    /// nothing recorded yet says so.
    pub fn list(
        &self,
        open_only: bool,
        jp_user: Option<&str>,
        clock: &mut dyn Clock,
    ) -> Result<ListView> {
        let now = clock.now()?;
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe(
                &self.storage,
                pile,
                signer,
                runtime,
                false,
                jp_user.is_some(),
            )?;
            let facts = &view.alarm;
            let mut raisers: Vec<Id> = Vec::new();
            let alarms = super::list(facts)
                .into_iter()
                .filter(|row| !open_only || row.open)
                .map(|row| {
                    for raiser in &row.raisers {
                        if !raisers.contains(raiser) {
                            raisers.push(*raiser);
                        }
                    }
                    let rows = steps(facts, row.alarm);
                    Listed {
                        alarm: row.alarm,
                        raised: row.raised,
                        classes: row
                            .classes
                            .iter()
                            .map(|class| {
                                class_name(*class)
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| format!("{class:x}"))
                            })
                            .collect(),
                        test: row.test,
                        raisers: row
                            .raisers
                            .iter()
                            .map(|raiser| person_label(&view, *raiser))
                            .collect(),
                        what: row
                            .what
                            .iter()
                            .map(|what| text_or_missing(&view.reader, *what))
                            .collect(),
                        open: row.open,
                        closed_at: super::closed_at(facts, row.alarm),
                        steps: rows.len(),
                        failed_steps: rows.iter().filter(|step| step.failed).count(),
                        pending_ack: match (row.open, jp_user, &view.discord) {
                            (true, Some(jp), Some(discord)) => {
                                ack::detect(facts, discord, &view.reader, row.alarm, jp)
                                    .and_then(|detected| via_name(detected.via))
                            }
                            _ => None,
                        },
                    }
                })
                .collect();
            let false_alarms = raisers
                .into_iter()
                .map(|raiser| {
                    (
                        person_label(&view, raiser),
                        guards::false_alarms(facts, raiser, now),
                    )
                })
                .filter(|(_, count)| *count > 0)
                .collect();
            Ok(ListView {
                now,
                alarms,
                false_alarms,
                unread: view.unread,
            })
        })
    }

    /// One alarm with its steps and every record about it.
    pub fn show(&self, alarm: &str) -> Result<ShowView> {
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe(&self.storage, pile, signer, runtime, false, false)?;
            let alarm = resolve_alarm(&view.alarm, alarm)?;
            show_from(&view, alarm)
        })
    }

    /// The alarm an id or unique id prefix names.
    pub fn resolve_id(&self, selector: &str) -> Result<Id> {
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe(&self.storage, pile, signer, runtime, false, false)?;
            resolve_alarm(&view.alarm, selector)
        })
    }

    /// What the follower reads on each turn, from one tolerant observation.
    pub fn state(&self, alarm: Id, jp_user: Option<&str>) -> Result<FollowState> {
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe(
                &self.storage,
                pile,
                signer,
                runtime,
                false,
                jp_user.is_some(),
            )?;
            let (detected, jp_in_voice) = match (jp_user, &view.discord) {
                (Some(jp), Some(discord)) => (
                    ack::detect(&view.alarm, discord, &view.reader, alarm, jp),
                    ack::jp_in_voice(discord, jp),
                ),
                _ => (None, false),
            };
            Ok(FollowState {
                open: super::is_open(&view.alarm, alarm),
                steps: steps(&view.alarm, alarm),
                detected,
                jp_in_voice,
            })
        })
    }

    /// The open, real alarms whose newest activity (the raise or a step) is
    /// more than `stale` before `now`, and whose ladder is not over: the
    /// ones a follower that died left behind.
    pub fn orphans(&self, now: i128, stale: i128) -> Result<Vec<Id>> {
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe(&self.storage, pile, signer, runtime, false, false)?;
            let facts = &view.alarm;
            Ok(super::list(facts)
                .into_iter()
                .filter(|row| row.open && !row.test)
                .filter_map(|row| {
                    let rows = steps(facts, row.alarm);
                    let newest = rows
                        .iter()
                        .map(|step| step.started)
                        .max()
                        .map_or(row.raised, |started| started.max(row.raised));
                    let over = ladder::next_action(
                        row.raised,
                        now,
                        &rows,
                        true,
                        false,
                        &ladder::Capabilities::default(),
                    ) == Next::Stop;
                    (now - newest > stale && !over).then_some(row.alarm)
                })
                .collect())
        })
    }

    /// The ack JP left on Discord for `alarm`, if any, read tolerantly.
    pub fn detect_ack(&self, alarm: Id, jp_user: &str) -> Result<Option<Detected>> {
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe(&self.storage, pile, signer, runtime, false, true)?;
            Ok(view.discord.as_ref().and_then(|discord| {
                ack::detect(&view.alarm, discord, &view.reader, alarm, jp_user)
            }))
        })
    }

    /// Record a detected ack as JP's (the alarm's `for` person), unless the
    /// alarm is closed already; the ack's id when one was written.
    pub fn record_detected(&self, alarm: Id, detected: Detected) -> Result<Option<Id>> {
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe(&self.storage, pile, signer, runtime, false, false)?;
            if is_closed(&view.alarm, alarm) {
                return Ok(None);
            }
            let facts = &view.alarm;
            let by = find!(
                person: Id,
                pattern!(facts, [{ alarm @ attrs::for_person: ?person }])
            )
            .min()
            .ok_or_else(|| anyhow!("alarm {alarm:x} names nobody it is for"))?;
            let (fragment, ack) = ack_fragment(&Ack {
                alarm,
                by,
                via: detected.via,
                at: detected.at,
                discord_message: Some(detected.message),
                note: None,
            })?;
            commit_alarm(&self.storage, pile, signer, runtime, fragment)?;
            Ok(Some(ack))
        })
    }

    /// Read what a rung says about `alarm`, for the follower.
    pub fn subject(&self, alarm: Id) -> Result<Subject> {
        self.storage.with_store(|pile, signer, runtime| {
            let view = observe(&self.storage, pile, signer, runtime, false, false)?;
            subject_of(&view, alarm)
        })
    }
}

/// Resolve the people of a request and build the raise it asks for.
fn draft(view: &View, request: &RaiseRequest<'_>, now: i128) -> Result<(Raise, Subject)> {
    let class = class_id(request.class)
        .map_err(|error| anyhow!("{error:#}. Before raising: {}.", guards::THE_CHECK))?;
    let raiser = resolve_zooid(view, request.raiser)?;
    let for_person =
        resolve_active(view, request.for_person).context("the person this alarm is for")?;
    let draft = Raise {
        raiser,
        for_person,
        class,
        host: request.host.trim().to_owned(),
        raised_at: now,
        what: request.what.trim().to_owned(),
        decays: request.decays.trim().to_owned(),
        evidence: request.evidence.trim().to_owned(),
        incident: request
            .incident
            .map(str::trim)
            .filter(|incident| !incident.is_empty())
            .map(str::to_owned),
        test: request.test,
    };
    let subject = Subject {
        alarm: Id::new([0xff; 16]).expect("a nonzero placeholder id"),
        raiser,
        raiser_label: person_label(view, raiser),
        class: class_name(class).unwrap_or_default().to_owned(),
        host: draft.host.clone(),
        what: draft.what.clone(),
        decays: draft.decays.clone(),
        evidence: draft.evidence.clone(),
        test: draft.test,
        raised_at: now,
    };
    Ok((draft, subject))
}

/// The alarm an id or unique id prefix names, among the raises this
/// observation holds.
pub(crate) fn resolve_alarm(facts: &FactArchive, selector: &str) -> Result<Id> {
    let candidates = find!(
        alarm: Id,
        pattern!(facts, [{ ?alarm @ metadata::tag: &KIND_ALARM }])
    );
    crate::resolve_id_prefix(selector, candidates)
        .with_context(|| format!("no alarm matches {selector:?}"))
}

/// An active (not retired) Relations person.
pub(crate) fn resolve_active(view: &View, selector: &str) -> Result<Id> {
    relations::resolve_person(&view.reader, &view.relations, selector, false)?
        .require_unique("active person", selector)
}

/// A live zooid: an active Relations person whose current profile carries
/// the zooid affinity. Anyone else is refused as a raiser.
pub(crate) fn resolve_zooid(view: &View, selector: &str) -> Result<Id> {
    if selector.trim().is_empty() {
        bail!("no raiser: an alarm is raised by a live zooid (set PERSONA, or pass from)");
    }
    let person = resolve_active(view, selector)
        .with_context(|| format!("raiser {selector:?} is not an active Relations person"))?;
    let heads = match relations::profile_head(&view.relations, person)? {
        Head::Unique(head) => vec![head],
        Head::Forked(heads) => heads,
        Head::Missing => Vec::new(),
    };
    for head in heads {
        let profile = relations::profile_snapshot(&view.relations, head)?;
        for affinity in profile.affinities {
            if read_text(&view.reader, affinity)
                .is_some_and(|text| text.trim().eq_ignore_ascii_case(ZOOID_AFFINITY))
            {
                return Ok(person);
            }
        }
    }
    bail!("raiser {selector:?} is not a zooid: only a live zooid raises an alarm")
}

/// A person's current Relations label, or their id when it cannot be read.
pub(crate) fn person_label(view: &View, person: Id) -> String {
    relations::current_profile(&view.relations, person)
        .ok()
        .and_then(|profile| read_text(&view.reader, profile.label))
        .unwrap_or_else(|| format!("{person:x}"))
}

/// A text blob, or `None` when it is not here (or not text).
pub(crate) fn read_text<R>(reader: &R, handle: TextHandle) -> Option<String>
where
    R: BlobStoreGet + ?Sized,
{
    reader
        .get::<anybytes::View<str>, _>(handle)
        .ok()
        .map(|text| text.to_string())
}

pub(crate) fn text_or_missing<R>(reader: &R, handle: TextHandle) -> String
where
    R: BlobStoreGet + ?Sized,
{
    read_text(reader, handle).unwrap_or_else(|| "(text not here yet)".to_owned())
}

fn texts<R, I>(reader: &R, handles: I) -> Vec<String>
where
    R: BlobStoreGet + ?Sized,
    I: IntoIterator<Item = TextHandle>,
{
    let mut handles: Vec<TextHandle> = handles.into_iter().collect();
    handles.sort();
    handles.dedup();
    handles
        .into_iter()
        .map(|handle| text_or_missing(reader, handle))
        .collect()
}

/// What a rung says about `alarm`, read from the observation. A raise that
/// carries a field twice shows the values joined; one missing shows empty.
pub(crate) fn subject_of(view: &View, alarm: Id) -> Result<Subject> {
    let facts = &view.alarm;
    let raised_at = find!(
        at: Lower,
        pattern!(facts, [{ alarm @ metadata::tag: &KIND_ALARM, metadata::created_at: ?at }])
    )
    .map(|at| at.0)
    .min()
    .ok_or_else(|| anyhow!("alarm {alarm:x} has no dated raise here"))?;
    let raiser = find!(
        raiser: Id,
        pattern!(facts, [{ alarm @ message::from: ?raiser }])
    )
    .min()
    .unwrap_or(alarm);
    let joined = |handles: Vec<TextHandle>| texts(&view.reader, handles).join(" / ");
    let class = find!(
        tag: Id,
        pattern!(facts, [{ alarm @ metadata::tag: ?tag }])
    )
    .filter_map(class_name)
    .collect::<Vec<_>>()
    .join(" / ");
    Ok(Subject {
        alarm,
        raiser,
        raiser_label: person_label(view, raiser),
        class,
        host: joined(
            find!(h: TextHandle, pattern!(facts, [{ alarm @ quota_attrs::host: ?h }])).collect(),
        ),
        what: joined(
            find!(h: TextHandle, pattern!(facts, [{ alarm @ metadata::description: ?h }]))
                .collect(),
        ),
        decays: joined(
            find!(h: TextHandle, pattern!(facts, [{ alarm @ attrs::decays: ?h }])).collect(),
        ),
        evidence: joined(
            find!(h: TextHandle, pattern!(facts, [{ alarm @ attrs::evidence: ?h }])).collect(),
        ),
        test: exists!(pattern!(facts, [{ alarm @ metadata::tag: &KIND_TEST }])),
        raised_at,
    })
}

fn show_from(view: &View, alarm: Id) -> Result<ShowView> {
    let facts = &view.alarm;
    let subject = subject_of(view, alarm)?;
    let steps = steps(facts, alarm)
        .into_iter()
        .map(|row| {
            let step = row.step;
            StepLine {
                started: row.started,
                rung: rung_name(row.rung)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{:x}", row.rung)),
                delivered: row.delivered,
                failed: row.failed,
                host: texts(
                    &view.reader,
                    find!(h: TextHandle, pattern!(facts, [{ step @ quota_attrs::host: ?h }])),
                ),
                error: if row.failed {
                    texts(
                        &view.reader,
                        find!(
                            h: TextHandle,
                            pattern!(facts, [{ step @ metadata::description: ?h }])
                        ),
                    )
                } else {
                    Vec::new()
                },
            }
        })
        .collect();
    let mut events = Vec::new();
    for (kind, tag) in [
        ("ack", KIND_ACK),
        ("resolve", KIND_RESOLVE),
        ("note", KIND_NOTE),
    ] {
        for (record, by, at) in find!(
            (record: Id, by: Id, at: Lower),
            pattern!(facts, [{ ?record @
                metadata::tag: tag,
                attrs::of: alarm,
                message::from: ?by,
                metadata::created_at: ?at,
            }])
        ) {
            let false_alarm =
                exists!(pattern!(facts, [{ record @ metadata::tag: &KIND_FALSE_ALARM }]));
            let via = find!(via: Id, pattern!(facts, [{ record @ attrs::via: ?via }]))
                .map(|via| {
                    via_name(via)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("{via:x}"))
                })
                .min();
            let text = texts(
                &view.reader,
                find!(
                    h: TextHandle,
                    pattern!(facts, [{ record @ metadata::description: ?h }])
                ),
            );
            events.push(Event {
                at: at.0,
                kind: if false_alarm {
                    "false-alarm resolve"
                } else {
                    kind
                },
                by: person_label(view, by),
                via,
                text: (!text.is_empty()).then(|| text.join(" / ")),
            });
        }
    }
    events.sort_by(|left, right| (left.at, left.kind).cmp(&(right.at, right.kind)));
    let incident = texts(
        &view.reader,
        find!(h: TextHandle, pattern!(facts, [{ alarm @ attrs::incident: ?h }])),
    );
    Ok(ShowView {
        subject,
        open: !is_closed(facts, alarm),
        incident,
        steps,
        events,
        unread: view.unread,
    })
}

/// The Rank9 view of `scope`, with this key's own commits carried into it.
fn attach(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    scope: Id,
    what: &str,
) -> Result<triblespace::core::collection::Collection<Rank9AcceleratedSuccinctArchiveBlob>> {
    let source = storage.open_collection_read(pile, scope, signer.verifying_key(), runtime)?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    {
        let mut local = pile.store();
        pollster::block_on(async {
            crate::storage::tolerate_own_lag(local.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(local.maintain_attached(rank9, signer).await)
        })
        .with_context(|| format!("maintain {what} fact collection"))?;
    }
    Ok(rank9)
}

/// One observation of the alarm and Relations collections. `strict` refuses
/// an alarm observation that cannot read every selected commit; otherwise
/// the commits it could not read are counted in `unread`. Relations is
/// always read completely: a raiser is resolved against all of it.
pub(crate) fn observe(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    strict: bool,
    with_discord: bool,
) -> Result<View> {
    let alarm = attach(storage, pile, signer, runtime, DEFAULT_SCOPE_ID, "Alarm")?;
    let discord = if with_discord {
        Some(attach(
            storage,
            pile,
            signer,
            runtime,
            DISCORD_SCOPE_ID,
            "Discord",
        )?)
    } else {
        None
    };
    let relation = attach(
        storage,
        pile,
        signer,
        runtime,
        RELATIONS_SCOPE_ID,
        "Relations",
    )?;
    let reader = AcquiringReader::new(
        pile.snapshot().context("freeze the Alarm observation")?,
        Arc::clone(runtime),
    );
    let (facts, unread) = if strict {
        (
            crate::storage::acquire_facts(&reader, alarm).context("read Alarm facts")?,
            0,
        )
    } else {
        let read = crate::storage::acquire_facts_read(&reader, alarm)?;
        let unread = read.unread().len();
        (read.into_value(), unread)
    };
    let relations =
        crate::storage::acquire_facts(&reader, relation).context("read Relations facts")?;
    // Discord is read tolerantly: a message still in flight is one the next
    // read will see, and nothing here counts or ranks by absence.
    let discord = match discord {
        Some(discord) => Some(crate::storage::acquire_facts_read(&reader, discord)?.into_value()),
        None => None,
    };
    Ok(View {
        alarm: facts,
        relations,
        discord,
        reader,
        unread,
    })
}

/// [`observe`] strictly, retried while commits are in flight, then refused.
fn observe_complete(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    clock: &mut dyn Clock,
) -> Result<View> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match observe(storage, pile, signer, runtime, true, false) {
            Ok(view) => return Ok(view),
            Err(error)
                if attempt < COMPLETE_READ_ATTEMPTS
                    && error
                        .chain()
                        .any(|cause| cause.is::<crate::storage::IncompleteAttachedRead>()) =>
            {
                let now = clock.now()?;
                clock.sleep_until(now + COMPLETE_READ_PAUSE)?;
            }
            Err(error) => {
                return Err(error.context(
                    "the alarm collection could not be read completely here, so the guards \
                     cannot decide: retry, or use the manual alarm recipe",
                ))
            }
        }
    }
}

/// Publish one alarm record and carry it into the derived views.
pub(crate) fn commit_alarm(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    fragment: Fragment,
) -> Result<CollectionCommit> {
    commit_to(
        storage,
        pile,
        signer,
        runtime,
        DEFAULT_SCOPE_ID,
        "Alarm",
        fragment,
    )
}

/// Publish one fragment into `scope` and carry it into its derived views.
pub(crate) fn commit_to(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    scope: Id,
    faculty: &str,
    fragment: Fragment,
) -> Result<CollectionCommit> {
    let collection = storage.open_collection_write(pile, scope, signer.verifying_key(), runtime)?;
    crate::collection_names::require_command_write_admission_acquiring(
        pile,
        collection,
        signer,
        faculty,
        "alarm list",
        runtime,
    )?;
    let commit = pile
        .commit(collection, signer, fragment)
        .with_context(|| format!("commit {faculty} record"))?;
    {
        let mut local = pile.store();
        drop(
            pollster::block_on(crate::storage::ensure_downstream(
                &mut *local,
                collection,
                signer,
            ))
            .with_context(|| {
                format!("{faculty} record was committed, but ensuring its derived views failed")
            })?,
        );
    }
    Ok(commit)
}

/// The classes this build raises, by their CLI names.
pub fn class_names() -> Vec<&'static str> {
    CLASSES.iter().map(|(_, name)| *name).collect()
}

#[cfg(test)]
pub(crate) mod tests;
