//! Presentation of already-opened Care observations. Display only: no value
//! is compared with a range, flagged or interpreted.

use super::operations::{csv, Added, Table};
use super::{format_number, format_time};
use crate::out::Out;
use anyhow::Result;

pub fn add(added: &Added, output: &mut Out<'_>) -> Result<()> {
    output.line(format!(
        "observation {:x} (sealed record {:x}) for {:x}",
        added.observation, added.record, added.subject
    ))
}

pub fn table(table: &Table, output: &mut Out<'_>) -> Result<()> {
    output.line(format!("observations for {:x}", table.subject))?;
    if table.rows.is_empty() {
        output.line("- (none readable)")?;
    }
    for row in &table.rows {
        let bounds = match (row.low, row.high) {
            (None, None) => String::new(),
            (low, high) => format!(
                " ({}..{})",
                low.map(format_number).unwrap_or_default(),
                high.map(format_number).unwrap_or_default()
            ),
        };
        let corrected = if row.supersedes.is_empty() {
            ""
        } else {
            "  [corrected]"
        };
        output.line(format!(
            "{}  {}  {}{} {}  {}  {}{corrected}",
            format_time(row.observed_at),
            row.kind,
            format_number(row.value),
            bounds,
            row.unit,
            row.method,
            &format!("{:x}", row.observation)[..8],
        ))?;
        if let Some(note) = &row.note {
            output.line(format!("    note: {note}"))?;
        }
    }
    if table.unreadable > 0 {
        output.line(format!(
            "({} record(s) present that this key cannot open)",
            table.unreadable
        ))?;
    }
    Ok(())
}

pub fn export_csv(table: &Table, output: &mut Out<'_>) -> Result<()> {
    output.text(csv(table))
}
