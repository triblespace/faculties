//! Tailored Quota CLI input and shared output delivery.

use super::{render, Quota};
use crate::out::Out;
use crate::quota::{self, Observation};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "quota",
    about = "Provider-reported subscription quota, observed per window"
)]
pub struct Cli {
    /// Path to the pile file.
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable signing-key file. Reads and writes never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record one provider-reported reading of one rate-limit window.
    Record {
        /// Provider name: claude or codex.
        #[arg(long)]
        provider: String,
        /// Opaque account digest (e.g. SHA-256 hex of the provider's account
        /// id). Never a raw account id or email; '@' and whitespace are refused.
        #[arg(long)]
        account: String,
        /// Window length in seconds (18000 for five hours, 604800 for a week).
        #[arg(long)]
        window_secs: u64,
        /// Used percentage exactly as reported; may be fractional.
        #[arg(long)]
        used: f64,
        /// When the provider resets this window, in unix seconds.
        #[arg(long)]
        resets_at: i64,
        /// When the reading was taken, in unix seconds. Default: now.
        #[arg(long)]
        observed_at: Option<i64>,
        /// Observing host. Default: this machine's short host name.
        #[arg(long)]
        host: Option<String>,
    },
    /// Show the latest reading per provider, account and window.
    Show {
        /// Print one JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
}

pub fn execute(cli: Cli, output: &mut Out<'_>) -> Result<()> {
    let operations = Quota::new(cli.pile, cli.key);
    match cli.command {
        Command::Record {
            provider,
            account,
            window_secs,
            used,
            resets_at,
            observed_at,
            host,
        } => {
            let observed_at = match observed_at {
                Some(seconds) => seconds,
                None => quota::tai_ns_to_unix(crate::clock::tai_nanoseconds_now()?),
            };
            let host = match host {
                Some(host) => host,
                None => quota::local_host()?,
            };
            let observation = Observation {
                provider: quota::provider_id(&provider)?,
                account,
                window_secs,
                used_percent: used,
                resets_at,
                observed_at,
                host,
            };
            render::record(&operations.record(observation)?, output)
        }
        Command::Show { json } => {
            let view = operations.show()?;
            if json {
                render::show_json(&view, output)
            } else {
                render::show(&view, output)
            }
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    crate::cli::with_output("quota", |output| execute(cli, output))
}
