use std::fmt::Write as _;

use anyhow::Result;
use triblespace::core::id::Id;

use super::{
    Activation, AddedHabit, Habit, HabitCheck, HabitList, HabitObservation, HabitOccurrence,
    HabitStateChange, State,
};
use crate::out::Out;

fn persona_targets(personas: &[Id]) -> String {
    if personas.is_empty() {
        "everyone".into()
    } else {
        personas
            .iter()
            .map(|id| format!("{id:x}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn added(receipt: &AddedHabit, out: &mut Out<'_>) -> Result<()> {
    if receipt.already_present {
        out.line(format!("Habit already present [{:x}]", receipt.id))?;
        return out.line(format!(
            "  personas: {}",
            persona_targets(&receipt.personas)
        ));
    }
    let carried = receipt
        .script
        .as_ref()
        .map(|(digest, len)| {
            format!(
                " · script {} ({len} bytes, carried in the pile)",
                &digest[..8]
            )
        })
        .unwrap_or_default();
    out.line(format!(
        "added {} [{:x}] · cooldown {}s{carried}",
        receipt.label, receipt.id, receipt.cooldown_secs
    ))?;
    out.line(format!(
        "  personas: {}",
        persona_targets(&receipt.personas)
    ))?;
    for id in &receipt.supersedes {
        out.line(format!("  supersedes [{id:x}]"))?;
    }
    if !receipt.sharing.is_empty() {
        let ids = receipt
            .sharing
            .iter()
            .map(|id| format!("{id:x}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.line(format!(
            "  note: {} other live Habit(s) share this label and remain live: {ids}",
            receipt.sharing.len()
        ))?;
        out.line("        add `--supersedes <id>` if this revision replaces one.")?;
    }
    Ok(())
}

/// One block per reading. A definition with a single reading renders exactly
/// as it always has; further readings follow, each complete.
pub fn shown(observation: &HabitObservation, out: &mut Out<'_>) -> Result<()> {
    shown_reading(&observation.definition, observation.superseded, out)?;
    let total = observation.alternatives.len() + 1;
    for (index, habit) in observation.alternatives.iter().enumerate() {
        out.line("")?;
        out.line(format!(
            "reading {} of {total} (several values for one attribute; each is read)",
            index + 2
        ))?;
        shown_reading(habit, observation.superseded, out)?;
    }
    Ok(())
}

fn shown_reading(habit: &Habit, superseded: bool, out: &mut Out<'_>) -> Result<()> {
    out.line(format!("label:       {}", habit.label))?;
    out.line(format!("id:          {:x}", habit.id))?;
    out.line(format!("personas:    {}", persona_targets(&habit.personas)))?;
    out.line(format!(
        "definition:  {}",
        if superseded { "superseded" } else { "live" }
    ))?;
    out.line(format!("condition:   {}", habit.condition))?;
    out.line(format!("nudge:\n{}", habit.nudge))?;
    match &habit.script {
        Some(script) => out.line(format!(
            "script:      {} ({} bytes, carried in the pile)",
            script.digest(),
            script.bytes.len()
        ))?,
        None => out.line("script:      none")?,
    }
    out.line(format!(
        "supersedes:  {}",
        if habit.supersedes.is_empty() {
            "none".into()
        } else {
            habit
                .supersedes
                .iter()
                .map(|id| format!("{id:x}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ))
}

/// The counts on the answer; every observation beside it, as a note. Nothing
/// here is a verdict, so the command succeeds whatever it observed.
pub fn checked(check: &HabitCheck, out: &mut Out<'_>) -> Result<()> {
    let audit = &check.audit;
    out.line(format!(
        "Habit collection {} (scope {:X}): {} definitions ({} live, {} carrying their own script), {} completions, {} state assertions read",
        check.collection,
        crate::schemas::habit::DEFAULT_SCOPE_ID,
        audit.definitions,
        audit.live,
        audit.scripted,
        audit.completions,
        audit.assertions,
    ))?;
    for note in &audit.notes {
        out.note(note.clone())?;
    }
    Ok(())
}

pub fn completed(receipt: &HabitOccurrence, out: &mut Out<'_>) -> Result<()> {
    out.line(format!("done {} [{:x}]", receipt.label, receipt.event))
}

pub fn state_changed(receipt: &HabitStateChange, out: &mut Out<'_>) -> Result<()> {
    match receipt.event {
        Some(id) => out.line(format!(
            "{} {} [{id:x}]",
            receipt.state.as_str(),
            receipt.label
        )),
        None => out.line(format!(
            "{} already {}",
            receipt.label,
            receipt.state.as_str()
        )),
    }
}

fn state_detail(state: &State) -> Option<String> {
    match state {
        State::Forked(heads) => Some(format!(
            "state heads disagree: {}",
            heads
                .iter()
                .map(|(id, state)| format!("{id:x}={}", state.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        State::Unparseable(error) | State::Failed(error) => Some(error.clone()),
        _ => None,
    }
}

pub struct Listing {
    pub text: String,
    pub diagnostics: Vec<String>,
}

/// Format already observed/evaluated rows. No predicates run while rendering.
pub fn listed(report: &HabitList, only_due: bool) -> Listing {
    let mut text = String::new();
    let mut diagnostics = Vec::new();
    let mut shown = 0usize;
    let mut deferred = 0usize;
    // An intention is due when any of its readings is, and is announced once.
    let mut announced = std::collections::BTreeSet::new();
    for observed in &report.entries {
        let row = &observed.row;
        if only_due {
            match &observed.state {
                Some(state) if state.is_due() => {
                    if announced.insert(row.id) {
                        shown += 1;
                        writeln!(text, "{}: {}", row.label, row.nudge).unwrap();
                    }
                }
                Some(State::Deferred(pending)) => {
                    deferred += 1;
                    writeln!(text, "{} [{:x}] deferred: {pending}", row.label, row.id).unwrap();
                }
                Some(state) => {
                    if let Some(detail) = state_detail(state) {
                        diagnostics.push(format!(
                            "{} [{:x}] {}: {detail}",
                            row.label,
                            row.id,
                            state.word().to_ascii_lowercase()
                        ));
                    }
                }
                None => diagnostics.push(format!(
                    "{} [{:x}] condition not evaluated",
                    row.label, row.id
                )),
            }
            continue;
        }
        shown += 1;
        let last = row
            .last_done()
            .map(|done| {
                format!(
                    "{}h ago",
                    report.observed_seconds.saturating_sub(done).max(0) / 3600
                )
            })
            .unwrap_or_else(|| "never".into());
        let carried = row
            .script
            .as_ref()
            .map(|script| format!("  script:{}", script.short_digest()))
            .unwrap_or_default();
        let word = observed
            .state
            .as_ref()
            .map(State::word)
            .unwrap_or(match &row.activation {
                Activation::Active(_) => "unevaluated",
                Activation::Paused(_) => "paused",
                Activation::Forked(_) => "FORKED",
            });
        writeln!(
            text,
            "{:<22} {:<9} done {:<12} {}{carried} [{:x}]",
            row.label, word, last, row.condition, row.id
        )
        .unwrap();
        if let Some(detail) = observed.state.as_ref().and_then(state_detail) {
            writeln!(text, "{:<22}   {detail}", "").unwrap();
        }
        if let Some(State::Deferred(pending)) = &observed.state {
            writeln!(text, "{:<22}   {pending}", "").unwrap();
        }
        if observed.state.is_none() {
            if let Activation::Forked(heads) = &row.activation {
                let detail = heads
                    .iter()
                    .map(|head| format!("{:x}={}", head.id, head.state.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(text, "{:<22}   state heads disagree: {detail}", "").unwrap();
            }
        }
    }
    if shown == 0 {
        if only_due && deferred > 0 {
            writeln!(text, "no confirmed due; {deferred} deferred reading(s)").unwrap();
        } else {
            writeln!(
                text,
                "{}",
                if only_due {
                    "nothing due"
                } else {
                    "no habits yet"
                }
            )
            .unwrap();
        }
    }
    if !only_due && report.superseded > 0 {
        writeln!(
            text,
            "({} superseded revision(s) not shown)",
            report.superseded
        )
        .unwrap();
    }
    Listing { text, diagnostics }
}
