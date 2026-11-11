//! The escalation ladder as a pure function of the raise time, the clock and
//! the steps already recorded. The follower only executes what it returns.
//!
//! The schedule (minutes since the raise):
//!
//! | at       | rungs                                                  |
//! |----------|--------------------------------------------------------|
//! | 0  (R0)  | colony, Discord mention                                |
//! | 3  (R1)  | Discord re-mention, Discord voice (JP in it), Mac banner |
//! | 6  (R2)  | Mac sound, Reachy shout                                |
//! | 10 (R3)  | Hue alert                                              |
//! | 13, 16, .. 58 | Mac sound, Reachy shout, Hue alert (every 3 min)  |
//! | 13, 23, .. 53 | Discord re-mention (every 10 min after R1)        |
//! | 60       | final Discord line, final colony message; nothing sounds after this |
//!
//! A test raise runs R0 only.
//!
//! Per rung only the LATEST occurrence that is due matters, and it is
//! covered by any step of that rung started at or after it. So a rung that
//! already stepped is not repeated, a failed step covers its occurrence like
//! a delivered one (one dead channel never stalls the ladder, and never
//! retries in a loop), and a follower that was down for twenty minutes runs
//! each overdue rung once rather than replaying every missed occurrence.
//!
//! A rung this host cannot run is returned once, with no step yet, so the
//! executor records it FAILED ("unreachable from <host>") and the ledger
//! shows the gap; after that it is skipped. The voice rung is skipped while
//! JP is not in the voice channel, and returned if he joins while its
//! occurrence is still the latest one.

use triblespace::prelude::*;

use super::{StepRow, MINUTE};
use crate::schemas::alarm::{
    RUNG_COLONY, RUNG_DISCORD_MENTION, RUNG_DISCORD_VOICE, RUNG_FINAL_COLONY, RUNG_FINAL_DISCORD,
    RUNG_HUE_ALERT, RUNG_MAC_NOTIFY, RUNG_MAC_SOUND, RUNG_REACHY_SHOUT,
};

/// Time after the raise at which nothing sounds any more.
pub const STOP_SOUNDING: i128 = 60 * MINUTE;

/// Interval at which sound, shout and lights repeat after R3.
pub const SOUND_REPEAT: i128 = 3 * MINUTE;

/// Interval at which the Discord mention repeats after R1.
pub const MENTION_REPEAT: i128 = 10 * MINUTE;

/// The rungs of R0, the only ones a test raise runs.
pub const R0: [Id; 2] = [RUNG_COLONY, RUNG_DISCORD_MENTION];

/// The rungs of the final notice at [`STOP_SOUNDING`].
pub const FINAL: [Id; 2] = [RUNG_FINAL_DISCORD, RUNG_FINAL_COLONY];

/// Every scheduled occurrence of a real alarm as (offset since raise, rung),
/// in schedule order.
pub fn schedule() -> Vec<(i128, Id)> {
    let mut occurrences = vec![
        (0, RUNG_COLONY),
        (0, RUNG_DISCORD_MENTION),
        (3 * MINUTE, RUNG_DISCORD_MENTION),
        (3 * MINUTE, RUNG_DISCORD_VOICE),
        (3 * MINUTE, RUNG_MAC_NOTIFY),
        (6 * MINUTE, RUNG_MAC_SOUND),
        (6 * MINUTE, RUNG_REACHY_SHOUT),
        (10 * MINUTE, RUNG_HUE_ALERT),
    ];
    let mut at = 10 * MINUTE + SOUND_REPEAT;
    while at < STOP_SOUNDING {
        occurrences.extend([
            (at, RUNG_MAC_SOUND),
            (at, RUNG_REACHY_SHOUT),
            (at, RUNG_HUE_ALERT),
        ]);
        at += SOUND_REPEAT;
    }
    let mut at = 3 * MINUTE + MENTION_REPEAT;
    while at < STOP_SOUNDING {
        occurrences.push((at, RUNG_DISCORD_MENTION));
        at += MENTION_REPEAT;
    }
    occurrences.extend(FINAL.map(|rung| (STOP_SOUNDING, rung)));
    occurrences.sort_by_key(|(at, _)| *at);
    occurrences
}

/// What this host can do, probed at follower start and kept current.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Capabilities {
    /// Rungs this host cannot run at all.
    pub unreachable: Vec<Id>,
    /// Whether JP is in the Discord voice channel now.
    pub jp_in_voice: bool,
}

/// One rung to run now, and the occurrence it serves (offset since raise).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Due {
    pub rung: Id,
    pub at: i128,
}

/// What the follower does next.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Next {
    /// Run these rungs now, each recording one step.
    Run(Vec<Due>),
    /// Nothing is due; sleep until this instant (TAI nanoseconds), or until
    /// the facts change.
    Wait(i128),
    /// The ladder is over: the alarm is closed, or a test is past R0, or the
    /// final notice has gone out. A closed alarm stops at once.
    Stop,
}

/// The pure ladder. `raised_at` and `now` are TAI nanoseconds; `steps` are
/// this alarm's recorded steps ([`super::steps`]); `open` is
/// [`super::is_open`].
pub fn next_action(
    raised_at: i128,
    now: i128,
    steps: &[StepRow],
    open: bool,
    is_test: bool,
    capabilities: &Capabilities,
) -> Next {
    if !open {
        return Next::Stop;
    }
    let elapsed = now - raised_at;
    let occurrences: Vec<(i128, Id)> = schedule()
        .into_iter()
        .filter(|(at, rung)| {
            if is_test {
                R0.contains(rung) && *at == 0
            } else if elapsed >= STOP_SOUNDING {
                FINAL.contains(rung)
            } else {
                true
            }
        })
        .collect();
    let stepped = |rung: Id| steps.iter().any(|step| step.rung == rung);
    let covered = |rung: Id, at: i128| {
        steps
            .iter()
            .any(|step| step.rung == rung && step.started >= raised_at + at)
    };
    // Rungs in first-appearance order, so the result follows the schedule.
    let mut rungs: Vec<Id> = Vec::new();
    for (_, rung) in &occurrences {
        if !rungs.contains(rung) {
            rungs.push(*rung);
        }
    }
    let mut due = Vec::new();
    let mut wake: Option<i128> = None;
    for rung in rungs {
        if capabilities.unreachable.contains(&rung) {
            // Recorded once as unreachable, then never again.
            if !stepped(rung) {
                let first = occurrences
                    .iter()
                    .find(|(_, candidate)| *candidate == rung)
                    .map(|(at, _)| *at)
                    .expect("the rung came from these occurrences");
                if first <= elapsed {
                    due.push(Due { rung, at: first });
                } else {
                    wake = Some(wake.map_or(first, |wake| wake.min(first)));
                }
            }
            continue;
        }
        let latest = occurrences
            .iter()
            .filter(|(at, candidate)| *candidate == rung && *at <= elapsed)
            .map(|(at, _)| *at)
            .max();
        let next = occurrences
            .iter()
            .filter(|(at, candidate)| *candidate == rung && *at > elapsed)
            .map(|(at, _)| *at)
            .min();
        if let Some(at) = latest {
            let skipped = rung == RUNG_DISCORD_VOICE && !capabilities.jp_in_voice;
            if !skipped && !covered(rung, at) {
                due.push(Due { rung, at });
            }
        }
        if let Some(at) = next {
            wake = Some(wake.map_or(at, |wake| wake.min(at)));
        }
    }
    if !due.is_empty() {
        return Next::Run(due);
    }
    match wake {
        Some(at) => Next::Wait(raised_at + at),
        None => Next::Stop,
    }
}
