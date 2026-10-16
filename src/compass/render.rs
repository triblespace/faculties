//! Presentation of native action receipts, shared where the two frontends agree.

use anyhow::Result;

use super::{AddedGoal, AddedNote, MovedGoal, PriorityChange};
use crate::out::Out;

pub(super) fn added(receipt: &AddedGoal, output: &mut Out<'_>) -> Result<()> {
    warn_unresolved(&receipt.unresolved_mentions);
    output.line(format!("Added goal {:x}", receipt.goal))?;
    if let Some(note) = receipt.note {
        output.line(format!("Added note {note:x} to goal {:x}", receipt.goal))?;
    }
    Ok(())
}

/// One stderr line per `@label` that addressed nobody. The write itself has
/// already succeeded; the warning only says that no one was told.
fn warn_unresolved(labels: &[String]) {
    for label in labels {
        eprintln!("warning: @{label} matches no Relations person or group; nobody was mentioned");
    }
}

pub(super) fn moved(receipt: &MovedGoal, output: &mut Out<'_>) -> Result<()> {
    output.line(format!(
        "Moved goal {:x} to {}",
        receipt.goal, receipt.status
    ))
}

pub(super) fn noted(receipt: &AddedNote, output: &mut Out<'_>) -> Result<()> {
    warn_unresolved(&receipt.unresolved_mentions);
    output.line(format!(
        "Added note {:x} to goal {:x}",
        receipt.note, receipt.goal
    ))
}

pub(super) fn prioritized(receipt: &PriorityChange, output: &mut Out<'_>) -> Result<()> {
    output.line(format!(
        "{}{} > {}",
        if receipt.active { "" } else { "Removed: " },
        if receipt.higher_title.is_empty() {
            "?"
        } else {
            &receipt.higher_title
        },
        if receipt.lower_title.is_empty() {
            "?"
        } else {
            &receipt.lower_title
        },
    ))
}
