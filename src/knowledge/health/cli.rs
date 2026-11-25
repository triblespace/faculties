//! Knowledge health subcommands, flattened into the `knowledge` CLI.
use super::{presentation, KnowledgeHealth};
use anyhow::Result;
use clap::Subcommand;
#[derive(Subcommand)]
pub(crate) enum Command {
    /// Show entry, frontier, tag, link, and orphan metrics.
    Health,
    /// Show current published/refuted states without settling forks.
    Quality,
    /// Show entries with the most unambiguously resolved incoming links.
    Hubs {
        #[arg(short, long, default_value = "15")]
        top: usize,
    },
    /// Find entries whose current states cite refuted or audit-warned entries.
    Risk,
    /// List entries for which every current state has zero outgoing links.
    Orphans {
        #[arg(short, long, default_value = "20")]
        top: usize,
        /// Print one stable entry selector per line.
        #[arg(long)]
        ids: bool,
    },
}

pub(crate) fn execute(
    command: Command,
    storage: crate::storage::Storage,
    out: &mut crate::out::Out<'_>,
) -> Result<()> {
    let health = KnowledgeHealth::with_storage(storage);
    match command {
        Command::Health => presentation::health(&health.health()?, out),
        Command::Quality => presentation::quality(&health.quality()?, out),
        Command::Hubs { top } => presentation::hubs(&health.hubs(top)?, out),
        Command::Risk => presentation::risk(&health.risk()?, out),
        Command::Orphans { top, ids } => presentation::orphans(&health.orphans(top)?, ids, out),
    }
}

/// Count current tag evidence both by state occurrence and by logical entry.
pub(crate) fn tag_counts(
    storage: crate::storage::Storage,
    out: &mut crate::out::Out<'_>,
) -> Result<()> {
    presentation::tags(&KnowledgeHealth::with_storage(storage).tags()?, out)
}
