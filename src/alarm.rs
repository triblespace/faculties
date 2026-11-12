//! An alarm that wakes JP physically, and the ledger of what was done about
//! it (compass ac4e2b87; the design is in the alarm-faculty-design lane).
//!
//! Every record is one self-contained intrinsic entity: a raise, and the
//! acks, resolves, notes and steps that name it through `alarm::of`. An exact
//! retry builds the same fragment and so converges on the same entity. That
//! is idempotence and nothing more: nothing here computes a record id in
//! order to find one, and every reader would still work if the ids were
//! random.
//!
//! Reads are typed queries at the point of use, over whatever facts the
//! caller holds. An alarm is open exactly while no ack or resolve names it,
//! which is monotone: a union of piles can close an alarm, never reopen one.
//! A fact this build does not model is ignored, and an optional field that is
//! absent leaves the record listed.
//!
//! The escalation ladder ([`ladder`]) and the credibility guards
//! ([`guards`]) are pure functions over these readers and an explicit clock;
//! the follower that executes the ladder is a separate step.

pub mod ack;
pub mod cli;
pub mod guards;
pub mod ladder;
pub mod mcp;
pub mod operations;
pub mod render;
pub mod rungs;

pub use operations::{Alarm, Clock, RaiseRequest, Raised, SystemClock};

use anyhow::{bail, Context, Result};
use hifitime::{Duration, Epoch};
use triblespace::core::blob::encodings::succinctarchive::Rank9AcceleratedSuccinctArchiveBlob;
use triblespace::core::collection::Collection;
use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::StoreRead;
use triblespace::prelude::*;

use crate::schemas::alarm::{
    alarm, CLASSES, KIND_ACK, KIND_ALARM, KIND_FALSE_ALARM, KIND_NOTE, KIND_RESOLVE, KIND_STEP,
    KIND_STEP_DELIVERED, KIND_STEP_FAILED, KIND_TEST, RUNGS, VIAS,
};
use crate::schemas::discord::discord as discord_attrs;
use crate::schemas::message::local as message;
use crate::schemas::quota::quota as quota_attrs;
use crate::storage::FactArchive;

pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;

/// Nanoseconds in a second and a minute, the ladder's units.
pub const SECOND: i128 = 1_000_000_000;
pub const MINUTE: i128 = 60 * SECOND;

/// Encode a TAI nanosecond coordinate as a point-valued interval.
pub fn point_at(tai_ns: i128) -> Result<IntervalValue> {
    crate::clock::point(Epoch::from_tai_duration(Duration::from_total_nanoseconds(
        tai_ns,
    )))
}

/// The handle a text is stored under: a value, compared by value.
pub fn text_handle(text: &str) -> TextHandle {
    let blob: Blob<blobencodings::UTF8String> = text.to_owned().to_blob();
    blob.get_handle()
}

fn name_in(table: &[(Id, &'static str)], id: Id) -> Option<&'static str> {
    table
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, name)| *name)
}

/// This build's name for a class id, if it raises that class.
pub fn class_name(class: Id) -> Option<&'static str> {
    name_in(&CLASSES, class)
}

/// This build's name for a rung id, if it runs that rung.
pub fn rung_name(rung: Id) -> Option<&'static str> {
    name_in(&RUNGS, rung)
}

/// This build's name for a via id, if it writes that channel.
pub fn via_name(via: Id) -> Option<&'static str> {
    name_in(&VIAS, via)
}

/// The class this build raises under `name`, case-insensitively.
pub fn class_id(name: &str) -> Result<Id> {
    let wanted = name.trim().to_lowercase();
    match CLASSES.iter().find(|(_, known)| *known == wanted) {
        Some((id, _)) => Ok(*id),
        None => bail!(
            "unknown alarm class {name:?}; this build raises {}",
            CLASSES
                .iter()
                .map(|(_, known)| *known)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// One raise as the raiser supplies it, before anything is stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Raise {
    /// Relations person who raises it.
    pub raiser: Id,
    /// Relations person it is for (JP).
    pub for_person: Id,
    pub class: Id,
    pub host: String,
    /// TAI nanoseconds.
    pub raised_at: i128,
    /// One line: what is wrong.
    pub what: String,
    /// What is lost, and by when.
    pub decays: String,
    /// What was measured.
    pub evidence: String,
    pub incident: Option<String>,
    pub test: bool,
}

fn required(text: &str, field: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        bail!("{field} is empty");
    }
    Ok(text.to_owned())
}

fn optional(text: Option<&str>, field: &str) -> Result<Option<String>> {
    text.map(|text| required(text, field)).transpose()
}

/// Writer-side checks on a raise. Readers never apply these.
pub fn validate_raise(raise: &Raise) -> Result<()> {
    if class_name(raise.class).is_none() {
        bail!("class {:x} is not one this build raises", raise.class);
    }
    required(&raise.what, "what")?;
    required(&raise.decays, "decays (what is lost, by when)")?;
    required(&raise.evidence, "evidence (what you measured)")?;
    required(&raise.host, "host")?;
    optional(raise.incident.as_deref(), "incident")?;
    Ok(())
}

fn name_fact(fragment: &mut Fragment, named: Id, name: &str) {
    let name: TextHandle = fragment.put(name.to_owned());
    *fragment += entity! { ExclusiveId::force_ref(&named) @ metadata::name: name };
}

/// Build one raise record. Returns the fragment and the alarm's intrinsic
/// id. The class entity's name travels with it (never part of identity).
pub fn raise_fragment(raise: &Raise) -> Result<(Fragment, Id)> {
    validate_raise(raise)?;
    let mut fragment = Fragment::empty();
    let what: TextHandle = fragment.put(required(&raise.what, "what")?);
    let decays: TextHandle = fragment.put(required(&raise.decays, "decays")?);
    let evidence: TextHandle = fragment.put(required(&raise.evidence, "evidence")?);
    let host: TextHandle = fragment.put(required(&raise.host, "host")?);
    let incident: Option<TextHandle> =
        optional(raise.incident.as_deref(), "incident")?.map(|incident| fragment.put(incident));
    let raised_at = point_at(raise.raised_at)?;
    let mut tags = vec![KIND_ALARM, raise.class];
    if raise.test {
        tags.push(KIND_TEST);
    }
    let record = entity! {
        metadata::tag*: tags,
        message::from: raise.raiser,
        alarm::for_person: raise.for_person,
        quota_attrs::host: host,
        metadata::created_at: raised_at,
        metadata::description: what,
        alarm::decays: decays,
        alarm::evidence: evidence,
        alarm::incident?: incident,
    };
    let id = record.root().expect("a raise has one intrinsic root");
    fragment += record;
    name_fact(
        &mut fragment,
        raise.class,
        class_name(raise.class).expect("validated class"),
    );
    Ok((fragment, id))
}

/// One acknowledgement: the alarm was seen.
#[derive(Clone, Debug, PartialEq)]
pub struct Ack {
    pub alarm: Id,
    /// JP, or the zooid relaying his word with [`crate::schemas::alarm::VIA_RELAYED`].
    pub by: Id,
    pub via: Id,
    /// TAI nanoseconds.
    pub at: i128,
    /// The Discord message anchor of the JP message that acked.
    pub discord_message: Option<Id>,
    /// Where JP said it, for a relayed ack.
    pub note: Option<String>,
}

pub fn ack_fragment(ack: &Ack) -> Result<(Fragment, Id)> {
    let Some(via) = via_name(ack.via) else {
        bail!("via {:x} is not one this build writes", ack.via);
    };
    let mut fragment = Fragment::empty();
    let note: Option<TextHandle> =
        optional(ack.note.as_deref(), "ack note")?.map(|note| fragment.put(note));
    let at = point_at(ack.at)?;
    let record = entity! {
        metadata::tag: &KIND_ACK,
        alarm::of: ack.alarm,
        message::from: ack.by,
        alarm::via: ack.via,
        metadata::created_at: at,
        discord_attrs::message?: ack.discord_message,
        metadata::description?: note,
    };
    let id = record.root().expect("an ack has one intrinsic root");
    fragment += record;
    name_fact(&mut fragment, ack.via, via);
    Ok((fragment, id))
}

/// One resolve: the cause was dealt with, or it was a false alarm.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolve {
    pub alarm: Id,
    pub by: Id,
    /// TAI nanoseconds.
    pub at: i128,
    pub reason: String,
    pub false_alarm: bool,
}

pub fn resolve_fragment(resolve: &Resolve) -> Result<(Fragment, Id)> {
    let mut fragment = Fragment::empty();
    let reason: TextHandle = fragment.put(required(&resolve.reason, "resolve reason")?);
    let at = point_at(resolve.at)?;
    let mut tags = vec![KIND_RESOLVE];
    if resolve.false_alarm {
        tags.push(KIND_FALSE_ALARM);
    }
    let record = entity! {
        metadata::tag*: tags,
        alarm::of: resolve.alarm,
        message::from: resolve.by,
        metadata::created_at: at,
        metadata::description: reason,
    };
    let id = record.root().expect("a resolve has one intrinsic root");
    fragment += record;
    Ok((fragment, id))
}

/// One note: news about an alarm that does not restart its ladder.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub alarm: Id,
    pub by: Id,
    /// TAI nanoseconds.
    pub at: i128,
    pub text: String,
}

pub fn note_fragment(note: &Note) -> Result<(Fragment, Id)> {
    let mut fragment = Fragment::empty();
    let text: TextHandle = fragment.put(required(&note.text, "note")?);
    let at = point_at(note.at)?;
    let record = entity! {
        metadata::tag: &KIND_NOTE,
        alarm::of: note.alarm,
        message::from: note.by,
        metadata::created_at: at,
        metadata::description: text,
    };
    let id = record.root().expect("a note has one intrinsic root");
    fragment += record;
    Ok((fragment, id))
}

/// How a step's action ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Delivered,
    /// The error, as the executor reports it.
    Failed(String),
}

/// One rung that ran, recorded after the action.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub alarm: Id,
    pub rung: Id,
    pub host: String,
    /// TAI nanoseconds.
    pub started: i128,
    /// TAI nanoseconds.
    pub finished: i128,
    pub outcome: Outcome,
    /// The Discord message anchor the step posted.
    pub discord_message: Option<Id>,
    /// The Discord snowflake of the message the step posted, as Discord's
    /// send receipt names it (`discord::message_id`, the value the message's
    /// anchor carries). Ack detection joins a reply to a step on this value,
    /// so it never computes an anchor id to look one up.
    pub discord_message_id: Option<String>,
    /// The say request a voice step published.
    pub say_request: Option<Id>,
}

pub fn step_fragment(step: &Step) -> Result<(Fragment, Id)> {
    let Some(rung) = rung_name(step.rung) else {
        bail!("rung {:x} is not one this build runs", step.rung);
    };
    if step.finished < step.started {
        bail!("a step cannot finish before it started");
    }
    let mut fragment = Fragment::empty();
    let host: TextHandle = fragment.put(required(&step.host, "host")?);
    let (outcome, error) = match &step.outcome {
        Outcome::Delivered => (KIND_STEP_DELIVERED, None),
        Outcome::Failed(error) => (
            KIND_STEP_FAILED,
            Some(fragment.put::<blobencodings::UTF8String, _>(required(error, "step error")?)),
        ),
    };
    let snowflake: Option<TextHandle> = match step.discord_message_id.as_deref() {
        Some(snowflake) => {
            crate::discord::validate_snowflake(snowflake).context("a step's Discord message id")?;
            Some(fragment.put(snowflake.to_owned()))
        }
        None => None,
    };
    let started = point_at(step.started)?;
    let finished = point_at(step.finished)?;
    let record = entity! {
        metadata::tag*: [KIND_STEP, outcome],
        alarm::of: step.alarm,
        alarm::rung: step.rung,
        quota_attrs::host: host,
        metadata::started_at: started,
        metadata::finished_at: finished,
        metadata::description?: error,
        discord_attrs::message?: step.discord_message,
        discord_attrs::message_id?: snowflake,
        discord_attrs::say_request?: step.say_request,
    };
    let id = record.root().expect("a step has one intrinsic root");
    fragment += record;
    name_fact(&mut fragment, step.rung, rung);
    Ok((fragment, id))
}

/// Whether `alarm` is raised and nothing has closed it: no ack and no
/// resolve names it. A closing record needs nothing but its kind and its
/// `alarm::of` to close; anything else on it is evidence, not a condition.
pub fn is_open<P>(facts: &P, alarm: Id) -> bool
where
    P: TriblePattern + ?Sized,
{
    exists!(pattern!(facts, [{ alarm @ metadata::tag: &KIND_ALARM }])) && !is_closed(facts, alarm)
}

/// Whether an ack or a resolve names `alarm`.
pub fn is_closed<P>(facts: &P, alarm: Id) -> bool
where
    P: TriblePattern + ?Sized,
{
    exists!(pattern!(facts, [{ _?record @ metadata::tag: &KIND_ACK, alarm::of: alarm }]))
        || exists!(pattern!(facts, [{ _?record @ metadata::tag: &KIND_RESOLVE, alarm::of: alarm }]))
}

/// The earliest time a dated ack or resolve gives for closing `alarm`.
/// `None` when it is open, or closed only by undated records.
pub fn closed_at<P>(facts: &P, alarm: Id) -> Option<i128>
where
    P: TriblePattern + ?Sized,
{
    let acks = find!(
        at: Lower,
        pattern!(facts, [{ _?record @
            metadata::tag: &KIND_ACK,
            alarm::of: alarm,
            metadata::created_at: ?at,
        }])
    );
    let resolves = find!(
        at: Lower,
        pattern!(facts, [{ _?record @
            metadata::tag: &KIND_RESOLVE,
            alarm::of: alarm,
            metadata::created_at: ?at,
        }])
    );
    acks.chain(resolves).map(|at| at.0).min()
}

/// One step as read back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StepRow {
    pub step: Id,
    pub rung: Id,
    /// TAI nanoseconds (the interval's lower bound).
    pub started: i128,
    pub delivered: bool,
    pub failed: bool,
}

/// Every step about `alarm` that names a rung and a start, earliest first.
/// The outcome is reported as found: a step carrying neither outcome tag
/// still lists, and one carrying both shows both.
pub fn steps<P>(facts: &P, alarm: Id) -> Vec<StepRow>
where
    P: TriblePattern + ?Sized,
{
    let mut rows: Vec<StepRow> = find!(
        (step: Id, rung: Id, started: Lower),
        pattern!(facts, [{ ?step @
            metadata::tag: &KIND_STEP,
            alarm::of: alarm,
            alarm::rung: ?rung,
            metadata::started_at: ?started,
        }])
    )
    .map(|(step, rung, started)| StepRow {
        step,
        rung,
        started: started.0,
        delivered: exists!(pattern!(facts, [{ step @ metadata::tag: &KIND_STEP_DELIVERED }])),
        failed: exists!(pattern!(facts, [{ step @ metadata::tag: &KIND_STEP_FAILED }])),
    })
    .collect();
    rows.sort_by_key(|row| (row.started, row.step));
    rows
}

/// One alarm as `list` shows it. Text fields stay handles: the caller reads
/// the blobs it renders, and one that has not arrived renders as missing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlarmRow {
    pub alarm: Id,
    /// TAI nanoseconds: the earliest raise time the record carries.
    pub raised: i128,
    /// Every tag but the alarm and test kinds: its class, or classes a newer
    /// build knows.
    pub classes: Vec<Id>,
    pub test: bool,
    pub raisers: Vec<Id>,
    pub what: Vec<TextHandle>,
    pub incident: Vec<TextHandle>,
    pub open: bool,
}

/// Every raise that carries a time, newest first.
pub fn list<P>(facts: &P) -> Vec<AlarmRow>
where
    P: TriblePattern + ?Sized,
{
    let mut raised: Vec<(Id, i128)> = find!(
        (alarm: Id, at: Lower),
        pattern!(facts, [{ ?alarm @ metadata::tag: &KIND_ALARM, metadata::created_at: ?at }])
    )
    .map(|(alarm, at)| (alarm, at.0))
    .collect();
    // Earliest time per alarm: sort by (alarm, time) and keep the first.
    raised.sort();
    raised.dedup_by_key(|(alarm, _)| *alarm);
    let mut rows: Vec<AlarmRow> = raised
        .into_iter()
        .map(|(alarm, raised)| {
            let mut classes: Vec<Id> = find!(
                tag: Id,
                pattern!(facts, [{ alarm @ metadata::tag: ?tag }])
            )
            .filter(|tag| *tag != KIND_ALARM && *tag != KIND_TEST)
            .collect();
            classes.sort();
            let mut raisers: Vec<Id> = find!(
                raiser: Id,
                pattern!(facts, [{ alarm @ message::from: ?raiser }])
            )
            .collect();
            raisers.sort();
            let mut what: Vec<TextHandle> = find!(
                what: TextHandle,
                pattern!(facts, [{ alarm @ metadata::description: ?what }])
            )
            .collect();
            what.sort();
            let mut incident: Vec<TextHandle> = find!(
                incident: TextHandle,
                pattern!(facts, [{ alarm @ alarm::incident: ?incident }])
            )
            .collect();
            incident.sort();
            AlarmRow {
                alarm,
                raised,
                classes,
                test: exists!(pattern!(facts, [{ alarm @ metadata::tag: &KIND_TEST }])),
                raisers,
                what,
                incident,
                open: !is_closed(facts, alarm),
            }
        })
        .collect();
    rows.sort_by(|left, right| (right.raised, right.alarm).cmp(&(left.raised, left.alarm)));
    rows
}

/// The facts one frozen snapshot holds for the alarm collection, and how
/// many of its selected commits it could not read.
///
/// A commit can arrive before its member blobs. Such a commit is left out
/// and counted, never an error, so a follower polling every few seconds
/// picks it up on a later read once sync brings the bytes. That makes every
/// answer here provisional in one direction: an ack still in flight leaves
/// its alarm reading open (the ladder keeps sounding a little longer), and a
/// raise still in flight would not be counted by the guards. So this read
/// is for the follower and for display; [`guards::decide`] is fed a complete
/// observation instead (see the guards' module doc).
pub struct Observed {
    pub facts: FactArchive,
    pub unread: usize,
}

pub fn observe<R>(
    snapshot: &R,
    rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
) -> Result<Observed>
where
    R: StoreRead,
{
    let attached = snapshot
        .attached(rank9)
        .context("attach the Alarm fact cover")?;
    let read = crate::storage::attached_facts_read(&attached)?;
    let unread = read.unread().len();
    Ok(Observed {
        facts: read.into_value(),
        unread,
    })
}

#[cfg(test)]
mod tests;
