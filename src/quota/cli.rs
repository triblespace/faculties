//! Tailored Quota CLI input and shared output delivery.

use super::{render, Quota};
use crate::out::Out;
use crate::quota::{self, Observation};
use anyhow::{bail, Result};
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
        /// Provider-side allowance name (e.g. a Codex rateLimitsByLimitId
        /// key). Joins the grouping key, so two allowances with the same
        /// window length stay apart. Omit it when the provider has none.
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        bucket: Option<String>,
    },
    /// Read each provider's own public status page (Statuspage v2
    /// summary.json for claude and codex) and record one observation per
    /// page. A page that cannot be fetched or parsed records nothing and
    /// makes the command fail after the others are recorded.
    ProviderStatus {
        /// Observing host. Default: this machine's short host name.
        #[arg(long)]
        host: Option<String>,
    },
    /// Show the latest reading per provider, account, window and bucket, and
    /// the latest reading of each provider's own status page.
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
            bucket,
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
                bucket,
            };
            render::record(&operations.record(observation)?, output)
        }
        Command::ProviderStatus { host } => {
            let host = match host {
                Some(host) => host,
                None => quota::local_host()?,
            };
            let outcomes = operations.provider_status(
                quota::status::fetch_summary,
                || Ok(quota::tai_ns_to_unix(crate::clock::tai_nanoseconds_now()?)),
                &host,
            )?;
            render::provider_status(&outcomes, output)?;
            let failed: Vec<&str> = outcomes
                .iter()
                .filter(|outcome| outcome.result.is_err())
                .map(|outcome| quota::known_provider_name(outcome.provider).unwrap_or("unknown"))
                .collect();
            if !failed.is_empty() {
                let failed = failed.join(", ");
                bail!("status page unreadable for {failed}; nothing recorded for {failed}");
            }
            Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record_args(bucket: Option<&str>) -> Vec<&str> {
        let mut args = vec![
            "quota",
            "--pile",
            "unused.pile",
            "record",
            "--provider",
            "codex",
            "--account",
            "a1b2c3",
            "--window-secs",
            "604800",
            "--used",
            "21",
            "--resets-at",
            "1792894800",
        ];
        if let Some(bucket) = bucket {
            args.extend(["--bucket", bucket]);
        }
        args
    }

    #[test]
    fn provider_status_takes_only_an_optional_host() {
        let base = ["quota", "--pile", "unused.pile", "provider-status"];
        assert!(Cli::try_parse_from(base).is_ok());
        assert!(Cli::try_parse_from(base.iter().chain(&["--host", "sky"])).is_ok());
        assert!(Cli::try_parse_from(base.iter().chain(&["--url", "x"])).is_err());
    }

    #[test]
    fn bucket_is_optional_and_an_empty_one_is_refused() {
        assert!(Cli::try_parse_from(record_args(None)).is_ok());
        assert!(Cli::try_parse_from(record_args(Some("codex"))).is_ok());
        assert!(Cli::try_parse_from(record_args(Some(""))).is_err());
    }
}
