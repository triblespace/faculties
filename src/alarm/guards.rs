//! Credibility guards: the alarm only works while it is believed, so a raise
//! that is not well-founded, or that repeats one already ringing, must not
//! start a second ladder. Pure functions over the facts and an explicit
//! clock; the CLI and MCP surfaces turn a [`Decision`] into a write.
//!
//! The order is deliberate. A raise that names no decay is refused before
//! anything else. Then a raise that repeats a known incident, repeats the
//! same text, or arrives while another ladder is running becomes a NOTE on
//! that alarm: that is always safe, so it comes before the rate limits.
//! Only a raise that would start a new ladder is rate-limited. A test raise
//! is checked for its reason and nothing else: it never mentions JP and never
//! goes past R0, so it neither joins a real alarm nor spends an allowance.

use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::prelude::*;

use super::{is_open, text_handle, validate_raise, Raise, MINUTE};
use crate::schemas::alarm::{alarm, KIND_ALARM, KIND_FALSE_ALARM, KIND_RESOLVE, KIND_TEST};
use crate::schemas::message::local as message;

pub const HOUR: i128 = 60 * MINUTE;
pub const DAY: i128 = 24 * HOUR;

/// The check every refusal quotes (wiki db1046cc).
pub const THE_CHECK: &str = "name the physical thing you measured and the semantic thing you \
     care about, and ask whether the system ever promised they would agree";

/// A raise with the same incident key joins an alarm closed this recently.
pub const INCIDENT_WINDOW: i128 = 6 * HOUR;
/// A raise with the same text, from anyone, joins an alarm raised this recently.
pub const SAME_TEXT_WINDOW: i128 = 30 * MINUTE;
/// One real raise per raiser in this window.
pub const RAISER_WINDOW: i128 = 30 * MINUTE;
/// Real raises per raiser per day, before false-alarm halving.
pub const RAISER_DAILY: u32 = 3;
/// Real raises per day, from everyone.
pub const GLOBAL_DAILY: u32 = 5;
/// How long a false alarm halves its raiser's daily allowance.
pub const FALSE_ALARM_WINDOW: i128 = 7 * DAY;

/// What becomes of a raise.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Decision {
    /// Start a new ladder.
    Raise,
    /// Attach it as a note to this alarm instead; the ladder is not restarted.
    Note { on: Id, why: String },
    /// Write nothing.
    Refuse(String),
}

/// A non-test raise: its id and its earliest raise time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Raised {
    alarm: Id,
    at: i128,
}

/// Every raise with a time, test raises left out, earliest time per alarm.
fn real_raises<P>(facts: &P) -> Vec<Raised>
where
    P: TriblePattern + ?Sized,
{
    let mut raised: Vec<(Id, i128)> = find!(
        (alarm: Id, at: Lower),
        pattern!(facts, [{ ?alarm @ metadata::tag: &KIND_ALARM, metadata::created_at: ?at }])
    )
    .filter(|(alarm, _)| {
        let alarm = *alarm;
        !exists!(pattern!(facts, [{ alarm @ metadata::tag: &KIND_TEST }]))
    })
    .map(|(alarm, at)| (alarm, at.0))
    .collect();
    raised.sort();
    raised.dedup_by_key(|(alarm, _)| *alarm);
    raised
        .into_iter()
        .map(|(alarm, at)| Raised { alarm, at })
        .collect()
}

fn newest(mut alarms: Vec<Raised>) -> Option<Raised> {
    alarms.sort_by_key(|raised| (raised.at, raised.alarm));
    alarms.pop()
}

fn ids(alarms: &[Raised]) -> String {
    alarms
        .iter()
        .map(|raised| format!("{:x}", raised.alarm))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The number of real alarms raised by `raiser` that were resolved as false
/// within [`FALSE_ALARM_WINDOW`] before `now`. It counts alarms, not
/// resolutions: one alarm resolved as false twice, by two zooids or by
/// concurrent writers, is one false alarm. A test raise spends no allowance,
/// so a test resolved as false does not halve it either.
pub fn false_alarms<P>(facts: &P, raiser: Id, now: i128) -> usize
where
    P: TriblePattern + ?Sized,
{
    let mut resolved: Vec<Id> = find!(
        (alarm: Id, at: Lower),
        pattern!(facts, [
            { _?resolve @
                metadata::tag: &KIND_RESOLVE,
                metadata::tag: &KIND_FALSE_ALARM,
                alarm::of: ?alarm,
                metadata::created_at: ?at,
            },
            { ?alarm @ metadata::tag: &KIND_ALARM, message::from: raiser }
        ])
    )
    .filter(|(alarm, at)| {
        let alarm = *alarm;
        at.0 > now - FALSE_ALARM_WINDOW
            && at.0 <= now
            && !exists!(pattern!(facts, [{ alarm @ metadata::tag: &KIND_TEST }]))
    })
    .map(|(alarm, _)| alarm)
    .collect();
    resolved.sort();
    resolved.dedup();
    resolved.len()
}

/// The raiser's daily allowance: [`RAISER_DAILY`], halved per recent false
/// alarm.
pub fn daily_allowance<P>(facts: &P, raiser: Id, now: i128) -> u32
where
    P: TriblePattern + ?Sized,
{
    let halvings = false_alarms(facts, raiser, now).min(31) as u32;
    RAISER_DAILY >> halvings
}

/// Decide what becomes of `draft` given everything already recorded.
/// `separate` is the raiser asking for a second ladder of a different class.
pub fn decide<P>(facts: &P, draft: &Raise, separate: bool, now: i128) -> Decision
where
    P: TriblePattern + ?Sized,
{
    if let Err(error) = validate_raise(draft) {
        return Decision::Refuse(format!(
            "{error:#}. An alarm needs a class, what is wrong, what is lost and by when \
             (--decays), and what you measured (--evidence). Before raising: {THE_CHECK}."
        ));
    }
    if draft.test {
        return Decision::Raise;
    }
    let raises = real_raises(facts);
    let open: Vec<Raised> = raises
        .iter()
        .copied()
        .filter(|raised| is_open(facts, raised.alarm))
        .collect();

    // Dedupe: a known incident, or the same text from anyone.
    if let Some(incident) = draft.incident.as_deref() {
        let key = text_handle(incident.trim());
        let same: Vec<Raised> = raises
            .iter()
            .copied()
            .filter(|raised| {
                let id = raised.alarm;
                exists!(pattern!(facts, [{ id @ alarm::incident: key }]))
                    && (is_open(facts, raised.alarm)
                        || super::closed_at(facts, raised.alarm)
                            .is_some_and(|closed| closed > now - INCIDENT_WINDOW))
            })
            .collect();
        if let Some(on) = newest(same) {
            return Decision::Note {
                on: on.alarm,
                why: format!(
                    "incident {incident:?} is already alarm {:x}; added as a note",
                    on.alarm
                ),
            };
        }
    } else {
        let what = text_handle(draft.what.trim());
        let same: Vec<Raised> = raises
            .iter()
            .copied()
            .filter(|raised| {
                let id = raised.alarm;
                raised.at > now - SAME_TEXT_WINDOW
                    && exists!(pattern!(facts, [{ id @ metadata::description: what }]))
            })
            .collect();
        if let Some(on) = newest(same) {
            return Decision::Note {
                on: on.alarm,
                why: format!(
                    "the same text was raised as alarm {:x} within 30 min; added as a note",
                    on.alarm
                ),
            };
        }
    }

    // One ladder at a time.
    let class = draft.class;
    let joins: Vec<Raised> = if separate {
        open.iter()
            .copied()
            .filter(|raised| {
                let id = raised.alarm;
                exists!(pattern!(facts, [{ id @ metadata::tag: class }]))
            })
            .collect()
    } else {
        open.clone()
    };
    if let Some(on) = newest(joins) {
        return Decision::Note {
            on: on.alarm,
            why: format!(
                "alarm {:x} is still open and its ladder is running; added as a note{}",
                on.alarm,
                if separate {
                    " (it has the same class, so --separate does not apply)"
                } else {
                    " (--separate starts a second ladder for a different class)"
                }
            ),
        };
    }

    // Rate limits, for a raise that would start a ladder.
    let raiser = draft.raiser;
    let by_raiser: Vec<Raised> = raises
        .iter()
        .copied()
        .filter(|raised| {
            let id = raised.alarm;
            exists!(pattern!(facts, [{ id @ message::from: raiser }]))
        })
        .collect();
    let recent: Vec<Raised> = by_raiser
        .iter()
        .copied()
        .filter(|raised| raised.at > now - RAISER_WINDOW)
        .collect();
    if !recent.is_empty() {
        return Decision::Refuse(format!(
            "you raised a real alarm within the last 30 min ({}); one per 30 min. \
             Add news to it with `alarm note`. Before raising again: {THE_CHECK}.",
            ids(&recent)
        ));
    }
    let today: Vec<Raised> = by_raiser
        .iter()
        .copied()
        .filter(|raised| raised.at > now - DAY)
        .collect();
    let allowance = daily_allowance(facts, draft.raiser, now);
    if today.len() as u64 >= u64::from(allowance) {
        return Decision::Refuse(format!(
            "you have raised {} real alarm(s) in 24 h ({}); your allowance is {allowance} \
             ({RAISER_DAILY}, halved for each false alarm of yours in the last 7 days: {}). \
             Before raising again: {THE_CHECK}.",
            today.len(),
            ids(&today),
            false_alarms(facts, draft.raiser, now)
        ));
    }
    let everyone: Vec<Raised> = raises
        .iter()
        .copied()
        .filter(|raised| raised.at > now - DAY)
        .collect();
    if everyone.len() as u64 >= u64::from(GLOBAL_DAILY) {
        return Decision::Refuse(format!(
            "{} real alarms were raised in 24 h ({}); the colony's limit is {GLOBAL_DAILY}. \
             Before raising again: {THE_CHECK}.",
            everyone.len(),
            ids(&everyone)
        ));
    }
    Decision::Raise
}
