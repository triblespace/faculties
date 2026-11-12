//! Presentation of already-read alarms. No storage reads occur here.

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::follow::Ended;
use super::guards::Decision;
use super::operations::{ListView, Preview, Raised, Recorded, ShowView, StepReport};
use super::{rung_name, Outcome, MINUTE, SECOND};
use crate::out::Out;

/// TAI nanoseconds as UTC, to the second.
pub fn utc(tai_ns: i128) -> String {
    let unix = crate::quota::tai_ns_to_unix(tai_ns);
    DateTime::<Utc>::from_timestamp(unix, 0)
        .map(|time| time.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("{tai_ns}ns"))
}

fn short(id: triblespace::prelude::Id) -> String {
    format!("{id:x}")[..8].to_owned()
}

fn outcome(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Delivered => "delivered".to_owned(),
        Outcome::Failed(error) => format!("FAILED: {error}"),
    }
}

fn steps(steps: &[StepReport], output: &mut Out<'_>) -> Result<()> {
    for step in steps {
        output.line(format!(
            "  {} {}",
            rung_name(step.rung).unwrap_or("unknown rung"),
            outcome(&step.outcome)
        ))?;
    }
    Ok(())
}

/// The line the raising window must not miss: the faculty is not the whole job.
pub const SECURE_FIRST: &str = "FIRST secure the decaying artifact if you can (copy the \
     snapshot, stop the writer): the alarm does not do that for you.";

pub fn raised(raised: &Raised, output: &mut Out<'_>) -> Result<()> {
    match raised {
        Raised::Alarm {
            alarm,
            subject,
            steps: ran,
        } => {
            output.line(format!(
                "{}alarm {} raised ({}) by {} on {}",
                if subject.test { "TEST " } else { "" },
                short(*alarm),
                subject.class,
                subject.raiser_label,
                subject.host
            ))?;
            output.line(format!("  id {alarm:x}"))?;
            output.line("R0:")?;
            steps(ran, output)?;
            if !subject.test {
                output.line("Also send a PushNotification from your window: a faculty cannot.")?;
            }
            Ok(())
        }
        Raised::Noted { on, note, why } => {
            output.line(format!("no new ladder: {why}"))?;
            output.line(format!("  note {} on alarm {on:x}", short(*note)))
        }
    }
}

pub fn preview(preview: &Preview, output: &mut Out<'_>) -> Result<()> {
    output.line("DRY RUN: nothing written, nothing sent.")?;
    match &preview.decision {
        Decision::Raise => output.line(format!(
            "would raise alarm {} ({})",
            short(preview.subject.alarm),
            preview.subject.class
        ))?,
        Decision::Note { on, why } => {
            output.line(format!("would become a note on alarm {on:x}: {why}"))?
        }
        Decision::Refuse(why) => output.line(format!("would be refused: {why}"))?,
    }
    output.line("ladder:")?;
    for (at, rung) in &preview.ladder {
        output.line(format!(
            "  t+{:>2} min  {}",
            at / MINUTE,
            rung_name(*rung).unwrap_or("unknown rung")
        ))?;
    }
    Ok(())
}

pub fn recorded(what: &str, recorded: &Recorded, output: &mut Out<'_>) -> Result<()> {
    output.line(format!(
        "{what} {} recorded on alarm {}{}",
        short(recorded.record),
        short(recorded.alarm),
        if recorded.was_closed {
            " (it was already closed)"
        } else {
            ""
        }
    ))
}

pub fn list(view: &ListView, output: &mut Out<'_>) -> Result<()> {
    if view.alarms.is_empty() {
        output.line("no alarms")?;
    }
    for alarm in &view.alarms {
        let state = if alarm.open {
            "OPEN".to_owned()
        } else {
            match alarm.closed_at {
                Some(at) => format!("closed {}", utc(at)),
                None => "closed".to_owned(),
            }
        };
        output.line(format!(
            "{} {}{} [{}] {} by {} ({} steps, {} failed; raised {}, {} min ago)",
            short(alarm.alarm),
            if alarm.test { "TEST " } else { "" },
            state,
            alarm.classes.join(","),
            alarm.what.join(" / "),
            alarm.raisers.join(","),
            alarm.steps,
            alarm.failed_steps,
            utc(alarm.raised),
            (view.now - alarm.raised) / MINUTE,
        ))?;
        if let Some(via) = alarm.pending_ack {
            output.line(format!(
                "  acked by JP ({via}), not yet recorded: the follower records it, or \
                 `alarm ack {} --by <him>`",
                short(alarm.alarm)
            ))?;
        }
    }
    for (raiser, count) in &view.false_alarms {
        output.line(format!(
            "false alarms in the last 7 days by {raiser}: {count}"
        ))?;
    }
    if view.unread > 0 {
        output.line(format!(
            "{} alarm commit(s) could not be read here yet",
            view.unread
        ))?;
    }
    Ok(())
}

pub fn show(view: &ShowView, output: &mut Out<'_>) -> Result<()> {
    let subject = &view.subject;
    output.line(format!(
        "alarm {:x} {}{}",
        subject.alarm,
        if subject.test { "TEST " } else { "" },
        if view.open { "OPEN" } else { "closed" }
    ))?;
    output.line(format!("  class    {}", subject.class))?;
    output.line(format!(
        "  raised   {} by {} on {}",
        utc(subject.raised_at),
        subject.raiser_label,
        subject.host
    ))?;
    output.line(format!("  what     {}", subject.what))?;
    output.line(format!("  decays   {}", subject.decays))?;
    output.line(format!("  evidence {}", subject.evidence))?;
    if !view.incident.is_empty() {
        output.line(format!("  incident {}", view.incident.join(" / ")))?;
    }
    for step in &view.steps {
        output.line(format!(
            "  step t+{}s {} {}{}{}",
            (step.started - subject.raised_at) / SECOND,
            step.rung,
            match (step.delivered, step.failed) {
                (true, false) => "delivered",
                (false, true) => "FAILED",
                (true, true) => "delivered+FAILED",
                (false, false) => "outcome unknown",
            },
            if step.host.is_empty() {
                String::new()
            } else {
                format!(" on {}", step.host.join(","))
            },
            if step.error.is_empty() {
                String::new()
            } else {
                format!(": {}", step.error.join(" / "))
            }
        ))?;
    }
    for event in &view.events {
        output.line(format!(
            "  {} {} by {}{}{}",
            utc(event.at),
            event.kind,
            event.by,
            event
                .via
                .as_ref()
                .map(|via| format!(" via {via}"))
                .unwrap_or_default(),
            event
                .text
                .as_ref()
                .map(|text| format!(": {text}"))
                .unwrap_or_default()
        ))?;
    }
    if view.unread > 0 {
        output.line(format!(
            "  {} alarm commit(s) could not be read here yet",
            view.unread
        ))?;
    }
    Ok(())
}

pub fn ended(alarm: triblespace::prelude::Id, ended: Ended, output: &mut Out<'_>) -> Result<()> {
    output.line(match ended {
        Ended::Acked { via } => format!(
            "alarm {} acked by JP ({}); the follower stops",
            short(alarm),
            super::via_name(via).unwrap_or("unknown way")
        ),
        Ended::Closed => format!("alarm {} is closed; the follower stops", short(alarm)),
        Ended::Finished => format!(
            "alarm {}: the ladder is over; the follower stops (an alarm still open stays open)",
            short(alarm)
        ),
    })
}

pub fn adopted(adopted: &[triblespace::prelude::Id], output: &mut Out<'_>) -> Result<()> {
    if adopted.is_empty() {
        return output.line("no open alarm without a follower");
    }
    for alarm in adopted {
        output.line(format!("adopted alarm {}: follower started", short(*alarm)))?;
    }
    Ok(())
}
