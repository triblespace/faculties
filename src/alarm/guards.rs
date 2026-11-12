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
//!
//! Each rule is one query, constrained to what it asks: the alarms with this
//! incident, the same text since 30 min ago, the open alarms (of this class),
//! this raiser's raises since a time, everyone's raises since a time, and
//! this raiser's alarms with a false resolution in the last 7 days. Time
//! windows are `value_range` constraints on the raise or resolve time.
//! What the engine cannot say positively is checked per candidate with
//! `exists!`: that an alarm is not a test, and that nothing has closed it.
//! The engine emits one row per witness, so a set of alarm ids is collected
//! wherever the answer is a count.
//!
//! What a decision can promise. The guards answer for the facts they are
//! given and nothing more. A raise whose commit has not yet arrived is not
//! counted, so `decide` must be fed a complete observation of the alarm
//! collection (`storage::acquire_facts`, or `require_complete_attached_read`
//! on an attached read); the tolerant read [`super::observe`] uses, and
//! `storage::acquire_facts_read`, are for the follower and for display,
//! never for a guard. And even a complete local read cannot serialise raises
//! across hosts or across concurrent processes on one host: two zooids that
//! decide at the same moment, or on hosts that have not yet synced each
//! other's raises, can both be told `Raise`. So the one-ladder rule and the
//! rate limits are best-effort per host. Two such raises are two open
//! alarms, and each gets a ladder. Step 5 owns that contract; nothing in
//! this module guarantees it.

use std::collections::BTreeSet;

use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::query::rangeconstraint::value_range;
use triblespace::core::query::TriblePattern;
use triblespace::prelude::*;

use super::{
    closed_at, is_closed, point_at, text_handle, validate_raise, IntervalValue, Raise, MINUTE,
};
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

/// Inclusive `value_range` bounds selecting every time interval whose lower
/// bound is after `after` and, when `until` is given, at or before it.
/// NsTAIInterval orders by lower bound, then upper (core/inline/encodings/
/// time.rs), so the minimum takes the lowest upper half and the maximum the
/// highest; a point-valued bound would cut off non-point intervals.
pub(crate) fn lower_after(after: i128, until: Option<i128>) -> (IntervalValue, IntervalValue) {
    let mut min = point_at(after.saturating_add(1)).expect("a point interval always encodes");
    min.raw[16..].fill(0);
    let max = match until {
        Some(until) => {
            let mut max = point_at(until).expect("a point interval always encodes");
            max.raw[16..].fill(255);
            max
        }
        None => IntervalValue::new([255; 32]),
    };
    (min, max)
}

fn is_test<P>(facts: &P, alarm: Id) -> bool
where
    P: TriblePattern + ?Sized,
{
    exists!(pattern!(facts, [{ alarm @ metadata::tag: &KIND_TEST }]))
}

/// The newest of `(alarm, raised at)` rows, by raise time, then id.
fn newest(rows: impl Iterator<Item = (Id, Lower)>) -> Option<Id> {
    rows.max_by_key(|(alarm, at)| (at.0, *alarm))
        .map(|(alarm, _)| alarm)
}

fn ids(alarms: &BTreeSet<Id>) -> String {
    alarms
        .iter()
        .map(|alarm| format!("{alarm:x}"))
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
    let (min, max) = lower_after(now - FALSE_ALARM_WINDOW, Some(now));
    let false_resolved: BTreeSet<Id> = find!(
        alarm: Id,
        temp!((at), and!(
            pattern!(facts, [
                { _?resolve @
                    metadata::tag: &KIND_RESOLVE,
                    metadata::tag: &KIND_FALSE_ALARM,
                    alarm::of: ?alarm,
                    metadata::created_at: ?at,
                },
                { ?alarm @ metadata::tag: &KIND_ALARM, message::from: raiser }
            ]),
            value_range(at, min, max),
        ))
    )
    .collect();
    false_resolved
        .into_iter()
        .filter(|alarm| !is_test(facts, *alarm))
        .count()
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
    let real = |alarm: &Id| !is_test(facts, *alarm);

    // Dedupe: a known incident, or the same text from anyone.
    if let Some(incident) = draft.incident.as_deref() {
        let key = text_handle(incident.trim());
        let same = newest(
            find!(
                (alarm: Id, at: Lower),
                pattern!(facts, [{ ?alarm @
                    metadata::tag: &KIND_ALARM,
                    alarm::incident: key,
                    metadata::created_at: ?at,
                }])
            )
            .filter(|(alarm, _)| {
                real(alarm)
                    && (!is_closed(facts, *alarm)
                        || closed_at(facts, *alarm)
                            .is_some_and(|closed| closed > now - INCIDENT_WINDOW))
            }),
        );
        if let Some(on) = same {
            return Decision::Note {
                on,
                why: format!("incident {incident:?} is already alarm {on:x}; added as a note"),
            };
        }
    } else {
        let what = text_handle(draft.what.trim());
        let (min, max) = lower_after(now - SAME_TEXT_WINDOW, None);
        let same = newest(
            find!(
                (alarm: Id, at: Lower),
                and!(
                    pattern!(facts, [{ ?alarm @
                        metadata::tag: &KIND_ALARM,
                        metadata::description: what,
                        metadata::created_at: ?at,
                    }]),
                    value_range(at, min, max),
                )
            )
            .filter(|(alarm, _)| real(alarm)),
        );
        if let Some(on) = same {
            return Decision::Note {
                on,
                why: format!(
                    "the same text was raised as alarm {on:x} within 30 min; added as a note"
                ),
            };
        }
    }

    // One ladder at a time: an open real alarm, of this class with --separate.
    let open = |(alarm, _): &(Id, Lower)| real(alarm) && !is_closed(facts, *alarm);
    let class = draft.class;
    let joins = if separate {
        newest(
            find!(
                (alarm: Id, at: Lower),
                pattern!(facts, [{ ?alarm @
                    metadata::tag: &KIND_ALARM,
                    metadata::tag: class,
                    metadata::created_at: ?at,
                }])
            )
            .filter(open),
        )
    } else {
        newest(
            find!(
                (alarm: Id, at: Lower),
                pattern!(facts, [{ ?alarm @ metadata::tag: &KIND_ALARM, metadata::created_at: ?at }])
            )
            .filter(open),
        )
    };
    if let Some(on) = joins {
        return Decision::Note {
            on,
            why: format!(
                "alarm {on:x} is still open and its ladder is running; added as a note{}",
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
    let raised_by_since = |after: i128| -> BTreeSet<Id> {
        let (min, max) = lower_after(after, None);
        find!(
            alarm: Id,
            temp!((at), and!(
                pattern!(facts, [{ ?alarm @
                    metadata::tag: &KIND_ALARM,
                    message::from: raiser,
                    metadata::created_at: ?at,
                }]),
                value_range(at, min, max),
            ))
        )
        .collect::<BTreeSet<Id>>()
        .into_iter()
        .filter(real)
        .collect()
    };
    let recent = raised_by_since(now - RAISER_WINDOW);
    if !recent.is_empty() {
        return Decision::Refuse(format!(
            "you raised a real alarm within the last 30 min ({}); one per 30 min. \
             Add news to it with `alarm note`. Before raising again: {THE_CHECK}.",
            ids(&recent)
        ));
    }
    let today = raised_by_since(now - DAY);
    let allowance = daily_allowance(facts, raiser, now);
    if today.len() as u64 >= u64::from(allowance) {
        return Decision::Refuse(format!(
            "you have raised {} real alarm(s) in 24 h ({}); your allowance is {allowance} \
             ({RAISER_DAILY}, halved for each false alarm of yours in the last 7 days: {}). \
             Before raising again: {THE_CHECK}.",
            today.len(),
            ids(&today),
            false_alarms(facts, raiser, now)
        ));
    }
    let (min, max) = lower_after(now - DAY, None);
    let everyone: BTreeSet<Id> = find!(
        alarm: Id,
        temp!((at), and!(
            pattern!(facts, [{ ?alarm @ metadata::tag: &KIND_ALARM, metadata::created_at: ?at }]),
            value_range(at, min, max),
        ))
    )
    .collect::<BTreeSet<Id>>()
    .into_iter()
    .filter(real)
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
