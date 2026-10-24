//! Archive shell syntax, trace setup and explicit local source discovery.
use super::{parse_tai_timestamp, Archive, ImportSource, Publication};
use super::sessions::{SessionEvent, SessionOptions, SessionRoot, SessionSource, SessionStart};
use crate::archive_collection::ArchiveImportWriter;
use crate::blockdag::Identity;
use crate::out::Out;
use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::sync::Once;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::EnvFilter;
use triblespace::core::trible::Fragment;
#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "archive",
    about = "Import and query the canonical Archive block DAG"
)]
pub struct Cli {
    /// Path to the pile file to use.
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable signing-key file. Reads and writes never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    /// Enable tracing spans for projection and collection derivation.
    #[arg(long)]
    trace: bool,
    /// Optional tracing filter (defaults to `info`).
    #[arg(long)]
    trace_filter: Option<String>,
    #[command(subcommand)]
    pub(super) command: Option<Command>,
}

#[derive(Subcommand)]
pub(super) enum Command {
    /// Discover and import this host's quiet Codex/Claude Code sessions.
    ///
    /// Defaults: ~/.claude/projects, ~/.codex*/{sessions,archived_sessions},
    /// and both directories under CODEX_HOME. Explicit roots replace defaults.
    /// Recursive regular JSONL files include subagents. Symlinks/special entries
    /// and hard-linked sources are refused; same-adapter overlapping roots collapse.
    /// Each file remains its own native import unit, including Claude sidecars.
    ///
    /// The checkpoint is bound to this pile, selected collection and root set;
    /// uninitialized/changed bindings require --all or --since. Legacy script
    /// stamps are not inherited. Success advances only through the fixed quiet
    /// cutoff. --all removes the lower bound, NOT the quiet-window deferral.
    /// Backdated/copied files require --all. Native tail diagnostics still apply:
    /// success does not assert that every source byte was projected.
    ///
    /// No timer or scheduler runs here. Host-local ingestion contention exits 3;
    /// other failures exit 1 without checkpoint advancement, preserving earlier
    /// publications. This lock is NOT machine-wide GB10 admission or a resource
    /// limit: the caller must arrange those. No build/lock scripts are launched.
    IngestSessions {
        #[arg(long, conflicts_with = "since")]
        all: bool,
        /// Provisional lower bound (RFC3339), saved only after complete success.
        #[arg(long, value_parser = humantime::parse_rfc3339)]
        since: Option<std::time::SystemTime>,
        #[arg(long, default_value = "1h", value_parser = parse_finished_after)]
        quiet: std::time::Duration,
        /// Explicit Codex root (repeatable); replaces automatic root discovery.
        #[arg(long)]
        codex_root: Vec<PathBuf>,
        /// Explicit Claude Code root (repeatable); replaces automatic discovery.
        #[arg(long)]
        claude_root: Vec<PathBuf>,
        /// Host-local progress directory (default XDG_STATE_HOME/faculties/archive).
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Project one or more sources into the Archive as content-defined pieces.
    ///
    /// Several PATHS are projected in ONE process: the pile is opened once and
    /// each source still gets its own signed commits. Atomicity is per source,
    /// not per process — and the open costs ~9.3 s on the live pile against ~1 s
    /// of projection for a small rollout, so a per-file process put a 3,161-file
    /// Codex backfill at ~8.2 hours of pure opening.
    ///
    /// Each PATH is one unit. A directory is one unit across its files, which
    /// cuts differently from importing those files one by one: to converge
    /// with the daily ingest, pass the files themselves (for a long list,
    /// through `--paths-from`).
    Import {
        /// Source files (or a directory, where the adapter accepts one).
        #[arg(num_args = 0..)]
        path: Vec<PathBuf>,
        /// Read more paths from this file, one per line (`-` for standard
        /// input), and import each as its own unit in this one process.
        #[arg(long, value_name = "FILE")]
        paths_from: Option<PathBuf>,
        /// Source adapter used to interpret PATH.
        #[arg(long, value_enum, default_value = "claude-code")]
        source: CliImportSource,
        /// Treat a source file last modified longer ago than this (for
        /// example `1h`) as finished, and emit what it holds back at its tail:
        /// Claude Code results logged inside its last message, a trailing
        /// Codex prompt event. A transcript cannot say it is finished, so
        /// without this option such a tail waits for a later record. The
        /// modification time decides only when the tail is emitted, never any
        /// identity.
        #[arg(long, value_name = "DURATION", value_parser = parse_finished_after)]
        finished_after: Option<std::time::Duration>,
    },
    /// List the most recent source projections from one frozen Archive view.
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show one exact source projection by id prefix.
    Show { id: String },
    /// Show the complete canonical ancestor DAG of one source projection.
    Thread {
        id: String,
        /// Maximum accepted block count. Exceeding it is an error rather than
        /// silently hiding one fork.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Search canonical block text through an exact portable BM25 cover.
    Search {
        #[arg(help = "Query text. Use @path for file input or @- for stdin.")]
        text: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Ensure exact raw-Succinct and portable BM25 collection derives.
    Index,
    /// Replay canonical blocks as one interleaved temporal stream.
    Replay {
        /// `start <from-ts>`, `stop`, or nothing for the next batch.
        #[arg(value_name = "ACTION")]
        action: Vec<String>,
        /// Blocks per batch. The exact block cursor makes every boundary safe.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Include tool, thinking, event, and media-only blocks.
        #[arg(long)]
        with_tools: bool,
        /// Cursor owner. There is deliberately no default persona.
        #[arg(long, env = "PERSONA")]
        persona: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum CliImportSource {
    Agy,
    #[value(name = "chatgpt")]
    ChatGpt,
    ClaudeCode,
    ClaudeWeb,
    Codex,
    Copilot,
    Gemini,
}

impl From<CliImportSource> for ImportSource {
    fn from(value: CliImportSource) -> Self {
        match value {
            CliImportSource::Agy => Self::Agy,
            CliImportSource::ChatGpt => Self::ChatGpt,
            CliImportSource::ClaudeCode => Self::ClaudeCode,
            CliImportSource::ClaudeWeb => Self::ClaudeWeb,
            CliImportSource::Codex => Self::Codex,
            CliImportSource::Copilot => Self::Copilot,
            CliImportSource::Gemini => Self::Gemini,
        }
    }
}
fn init_tracing(enabled: bool, filter: Option<&str>) {
    static TRACE_INIT: Once = Once::new();
    if !enabled {
        return;
    }
    TRACE_INIT.call_once(|| {
        let env_filter = filter
            .map(EnvFilter::new)
            .or_else(|| {
                std::env::var("PLAYGROUND_ARCHIVE_TRACE_FILTER")
                    .ok()
                    .map(EnvFilter::new)
            })
            .unwrap_or_else(|| EnvFilter::new("info"));
        let _ = tracing_subscriber::fmt()
            .with_target(false)
            .without_time()
            .with_env_filter(env_filter)
            .with_span_events(FmtSpan::CLOSE)
            .try_init();
    });
}

/// Import every PATH through ONE opened writer, one unit of pieces per path.
///
/// The open is paid once for the whole command, whatever the source. Opening
/// grows with the pile (~9.3 s per open on the 44 GB live pile, 2026-09-01)
/// while a small source projects in well under a second, so one process per
/// path spends most of its time opening. Atomicity is per path, never per
/// process.
///
/// A failure names the path that caused it and stops the command. Every path
/// before it stays committed, each its own atomic publication. The failing
/// path's staged facts are dropped unpublished: `close` commits nothing after
/// an error, and payload bytes it already appended stay unreachable until a
/// later COMMIT names them. Paths after it are not read; rerunning the same
/// command republishes nothing for the paths already committed.
pub(super) fn import_paths(
    pile: &Path,
    key: Option<&Path>,
    paths: &[PathBuf],
    source: CliImportSource,
    finished_before: Option<std::time::SystemTime>,
    out: &mut Out<'_>,
) -> Result<()> {
    let mut writer = ArchiveImportWriter::open(pile, key)?;
    let total = paths.len();
    let result = paths.iter().enumerate().try_for_each(|(index, path)| {
        if total > 1 {
            eprintln!("[{}/{total}] {}", index + 1, path.display());
        }
        import_into(&mut writer, pile, path, source, finished_before, out)
            .with_context(|| format!("import {}", path.display()))
    });
    writer.close(result)
}

/// Project one source PATH into the open writer and publish its pieces.
///
/// The writer reads nothing back: an unchanged source cuts into the pieces it
/// cut before, and publishing them again appends nothing to the pile. The
/// report says how many pieces and COMMITs the path published and how many
/// bytes the pile file grew by while it did.
fn import_into(
    writer: &mut ArchiveImportWriter,
    pile: &Path,
    path: &Path,
    source: CliImportSource,
    finished_before: Option<std::time::SystemTime>,
    out: &mut Out<'_>,
) -> Result<()> {
    let before = pile_length(pile)?;
    let stage = |fragment: Fragment, identity: Identity, source_path: &Path| {
        writer
            .stage(fragment, identity)
            .with_context(|| format!("stage {}", source_path.display()))
    };
    let summary = super::local::project_path(path, source.into(), finished_before, stage)?;
    let unit = writer.commit_unit()?;
    let appended = pile_length(pile)?.saturating_sub(before);
    summary.write(
        &Publication {
            unit,
            appended: Some(appended),
        },
        out,
    )
}

fn pile_length(pile: &Path) -> Result<u64> {
    Ok(std::fs::metadata(pile)
        .with_context(|| format!("measure pile {}", pile.display()))?
        .len())
}

/// The paths of a `--paths-from` list: one per line, kept verbatim except
/// for a line ending's carriage return; lines that are empty or only
/// whitespace are skipped, while a real name keeps any spaces around it. A
/// line that is not UTF-8 refuses the whole list, naming the line, before
/// anything is imported.
fn paths_from_list(bytes: &[u8]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = std::str::from_utf8(line)
            .map_err(|_| anyhow!("--paths-from line {} is not UTF-8", index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        paths.push(PathBuf::from(line));
    }
    Ok(paths)
}

/// A `--finished-after` age such as `90s`, `1h` or `2days`.
fn parse_finished_after(raw: &str) -> std::result::Result<std::time::Duration, String> {
    humantime::parse_duration(raw).map_err(|error| error.to_string())
}

pub fn execute(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let Some(command) = cli.command else {
        out.line(Cli::command().render_help().to_string())?;
        return Ok(());
    };
    let archive = Archive::new(cli.pile.clone(), cli.key.clone());
    match command {
        Command::IngestSessions { all, since, quiet, codex_root, claude_root, state_dir } => {
            let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory for session discovery/state"))?;
            let roots = if codex_root.is_empty() && claude_root.is_empty() {
                let codex_home = std::env::var_os("CODEX_HOME").filter(|s| !s.is_empty()).map(PathBuf::from);
                super::sessions::default_roots(&home, codex_home.as_deref())?
            } else {
                claude_root.into_iter().map(|path| SessionRoot { source: SessionSource::ClaudeCode, path })
                    .chain(codex_root.into_iter().map(|path| SessionRoot { source: SessionSource::Codex, path })).collect()
            };
            let state_dir = state_dir.unwrap_or_else(|| std::env::var_os("XDG_STATE_HOME")
                .filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".local/state"))
                .join("faculties/archive"));
            let start = if all { SessionStart::All } else if let Some(time) = since { SessionStart::Since(time) } else { SessionStart::Incremental };
            archive.ingest_sessions(&SessionOptions { roots, state_dir, start, quiet, now: std::time::SystemTime::now() }, |event| {
                match event {
                    SessionEvent::Deferred(path) => out.line(format!("Deferred (quiet window): {:?}", path)),
                    SessionEvent::Importing(path) => out.line(format!("Importing session: {:?}", path)),
                    SessionEvent::Imported { receipt, .. } => receipt.write(out),
                    SessionEvent::Complete(report) => out.line(format!(
                        "Session pass: {} imported, {} deferred, {} unchanged, {} other files; {} pieces in {} COMMITs (publication work, not unique growth); collection {}; checkpoint cutoff {}. Native qualifiers above apply; not continuous/complete-byte coverage. Local ingestion lock only; host admission/resources are caller-owned.",
                        report.imported, report.deferred, report.unchanged, report.other_files,
                        report.pieces, report.commits, report.collection, humantime::format_rfc3339(report.cutoff))),
                }
            })?;
            Ok(())
        }
        Command::Import {
            mut path,
            paths_from,
            source,
            finished_after,
        } => {
            if let Some(list) = paths_from {
                let bytes = if list.as_os_str() == "-" {
                    let mut bytes = Vec::new();
                    std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)
                        .context("read paths from standard input")?;
                    bytes
                } else {
                    std::fs::read(&list)
                        .with_context(|| format!("read paths from {}", list.display()))?
                };
                path.extend(paths_from_list(&bytes)?);
            }
            if path.is_empty() {
                bail!("archive import needs at least one PATH or a --paths-from list");
            }
            let finished_before = finished_after
                .map(|age| {
                    std::time::SystemTime::now()
                        .checked_sub(age)
                        .ok_or_else(|| anyhow!("--finished-after reaches before the clock's epoch"))
                })
                .transpose()?;
            import_paths(
                &cli.pile,
                cli.key.as_deref(),
                &path,
                source,
                finished_before,
                out,
            )
        }
        Command::List { limit } => archive.list(limit, out),
        Command::Show { id } => archive.show(&id, out),
        Command::Thread { id, limit } => archive.thread(&id, limit, out),
        Command::Search { text, limit } => {
            archive.search(&crate::text_arg(&text, "search text")?, limit, out)
        }
        Command::Index => archive.index(out),
        Command::Replay {
            action,
            limit,
            with_tools,
            persona,
        } => {
            let persona=persona.as_deref().ok_or_else(||anyhow!("no persona: set $PERSONA or pass --persona; replay cursors are session bookkeeping"))?;
            if limit == 0 {
                bail!("replay limit must be at least 1");
            }
            match action.first().map(String::as_str) {
                Some("start") => {
                    if action.len() != 2 {
                        bail!("usage: archive replay start <YYYY-MM-DDTHH:MM:SS>");
                    }
                    let raw = &action[1];
                    archive.replay_start(persona, parse_tai_timestamp(raw)?)?;
                    out.line(format!("replay started at {raw} (persona {persona})"))
                }
                Some("stop") => {
                    if action.len() != 1 {
                        bail!("usage: archive replay stop");
                    }
                    archive.replay_stop(persona)?;
                    out.line(format!("replay stopped (persona {persona})"))
                }
                Some(other) => bail!("unknown replay action `{other}` (start/stop or nothing)"),
                None => archive.replay(persona, limit, with_tools, out),
            }
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.trace, cli.trace_filter.as_deref());
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    crate::cli::with_output("archive", |out| execute(cli, out))
}
