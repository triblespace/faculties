//! Care CLI: literal arguments, `@path`/`@-` for note text, a photo by path.

use super::{render, AddInput, Care};
use crate::out::Out;
use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "care",
    about = "Record and display health observations (one sealed record per reading). Records and displays only; no interpretation."
)]
pub struct Cli {
    /// Path to the pile file.
    #[arg(long, env = "PILE")]
    pile: Option<PathBuf>,
    /// Existing durable signing-key file. Reads and writes never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Record one observation. Blood pressure is two observations
    /// (systolic, diastolic) with the same --observed and source.
    Add {
        /// Relations label/alias or exact id of the person observed.
        #[arg(long)]
        subject: String,
        /// systolic, diastolic, heart-rate, temperature, urine, intake, pain (or LOINC code).
        #[arg(long)]
        kind: String,
        #[arg(long, allow_hyphen_values = true)]
        value: f64,
        /// UCUM code or alias: mm[Hg], /min, Cel, mL, {score}.
        #[arg(long)]
        unit: String,
        /// When it happened (UTC): YYYY-MM-DDTHH:MM[:SS], or A..B when approximate.
        #[arg(long)]
        observed: String,
        /// When it was reported (UTC); defaults to now.
        #[arg(long)]
        reported: Option<String>,
        /// measured-device, photo-estimate, reported-by-subject, administered.
        #[arg(long)]
        method: String,
        /// Lower bound of an estimate.
        #[arg(long, allow_hyphen_values = true)]
        low: Option<f64>,
        /// Upper bound of an estimate.
        #[arg(long, allow_hyphen_values = true)]
        high: Option<f64>,
        /// Source photo file; its bytes are sealed with the record.
        #[arg(long)]
        photo: Option<PathBuf>,
        /// Source Discord message: its exact stable message anchor id.
        #[arg(long)]
        message: Option<String>,
        /// Note text; @path reads a file, @- reads stdin.
        #[arg(long)]
        note: Option<String>,
        /// Exact id of an observation this one corrects (repeatable).
        #[arg(long)]
        supersedes: Vec<String>,
    },
    /// Show a subject's observations, newest correction of each.
    Table {
        subject: String,
        /// Only observations whose observed time reaches this UTC time.
        #[arg(long)]
        since: Option<String>,
        /// Only this kind.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Share a subject's readings with another key through Secrets group
    /// grants (one per kind), then deliver their keys. Later readings in the
    /// same groups reach that key at the next delivery maintenance.
    Grant {
        subject: String,
        /// Recipient Ed25519 public key, 64 hex characters.
        #[arg(long)]
        to: String,
        /// Only this kind's group.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Export a subject's observations for a doctor.
    Export {
        subject: String,
        /// CSV output (the only format so far).
        #[arg(long)]
        csv: bool,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        kind: Option<String>,
    },
}

fn execute(care: Care, command: Command, output: &mut Out<'_>) -> Result<()> {
    match command {
        Command::Add {
            subject,
            kind,
            value,
            unit,
            observed,
            reported,
            method,
            low,
            high,
            photo,
            message,
            note,
            supersedes,
        } => {
            let photo = photo
                .map(|path| {
                    std::fs::read(&path).with_context(|| format!("read photo {}", path.display()))
                })
                .transpose()?;
            let note = note.map(|raw| crate::text_arg(&raw, "note")).transpose()?;
            let added = care.add(&AddInput {
                subject,
                kind,
                value,
                low,
                high,
                unit,
                observed,
                reported,
                method,
                photo,
                message,
                note,
                supersedes,
            })?;
            render::add(&added, output)
        }
        Command::Grant { subject, to, kind } => {
            render::grant(&care.grant(&subject, &to, kind.as_deref())?, output)
        }
        Command::Table {
            subject,
            since,
            kind,
        } => render::table(
            &care.table(&subject, since.as_deref(), kind.as_deref())?,
            output,
        ),
        Command::Export {
            subject,
            csv,
            since,
            kind,
        } => {
            if !csv {
                bail!("choose an export format: --csv");
            }
            render::export_csv(
                &care.table(&subject, since.as_deref(), kind.as_deref())?,
                output,
            )
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    let Some(pile) = cli.pile else {
        bail!("no pile: set $PILE or pass --pile");
    };
    crate::cli::with_output("care", |output| {
        crate::storage::Storage::new(pile.clone(), cli.key.clone())
            .scope_to_exit(|storage| execute(Care::with_storage(storage.clone()), command, output))
    })
}
