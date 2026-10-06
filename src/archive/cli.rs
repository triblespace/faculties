//! Archive shell syntax, trace setup and explicit local source discovery.
use super::{parse_tai_timestamp, Archive, ImportSource, ImportSummary};
use crate::archive_collection::ArchiveImportWriter;
use crate::out::Out;
use crate::{
    archive_agy, archive_chatgpt, archive_claude_code, archive_claude_web, archive_codex,
    archive_copilot, archive_gemini,
};
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
    /// Project one or more sources into the Archive, publishing one COMMIT each.
    ///
    /// Several PATHS are projected in ONE process: the pile is opened once and
    /// each source still gets its own signed commit. Atomicity is per source,
    /// not per process — and the open costs ~9.3 s on the live pile against ~1 s
    /// of projection for a small rollout, so a per-file process put a 3,161-file
    /// Codex backfill at ~8.2 hours of pure opening.
    Import {
        /// Source files (or a directory, where the adapter accepts one).
        #[arg(required = true, num_args = 1..)]
        path: Vec<PathBuf>,
        /// Source adapter used to interpret PATH.
        #[arg(long, value_enum, default_value = "claude-code")]
        source: CliImportSource,
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

/// Import every PATH through ONE opened writer, one signed COMMIT per path.
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
    out: &mut Out<'_>,
) -> Result<()> {
    let mut writer = ArchiveImportWriter::open(pile, key)?;
    let total = paths.len();
    let result = paths.iter().enumerate().try_for_each(|(index, path)| {
        if total > 1 {
            eprintln!("[{}/{total}] {}", index + 1, path.display());
        }
        import_into(&mut writer, path, source, out)
            .with_context(|| format!("import {}", path.display()))
    });
    writer.close(result)
}

/// Project one source PATH into the open writer and publish it as one COMMIT.
///
/// `commit_unit` folds what it publishes into the writer's known facts, so a
/// later path skips whatever an earlier one already carried, just as a fresh
/// open would have: an unchanged source stages nothing and publishes nothing.
fn import_into(
    writer: &mut ArchiveImportWriter,
    path: &Path,
    source: CliImportSource,
    out: &mut Out<'_>,
) -> Result<()> {
    let mut stage = |fragment: Fragment, source_path: &Path| {
        writer
            .stage_fragment(fragment)
            .with_context(|| format!("stage {}", source_path.display()))
    };
    let summary = match source {
        CliImportSource::Agy => {
            archive_agy::project_path(path, |p| stage(p.fragment, &p.source_path))
                .map(ImportSummary::Agy)
        }
        CliImportSource::ChatGpt => {
            archive_chatgpt::project_path(path, |p| stage(p.fragment, &p.source_path))
                .map(ImportSummary::ChatGpt)
        }
        CliImportSource::ClaudeCode => {
            archive_claude_code::project_path(path, |p| stage(p.fragment, &p.source_path))
                .map(ImportSummary::ClaudeCode)
        }
        CliImportSource::ClaudeWeb => {
            archive_claude_web::project_path(path, |p| stage(p.fragment, &p.source_path))
                .map(ImportSummary::ClaudeWeb)
        }
        CliImportSource::Codex => {
            archive_codex::project_path(path, |p| stage(p.fragment, &p.source_path))
                .map(ImportSummary::Codex)
        }
        CliImportSource::Copilot => {
            archive_copilot::project_path(path, |p| stage(p.fragment, &p.source_path))
                .map(ImportSummary::Copilot)
        }
        CliImportSource::Gemini => {
            archive_gemini::project_path(path, |p| stage(p.fragment, &p.source_path))
                .map(ImportSummary::Gemini)
        }
    }?;
    let commit = writer.commit_unit()?;
    summary.write(commit.is_some(), out)
}

pub fn execute(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let Some(command) = cli.command else {
        out.line(Cli::command().render_help().to_string())?;
        return Ok(());
    };
    let archive = Archive::new(cli.pile.clone(), cli.key.clone());
    match command {
        Command::Import { path, source } => {
            import_paths(&cli.pile, cli.key.as_deref(), &path, source, out)
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
