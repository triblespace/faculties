//! `rules tick`: run the sensors, settle the driver, and in shadow mode log the
//! rules' conclusions beside what `ignite.sh --dry-run` and `ask-jp-due.sh`
//! decide. Nothing here acts on a conclusion.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use triblespace::core::collection::CollectionStoreExt;
use triblespace::core::query::register::Resolution;
use triblespace::core::reactive::driver::{Bounds, CollectionDriver};
use triblespace::core::repo::StorageFlush;
use triblespace::prelude::*;

use super::{
    ask_quiet_since, eligible_goals, nudges_in, open_asks, program, quota_ok, run_requests_in,
    settle, tick_fragment, Sensed,
};
use crate::compass::Compass;
use crate::out::Out;
use crate::quota::operations::Quota;
use crate::schemas::rules::{DEFAULT_SCOPE_ID as RULES_SCOPE_ID, ENVIRONMENT_ANCHOR};
use crate::storage::Storage;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "rules",
    about = "The colony's rule environment: positive sensors, text rules, one driver (shadow mode)"
)]
pub struct Cli {
    /// Pile the sensors read Compass and Quota from.
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
    /// One tick: sense, publish the tick, settle the driver, report.
    Tick {
        /// Pile holding the rules collection; defaults to --pile.
        #[arg(long)]
        rules_pile: Option<PathBuf>,
        /// Persona tag whose goals ignition considers (ignite.sh's OWNER).
        #[arg(long, env = "RULES_OWNER")]
        owner: String,
        /// ignite.sh's state directory (its `ignited` list, the `asked` marker).
        #[arg(long, env = "RULES_IGNITE_STATE")]
        ignite_state: PathBuf,
        /// Most driver polls in one tick; each poll is itself bounded.
        #[arg(long, default_value_t = 16)]
        max_polls: usize,
        /// Also run ignite.sh --dry-run and ask-jp-due.sh, and compare.
        #[arg(long)]
        compare: bool,
        /// ignite.sh, for --compare.
        #[arg(long, env = "RULES_IGNITE_SCRIPT")]
        ignite_script: Option<PathBuf>,
        /// ask-jp-due.sh, for --compare.
        #[arg(long, env = "RULES_ASK_SCRIPT")]
        ask_script: Option<PathBuf>,
        /// Append one JSON line per tick here.
        #[arg(long)]
        log: Option<PathBuf>,
    },
}

fn ignited(state: &Path) -> Result<BTreeSet<Id>> {
    let path = state.join("ignited");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    Ok(text.split_whitespace().filter_map(Id::from_hex).collect())
}

fn last_asked(state: &Path) -> Result<Option<i64>> {
    let path = state.join("asked");
    match std::fs::metadata(&path) {
        Ok(meta) => Ok(Some(
            meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs() as i64
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

fn hex(ids: impl IntoIterator<Item = Id>) -> Vec<String> {
    ids.into_iter().map(|id| format!("{id:x}")).collect()
}

/// What ignite.sh --dry-run printed: its pick, or its reason for none.
fn ignite_dry_run(script: &Path) -> Result<(Option<String>, String)> {
    let output = Process::new(script)
        .arg("--dry-run")
        .output()
        .with_context(|| format!("run {} --dry-run", script.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("").to_owned();
    let pick = line
        .split("would ignite goal ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_owned);
    Ok((pick, format!("{line} (exit {:?})", output.status.code())))
}

fn execute(cli: Cli, output: &mut Out<'_>) -> Result<()> {
    let Command::Tick {
        rules_pile,
        owner,
        ignite_state,
        max_polls,
        compare,
        ignite_script,
        ask_script,
        log,
    } = cli.command;

    // Sense: one read of Compass and Quota, then the two local state files.
    let states = Compass::with_storage(Storage::new(cli.pile.clone(), cli.key.clone()))
        .goal_states()
        .context("sense compass")?;
    let view = Quota::with_storage(Storage::new(cli.pile.clone(), cli.key.clone()))
        .show()
        .context("sense quota")?;
    let now_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let sensed = Sensed {
        eligible: eligible_goals(&states, &owner, &ignited(&ignite_state)?),
        quota_ok: quota_ok(&view),
        ask_open: open_asks(&states),
        ask_quiet_since: ask_quiet_since(last_asked(&ignite_state)?, now_unix),
    };
    // The scripts read the same world as close in time as possible.
    let scripts = if compare {
        let (Some(ignite_script), Some(ask_script)) = (ignite_script, ask_script) else {
            bail!("--compare needs --ignite-script and --ask-script (or RULES_IGNITE_SCRIPT and RULES_ASK_SCRIPT)");
        };
        let ignite = ignite_dry_run(&ignite_script)?;
        let ask_due = Process::new(&ask_script)
            .arg("--due")
            .output()
            .with_context(|| format!("run {}", ask_script.display()))?
            .status
            .code();
        Some((ignite, ask_due))
    } else {
        None
    };

    let at = crate::clock::point_now()?;
    let tick = tick_fragment(at, &sensed)?;
    let tick_id = tick.root().expect("a tick exports one root");
    let rules_storage = Storage::new(rules_pile.unwrap_or(cli.pile), cli.key);
    let (settled, seeded, requests, nudges) =
        rules_storage.with_store(|store, signer, runtime| {
            let root = rules_storage.open_collection_write(
                store,
                RULES_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            store.commit(root, signer, tick.clone())?;
            store.flush()?;
            let bounds = Bounds {
                firings: 4096,
                chunk: 64,
                time: Duration::from_secs(20),
            };
            let mut driver =
                CollectionDriver::new(root, ENVIRONMENT_ANCHOR, signer.clone(), bounds);
            let mut settled = settle(&mut driver, store, max_polls)?;
            // The rules the driver runs are the pile's current reading. The
            // compiled-in text is only the seed, published when no reading exists.
            let seeded = driver.environment().reading() == Some(&Resolution::Empty);
            if seeded {
                store.commit(root, signer, program()?)?;
                store.flush()?;
                settled = settle(&mut driver, store, max_polls)?;
            }
            let facts = driver.environment().facts();
            Ok((
                settled,
                seeded,
                run_requests_in(facts, tick_id),
                nudges_in(facts, tick_id),
            ))
        })?;

    let requested: BTreeSet<Id> = requests.values().copied().collect();
    let mut record = serde_json::json!({
        "at": chrono_now(now_unix),
        "tick": format!("{tick_id:x}"),
        "sensed": {
            "eligible": hex(sensed.eligible.iter().copied()),
            "quota_ok": sensed.quota_ok.map(|id| format!("{id:x}")),
            "ask_open": hex(sensed.ask_open.iter().copied()),
            "ask_quiet_since": sensed.ask_quiet_since,
        },
        "rules": {
            "run_requests": requests.iter().map(|(r, g)| serde_json::json!({"request": format!("{r:x}"), "goal": format!("{g:x}")})).collect::<Vec<_>>(),
            "nudges": nudges.iter().map(|(n, qs)| serde_json::json!({"nudge": format!("{n:x}"), "about": hex(qs.iter().copied())})).collect::<Vec<_>>(),
        },
        "driver": {
            "polls": settled.polls, "firings": settled.firings, "commits": settled.commits,
            "outcome": settled.outcome, "settled": settled.settled, "seeded": seeded,
        },
    });
    if let Some(((pick, line), ask_due)) = scripts {
        let ignite_verdict = match &pick {
            Some(goal) if requested.iter().any(|g| format!("{g:x}") == *goal) => "match",
            Some(_) => "MISMATCH: ignite's pick is not among the rules' requests",
            None if line.contains("spawn unit(s) already running") => {
                "differs by design: ignite held by spawn room, which is the effector's admission"
            }
            None if requested.is_empty() => "match",
            None => "MISMATCH: rules request but ignite picks nothing",
        };
        let due = ask_due == Some(0);
        let ask_verdict = if due == !nudges.is_empty() {
            "match"
        } else {
            "MISMATCH"
        };
        record["compare"] = serde_json::json!({
            "ignite_dry_run": line, "ignite_pick": pick, "ignite": ignite_verdict,
            "ask_jp_due_exit": ask_due, "ask_jp": ask_verdict,
        });
    }
    let line = serde_json::to_string(&record)?;
    if let Some(log) = log {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .with_context(|| format!("open {}", log.display()))?;
        writeln!(file, "{line}")?;
    }
    output.line(serde_json::to_string_pretty(&record)?);
    Ok(())
}

fn chrono_now(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|t| t.format("%FT%TZ").to_string())
        .unwrap_or_else(|| unix.to_string())
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    crate::cli::with_output("rules", |output| execute(cli, output))
}
