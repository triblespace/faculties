//! Pure publication units and point-of-use queries. IDs are opaque: a
//! randomly identified imported execution suppresses the same timer slot as
//! an intrinsically identified local execution.

use anyhow::{bail, Result};
use ed25519_dalek::VerifyingKey;
use hifitime::{Duration, Epoch};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::prelude::*;

use super::runner::{Context, Outcome, Status};
use crate::clock;
use crate::schemas::trigger::{attrs, KIND_RESULT, KIND_RUN, KIND_STATE, KIND_TRIGGER};

pub type Text = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type Bytes = Inline<inlineencodings::Handle<blobencodings::RawBytes>>;

/// An operation-local typed projection, never retained as a second catalogue.
#[derive(Clone, Debug)]
pub struct Timer {
    pub id: Id,
    pub label: String,
    pub command: Text,
    pub interval_seconds: u64,
    pub origin: Epoch,
}

/// Fixed-period timers coalesce missed slots into the latest due slot. There
/// is no startup backlog, no elapsed-time arithmetic involving a completion,
/// and no repeated check while delivery is pending.
pub fn slot(origin: Epoch, interval_seconds: u64, now: Epoch) -> Option<Epoch> {
    let period = i128::from(interval_seconds).checked_mul(1_000_000_000)?;
    if period == 0 {
        return None;
    }
    let start = origin.to_tai_duration().total_nanoseconds();
    let elapsed = now
        .to_tai_duration()
        .total_nanoseconds()
        .checked_sub(start)?;
    if elapsed < 0 {
        return None;
    }
    let value = start.checked_add((elapsed / period).checked_mul(period)?)?;
    Some(Epoch::from_tai_duration(Duration::from_total_nanoseconds(
        value,
    )))
}

pub fn timer_definition(
    label: &str,
    command: &str,
    interval_seconds: u64,
    origin: Epoch,
    personas: &[Id],
    predecessors: &[Id],
) -> Result<Fragment> {
    if label.is_empty() || label.len() > crate::schemas::habit::MAX_LABEL_BYTES {
        bail!("Trigger label must contain 1..=30 UTF-8 bytes");
    }
    if command.trim().is_empty() || interval_seconds == 0 {
        bail!("Trigger needs a nonempty check and a positive timer interval");
    }
    let origin = clock::point(origin)?;
    Ok(entity! {
        metadata::tag: KIND_TRIGGER,
        attrs::label: label,
        attrs::command: command.to_owned(),
        attrs::context: "timer",
        attrs::interval_seconds: interval_seconds,
        attrs::scheduled_at: origin,
        attrs::persona*: personas.iter().copied(),
        metadata::supersedes*: predecessors.iter().copied(),
    })
}

pub fn event_definition(
    label: &str,
    command: &str,
    event: &str,
    context: Context,
    personas: &[Id],
    predecessors: &[Id],
) -> Result<Fragment> {
    if context == Context::Timer
        || command.trim().is_empty()
        || label.is_empty()
        || label.len() > 30
        || event.is_empty()
        || event.len() > 30
    {
        bail!("event trigger needs a label, check, event name and explicit event context");
    }
    Ok(entity! {
        metadata::tag: KIND_TRIGGER,
        attrs::label: label,
        attrs::command: command.to_owned(),
        attrs::context: context.as_str(),
        attrs::event: event,
        attrs::persona*: personas.iter().copied(),
        metadata::supersedes*: predecessors.iter().copied(),
    })
}

/// The query asks whether a recognized successor exists; it does not compare
/// an entity against a reconstructed hash or reject unfamiliar annotations.
pub fn current<P: TriblePattern + ?Sized>(facts: &P, definition: Id) -> bool {
    for (successor, _, _, context) in find!(
        (successor: Id, label: String, command: Text, context: String),
        pattern!(facts, [{ ?successor @
            metadata::tag: &KIND_TRIGGER,
            metadata::supersedes: &definition,
            attrs::label: ?label,
            attrs::command: ?command,
            attrs::context: ?context,
        }])
    ) {
        match context.as_str() {
            "timer" => {
                for (seconds, interval) in find!(
                    (seconds: u64, interval: (Epoch, Epoch)),
                    pattern!(facts, [{ successor @
                        attrs::interval_seconds: ?seconds,
                        attrs::scheduled_at: ?interval,
                    }])
                ) {
                    if seconds > 0 && interval.0 == interval.1 {
                        return false;
                    }
                }
            }
            "advisory-event" | "synchronous-event" => {
                if find!(event: String,
                    pattern!(facts, [{ successor @ attrs::event: ?event }])
                )
                .next()
                .is_some()
                {
                    return false;
                }
            }
            _ => {}
        }
    }
    // No payload is fetched here. A typed successor with unavailable command
    // bytes still retires its predecessor; execution records the failed read
    // instead of silently falling back to the obsolete command.
    true
}

pub fn addressed_to<P: TriblePattern + ?Sized>(facts: &P, definition: Id, persona: Id) -> bool {
    !exists!(pattern!(facts, [{ definition @ attrs::persona: _?recipient }]))
        || exists!(pattern!(facts, [{ definition @ attrs::persona: &persona }]))
}

/// Pause is conservative across a concurrent active/paused fork; neither
/// wall clock nor record order silently chooses a winner. A resume operation
/// explicitly supersedes the heads it has observed.
pub fn active<P: TriblePattern + ?Sized>(facts: &P, definition: Id) -> bool {
    let mut saw_state = false;
    let mut saw_head = false;
    for (assertion, state) in find!((id: Id, state: String), pattern!(facts, [{ ?id @
        metadata::tag: &KIND_STATE,
        attrs::trigger_of: &definition,
        attrs::state: ?state,
    }])) {
        if !matches!(state.as_str(), "active" | "paused") {
            continue;
        }
        saw_state = true;
        let superseded = exists!(pattern!(facts, [{
            metadata::tag: &KIND_STATE,
            attrs::trigger_of: &definition,
            metadata::supersedes: &assertion,
            attrs::state: "active",
        }])) || exists!(pattern!(facts, [{
            metadata::tag: &KIND_STATE,
            attrs::trigger_of: &definition,
            metadata::supersedes: &assertion,
            attrs::state: "paused",
        }]));
        if !superseded {
            saw_head = true;
            if state == "paused" {
                return false;
            }
        }
    }
    // Definitions start active without activation assertions. An observed
    // history with no understood frontier (e.g. opaque imported cycles) is
    // different: absence of an active frontier is not permission to execute.
    !saw_state || saw_head
}

pub fn state_fragment(definition: Id, paused: bool, predecessors: &[Id]) -> Fragment {
    entity! {
        metadata::tag: KIND_STATE,
        attrs::trigger_of: definition,
        attrs::state: if paused { "paused" } else { "active" },
        metadata::supersedes*: predecessors.iter().copied(),
    }
}

/// Select scheduled candidate slots from one observation. Attempt suppression
/// follows executable preparation: only that boundary knows whether a command
/// invokes an attached script or leaves unrelated attachments unused.
pub fn scheduled<P: TriblePattern + ?Sized>(
    facts: &P,
    persona: Id,
    now: Epoch,
) -> Vec<(Timer, Epoch)> {
    let mut due = Vec::new();
    for (id, label, command, seconds, interval) in find!(
        (id: Id, label: String, command: Text, seconds: u64,
         interval: (Epoch, Epoch)),
        pattern!(facts, [{ ?id @
            metadata::tag: &KIND_TRIGGER,
            attrs::label: ?label,
            attrs::command: ?command,
            attrs::context: "timer",
            attrs::interval_seconds: ?seconds,
            attrs::scheduled_at: ?interval,
        }])
    ) {
        // A range does not denote this reader's point-valued timer origin.
        // Other readers remain free to give such facts another meaning.
        if interval.0 != interval.1
            || !current(facts, id)
            || !active(facts, id)
            || !addressed_to(facts, id, persona)
        {
            continue;
        }
        let Some(scheduled) = slot(interval.0, seconds, now) else {
            continue;
        };
        due.push((
            Timer {
                id,
                label,
                command,
                interval_seconds: seconds,
                origin: interval.0,
            },
            scheduled,
        ));
    }
    due
}

pub fn attempted<P: TriblePattern + ?Sized>(
    facts: &P,
    definition: Id,
    command: Text,
    script: Option<Bytes>,
    executor: VerifyingKey,
    persona: Id,
    scheduled: Epoch,
) -> bool {
    let Ok(scheduled) = clock::point(scheduled) else {
        return false;
    };
    find!(run: Id, pattern!(facts, [{ ?run @
        metadata::tag: &KIND_RUN,
        attrs::trigger_of: &definition,
        attrs::command: command,
        attrs::executor: executor,
        attrs::persona: &persona,
        attrs::context: "timer",
        attrs::scheduled_at: scheduled,
    }]))
    .any(|run| match script {
        Some(script) => exists!(pattern!(facts, [{ run @ attrs::script: script }])),
        None => !exists!(pattern!(facts, [{ run @ attrs::script: _?script }])),
    })
}

/// Publish and flush this intent before invoking the check. The local
/// persistent owner serializes attempts; this is NOT a distributed exactly-
/// once lock. A crash after publication leaves an honest unfinished run and
/// does not cause the same timer slot to be executed on restart.
pub fn timer_run(
    definition: Id,
    command: Text,
    script: Option<Bytes>,
    executor: VerifyingKey,
    persona: Id,
    scheduled: Epoch,
) -> Result<Fragment> {
    let scheduled = clock::point(scheduled)?;
    Ok(entity! {
        metadata::tag: KIND_RUN,
        attrs::trigger_of: definition,
        attrs::command: command,
        attrs::script?: script,
        attrs::executor: executor,
        attrs::persona: persona,
        attrs::context: "timer",
        attrs::scheduled_at: scheduled,
    })
}

/// A result keeps execution status separate from a synchronous policy
/// verdict. `verdict` may be nonzero even when the native check ran correctly.
pub fn result_fragment(
    run: Id,
    outcome: &Outcome,
    verdict: Option<i32>,
    finished: Epoch,
) -> Result<Fragment> {
    if verdict.is_some() && outcome.context != Context::SynchronousEvent {
        bail!("only a synchronous event has a caller verdict");
    }
    let (status, detail) = match &outcome.status {
        Status::Success => ("success", String::new()),
        Status::Exit(code) => ("exit", code.to_string()),
        Status::Signal(signal) => ("signal", signal.to_string()),
        Status::TimedOut => ("timeout", "check exceeded its time limit".into()),
        Status::OutputLimit => ("output-limit", "check exceeded its output budget".into()),
        Status::SpawnFailed(error) => ("spawn-failed", error.clone()),
        Status::IoFailed(error) => ("io-failed", error.clone()),
    };
    let finished = clock::point(finished)?;
    Ok(entity! {
        metadata::tag: KIND_RESULT,
        attrs::run_of: run,
        attrs::context: outcome.context.as_str(),
        attrs::stdout: outcome.stdout.clone(),
        attrs::stderr: outcome.stderr.clone(),
        attrs::status: status,
        attrs::detail: detail,
        attrs::verdict?: verdict.map(i64::from),
        metadata::finished_at: finished,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn time(seconds: f64) -> Epoch {
        Epoch::from_unix_seconds(seconds)
    }
    fn host(byte: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[byte; 32]).verifying_key()
    }

    #[test]
    fn schedule_coalesces_without_completion_or_acknowledgement() {
        let persona = *triblespace::core::id::genid();
        let def = timer_definition("check", "true", 60, time(0.), &[], &[]).unwrap();
        let id = def.root().unwrap();
        let mut facts = def.facts().clone();
        assert!(scheduled(&facts, persona, time(-1.)).is_empty());
        assert_eq!(scheduled(&facts, persona, time(181.))[0].1, time(180.));
        let command = scheduled(&facts, persona, time(181.))[0].0.command;
        facts += timer_run(id, command, None, host(1), persona, time(180.))
            .unwrap()
            .facts()
            .clone();
        // No result or delivery receipt is needed to advance the clock.
        assert!(attempted(
            &facts,
            id,
            command,
            None,
            host(1),
            persona,
            time(180.)
        ));
        assert!(!attempted(
            &facts,
            id,
            command,
            None,
            host(1),
            persona,
            time(240.)
        ));
        assert!(!attempted(
            &facts,
            id,
            command,
            None,
            host(2),
            persona,
            time(180.)
        ));
        assert!(!attempted(
            &facts,
            id,
            command,
            None,
            host(1),
            *triblespace::core::id::genid(),
            time(180.)
        ));
        assert_eq!(scheduled(&facts, persona, time(240.))[0].1, time(240.));
        assert_eq!(slot(time(0.), 0, time(0.)), None);
    }

    #[test]
    fn opaque_runs_extra_facts_and_old_done_events_do_not_break_queries() {
        let persona = *triblespace::core::id::genid();
        let def = timer_definition("check", "false", 60, time(0.), &[], &[]).unwrap();
        let id = def.root().unwrap();
        let mut facts = def.facts().clone();
        facts += entity! {
            metadata::tag: crate::schemas::habit::KIND_DONE_ID,
            crate::schemas::habit::attrs::of: id,
            metadata::created_at: clock::point(time(20.)).unwrap(),
        }
        .facts()
        .clone();
        assert_eq!(scheduled(&facts, persona, time(30.)).len(), 1);
        let command = scheduled(&facts, persona, time(30.))[0].0.command;
        assert!(!attempted(
            &facts,
            id,
            command,
            None,
            host(1),
            persona,
            time(0.)
        ));
        let random = triblespace::core::id::genid();
        facts += entity! { &random @
            metadata::tag: KIND_RUN,
            attrs::trigger_of: id,
            attrs::command: command,
            attrs::executor: host(1),
            attrs::persona: persona,
            attrs::context: "timer",
            attrs::scheduled_at: clock::point(time(0.)).unwrap(),
            metadata::name: "unmodelled annotation",
        }
        .facts()
        .clone();
        assert!(attempted(
            &facts,
            id,
            command,
            None,
            host(1),
            persona,
            time(0.)
        ));
    }

    #[test]
    fn audience_pause_fork_and_explicit_resume() {
        let persona = *triblespace::core::id::genid();
        let def = timer_definition("check", "true", 60, time(0.), &[persona], &[]).unwrap();
        let id = def.root().unwrap();
        let mut facts = def.facts().clone();
        assert!(scheduled(&facts, *triblespace::core::id::genid(), time(0.)).is_empty());
        let pause = state_fragment(id, true, &[]);
        let active = state_fragment(id, false, &[]);
        facts += pause.facts().clone();
        facts += active.facts().clone();
        assert!(scheduled(&facts, persona, time(0.)).is_empty());
        facts += state_fragment(id, false, &[pause.root().unwrap(), active.root().unwrap()])
            .facts()
            .clone();
        assert_eq!(scheduled(&facts, persona, time(0.)).len(), 1);
        facts += timer_definition("check", "true", 60, time(0.), &[persona], &[id])
            .unwrap()
            .facts()
            .clone();
        assert!(!current(&facts, id));
    }

    #[test]
    fn script_qualified_attempts_do_not_suppress_other_executables() {
        let persona = *triblespace::core::id::genid();
        let def = timer_definition("carried", "@script", 60, time(0.), &[], &[]).unwrap();
        let id = def.root().unwrap();
        let mut facts = def.facts().clone();
        let command = scheduled(&facts, persona, time(0.))[0].0.command;
        let first = Bytes::new([11; 32]);
        let second = Bytes::new([12; 32]);
        let run = triblespace::core::id::genid();
        facts += entity! { &run @
            metadata::tag: KIND_RUN,
            attrs::trigger_of: id,
            attrs::command: command,
            attrs::script: first,
            attrs::executor: host(1),
            attrs::persona: persona,
            attrs::context: "timer",
            attrs::scheduled_at: clock::point(time(0.)).unwrap(),
        }
        .facts()
        .clone();
        assert!(attempted(
            &facts,
            id,
            command,
            Some(first),
            host(1),
            persona,
            time(0.)
        ));
        assert!(!attempted(
            &facts,
            id,
            command,
            Some(second),
            host(1),
            persona,
            time(0.)
        ));
        assert!(!attempted(
            &facts,
            id,
            command,
            None,
            host(1),
            persona,
            time(0.)
        ));
        assert_ne!(
            timer_run(id, command, Some(first), host(1), persona, time(0.))
                .unwrap()
                .root(),
            timer_run(id, command, Some(second), host(1), persona, time(0.))
                .unwrap()
                .root()
        );
    }

    #[test]
    fn unknown_or_incomplete_successors_do_not_retire_understood_definitions() {
        let definition = timer_definition("old", "true", 60, time(0.), &[], &[]).unwrap();
        let id = definition.root().unwrap();
        let mut facts = definition.facts().clone();
        facts += entity! {
            metadata::tag: KIND_TRIGGER,
            metadata::supersedes: id,
        }
        .facts()
        .clone();
        facts += entity! {
            metadata::tag: KIND_TRIGGER,
            metadata::supersedes: id,
            attrs::label: "future",
            attrs::command: "false".to_owned(),
            attrs::context: "future-context",
        }
        .facts()
        .clone();
        assert!(current(&facts, id));

        let missing_command = Text::new([17; 32]);
        facts += entity! {
            metadata::tag: KIND_TRIGGER,
            metadata::supersedes: id,
            attrs::label: "known, bytes absent",
            attrs::command: missing_command,
            attrs::context: "timer",
            attrs::interval_seconds: 60u64,
            attrs::scheduled_at: clock::point(time(0.)).unwrap(),
        }
        .facts()
        .clone();
        assert!(
            !current(&facts, id),
            "missing bytes must not revive obsolete commands"
        );
    }

    #[test]
    fn unknown_or_incomplete_states_cannot_cancel_a_pause() {
        let definition = *triblespace::core::id::genid();
        let pause = state_fragment(definition, true, &[]);
        let paused = pause.root().unwrap();
        let mut facts = pause.facts().clone();
        facts += entity! {
            metadata::tag: KIND_STATE,
            attrs::trigger_of: definition,
            metadata::supersedes: paused,
        }
        .facts()
        .clone();
        facts += entity! {
            metadata::tag: KIND_STATE,
            attrs::trigger_of: definition,
            metadata::supersedes: paused,
            attrs::state: "future-state",
        }
        .facts()
        .clone();
        assert!(!active(&facts, definition));
        facts += state_fragment(definition, false, &[paused]).facts().clone();
        assert!(active(&facts, definition));
    }

    #[test]
    fn an_activation_cycle_is_not_an_active_frontier() {
        let definition = *triblespace::core::id::genid();
        let left = triblespace::core::id::genid();
        let right = triblespace::core::id::genid();
        let mut facts = entity! { &left @
            metadata::tag: KIND_STATE,
            attrs::trigger_of: definition,
            metadata::supersedes: *right,
            attrs::state: "paused",
        }
        .facts()
        .clone();
        facts += entity! { &right @
            metadata::tag: KIND_STATE,
            attrs::trigger_of: definition,
            metadata::supersedes: *left,
            attrs::state: "paused",
        }
        .facts()
        .clone();
        assert!(!active(&facts, definition));
        let alone = triblespace::core::id::genid();
        let self_cycle = entity! { &alone @
            metadata::tag: KIND_STATE,
            attrs::trigger_of: definition,
            metadata::supersedes: *alone,
            attrs::state: "paused",
        };
        assert!(!active(self_cycle.facts(), definition));
    }

    #[test]
    fn result_preserves_streams_and_policy_verdict_is_not_execution_failure() {
        let outcome = Outcome {
            context: Context::SynchronousEvent,
            stdout: b"denied\n".to_vec(),
            stderr: vec![0, 255],
            status: Status::Success,
        };
        let result =
            result_fragment(*triblespace::core::id::genid(), &outcome, Some(1), time(1.)).unwrap();
        assert!(exists!(
            pattern!(result.facts(), [{ attrs::status: "success", attrs::verdict: 1i64 }])
        ));
        let timer = Outcome {
            context: Context::Timer,
            ..outcome
        };
        assert!(
            result_fragment(*triblespace::core::id::genid(), &timer, Some(1), time(1.)).is_err()
        );
    }
}
