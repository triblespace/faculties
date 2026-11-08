//! Archive shell syntax, trace setup and explicit local source discovery.
use super::sessions::{SessionEvent, SessionOptions, SessionRoot, SessionSource, SessionStart};
use super::{parse_tai_timestamp, Archive, ImportSource, Publication, Reading};
use crate::archive_collection::ArchiveImportWriter;
use crate::blockdag::Identity;
use crate::out::Out;
use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::sync::Once;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::EnvFilter;
use triblespace::core::repo::async_store::{AsyncBlobStoreAcquire, AsyncBlobStoreGet};
use triblespace::core::repo::Store;
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
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    /// Enable tracing spans for projection and collection derivation.
    #[arg(long)]
    trace: bool,
    /// Optional tracing filter (defaults to `info`).
    #[arg(long)]
    trace_filter: Option<String>,
    /// The provider whose model reads this output (`anthropic`, `google` or
    /// `openai`), as the caller declares it: advisory, never verified. A
    /// recorded classifier stop withholds what it followed from a reader of
    /// the stopped provider and from one that names none or another value;
    /// another provider's reader gets the content with a note.
    #[arg(long, global = true, env = "READER_PROVIDER")]
    reader_provider: Option<String>,
    #[command(subcommand)]
    pub(super) command: Option<Command>,
}

#[derive(Subcommand)]
pub(super) enum Command {
    /// Discover and import this host's Codex/Claude Code sessions.
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
    /// cutoff. A file written within the quiet window is live: it is imported
    /// up to its last finished record (a trailing partial line and an
    /// unfinished message's tail are held back) and selected again next pass,
    /// so a conversation that never goes quiet is still archived. --quiet only
    /// decides when a file counts as finished and its held-back tail is flushed.
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
        /// Age after which a file counts as finished (its held-back tail is
        /// flushed); younger files are imported live up to their last finished record.
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
    /// Show one source projection (receipt) or one node by id prefix; a
    /// search hit's leading id is a node.
    Show {
        id: String,
        /// Print content a recorded classifier stop followed, which a read
        /// otherwise withholds and names in its place.
        #[arg(long)]
        force: bool,
    },
    /// Show the complete canonical ancestor DAG of one source projection
    /// (receipt) or one node, by id prefix; a search hit's leading id is a
    /// node.
    Thread {
        id: String,
        /// Maximum accepted block count. Exceeding it is an error rather than
        /// silently hiding one fork.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Show only the newest N ancestors instead: those the fewest steps
        /// back from the start (ties by id), in reading order. The walk stops
        /// after N, so --limit does not apply.
        #[arg(long)]
        tail: Option<usize>,
        /// Print content a recorded classifier stop followed, which a read
        /// otherwise withholds and names in its place.
        #[arg(long)]
        force: bool,
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
        /// Print content a recorded classifier stop followed, which a read
        /// otherwise withholds and names in its place.
        #[arg(long)]
        force: bool,
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

/// Selected imports borrow the exact routed owner instead of reopening an
/// unrouted writer. The legacy entry point keeps its existing opening and
/// admission behavior when no workspace was selected.
fn import_paths_with_storage(
    storage: &crate::storage::Storage,
    paths: &[PathBuf],
    source: CliImportSource,
    finished_before: Option<std::time::SystemTime>,
    out: &mut Out<'_>,
) -> Result<()> {
    if storage.collection_routes().is_none() {
        return import_paths(
            storage.path(),
            storage.key_path(),
            paths,
            source,
            finished_before,
            out,
        );
    }
    storage.with_store(|pile, signer, runtime| {
        let mut writer =
            ArchiveImportWriter::from_store_with_storage(pile, signer, runtime.clone(), storage)?;
        let total = paths.len();
        for (index, path) in paths.iter().enumerate() {
            if total > 1 {
                eprintln!("[{}/{total}] {}", index + 1, path.display());
            }
            import_into(
                &mut writer,
                storage.path(),
                path,
                source,
                finished_before,
                out,
            )
            .with_context(|| format!("import {}", path.display()))?;
        }
        // Each successful path committed its unit. On error the borrowed
        // writer drops its unfinished staging; Storage closes the owner.
        Ok(())
    })
}

/// Project one source PATH into the open writer and publish its pieces.
///
/// The writer reads nothing back: an unchanged source cuts into the pieces it
/// cut before, and publishing them again appends nothing to the pile. The
/// report says how many pieces and COMMITs the path published and how many
/// bytes the pile file grew by while it did.
fn import_into<P>(
    writer: &mut ArchiveImportWriter<P>,
    pile: &Path,
    path: &Path,
    source: CliImportSource,
    finished_before: Option<std::time::SystemTime>,
    out: &mut Out<'_>,
) -> Result<()>
where
    P: Store + AsyncBlobStoreAcquire + Send,
    P::Snapshot: AsyncBlobStoreGet,
{
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

/// One-shot storage: every operation opens and closes its own store.
pub fn execute(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let storage = if cli.command.is_none() {
        crate::storage::Storage::new(cli.pile.clone(), cli.key.clone())
    } else {
        cli.workspace.storage(cli.pile.clone(), cli.key.clone())?
    };
    execute_in(&storage, cli, out)
}

/// The process's whole body: one store, closed for process exit.
fn execute_to_exit(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    (if cli.command.is_none() {
        crate::storage::Storage::new(cli.pile.clone(), cli.key.clone())
    } else {
        cli.workspace.storage(cli.pile.clone(), cli.key.clone())?
    })
    .scope_to_exit(|storage| execute_in(storage, cli, out))
}

fn execute_in(storage: &crate::storage::Storage, cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let Some(command) = cli.command else {
        out.line(Cli::command().render_help().to_string())?;
        return Ok(());
    };
    let archive = Archive::with_storage(storage.clone());
    let reading = |force| Reading {
        force,
        provider: cli.reader_provider.as_deref(),
    };
    match command {
        Command::IngestSessions {
            all,
            since,
            quiet,
            codex_root,
            claude_root,
            state_dir,
        } => {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow!("no home directory for session discovery/state"))?;
            let roots = if codex_root.is_empty() && claude_root.is_empty() {
                let codex_home = std::env::var_os("CODEX_HOME")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                super::sessions::default_roots(&home, codex_home.as_deref())?
            } else {
                claude_root
                    .into_iter()
                    .map(|path| SessionRoot {
                        source: SessionSource::ClaudeCode,
                        path,
                    })
                    .chain(codex_root.into_iter().map(|path| SessionRoot {
                        source: SessionSource::Codex,
                        path,
                    }))
                    .collect()
            };
            let state_dir = state_dir.unwrap_or_else(|| {
                std::env::var_os("XDG_STATE_HOME")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/state"))
                    .join("faculties/archive")
            });
            let start = if all {
                SessionStart::All
            } else if let Some(time) = since {
                SessionStart::Since(time)
            } else {
                SessionStart::Incremental
            };
            archive.ingest_sessions(&SessionOptions { roots, state_dir, start, quiet, now: std::time::SystemTime::now() }, |event| {
                match event {
                    SessionEvent::Live(path) => out.line(format!("Live (written within the quiet window; importing up to its last finished record): {:?}", path)),
                    SessionEvent::Importing(path) => out.line(format!("Importing session: {:?}", path)),
                    SessionEvent::Imported { receipt, .. } => receipt.write(out),
                    SessionEvent::Complete(report) => out.line(format!(
                        "Session pass: {} imported ({} live), {} unchanged, {} other files; {} pieces in {} COMMITs (publication work, not unique growth); collection {}; checkpoint cutoff {}.{} Native qualifiers above apply; not continuous/complete-byte coverage. Local ingestion lock only; host admission/resources are caller-owned.",
                        report.imported, report.live, report.unchanged, report.other_files,
                        report.pieces, report.commits, report.collection, humantime::format_rfc3339(report.cutoff),
                        if report.rebound { " The pile at the bound path is a different file than last pass and still holds the checkpoint's last published COMMIT; the checkpoint now names this file." } else { "" })),
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
            import_paths_with_storage(&storage, &path, source, finished_before, out)
        }
        Command::List { limit } => archive.list(limit, reading(false), out),
        Command::Show { id, force } => archive.show(&id, reading(force), out),
        Command::Thread {
            id,
            limit,
            tail,
            force,
        } => archive.thread(&id, limit, tail, reading(force), out),
        Command::Search { text, limit } => archive.search(
            &crate::text_arg(&text, "search text")?,
            limit,
            reading(false),
            out,
        ),
        Command::Index => archive.index(out),
        Command::Replay {
            action,
            limit,
            with_tools,
            force,
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
                None => archive.replay(persona, limit, with_tools, reading(force), out),
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
    crate::cli::with_output("archive", |out| execute_to_exit(cli, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, CollectionStoreExt};
    use triblespace::core::metadata;
    use triblespace::core::repo::SnapshotSource;
    use triblespace::prelude::*;

    #[test]
    fn workspace_is_global_and_no_command_does_not_resolve_it() {
        let revision = genid().id;
        let id = format!("{revision:X}");
        for args in [
            vec![
                "archive",
                "--pile",
                "/absent.pile",
                "--workspace",
                &id,
                "list",
            ],
            vec![
                "archive",
                "--pile",
                "/absent.pile",
                "list",
                "--workspace",
                &id,
            ],
        ] {
            assert_eq!(
                Cli::try_parse_from(args).unwrap().workspace.workspace,
                Some(revision)
            );
        }
        let cli =
            Cli::try_parse_from(["archive", "--pile", "/absent.pile", "--workspace", &id]).unwrap();
        execute(cli, &mut Out::new(&mut |_| Ok(()))).unwrap();
        let help = Cli::try_parse_from(["archive", "--workspace", &id, "--help"])
            .err()
            .unwrap();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn local_path_import_uses_the_selected_root_and_missing_role_never_falls_back() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("archive.pile");
        let key = directory.path().join("archive.key");
        std::fs::File::create(&pile).unwrap();
        let signer = crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let storage = crate::storage::Storage::new(pile, Some(key));
        let scope = crate::schemas::blockdag::DEFAULT_SCOPE_ID;
        let (legacy, selected) = storage
            .with_store(|store, _, _| {
                let legacy = crate::collection_names::open(store, scope, signer.verifying_key())?;
                let selected = store.collection(
                    crate::collection_names::require_name(scope),
                    CollectionPolicy::new(
                        AdmissionPolicy::Open,
                        AdmissionPolicy::direct(signer.verifying_key()),
                    ),
                )?;
                Ok((legacy, selected))
            })
            .unwrap();
        assert_ne!(legacy.handle(), selected.handle());
        let source = directory.path().join("source.jsonl");
        std::fs::write(
            &source,
            concat!(
                "{\"type\":\"user\",\"sessionId\":\"selected\",\"uuid\":\"first\",",
                "\"timestamp\":\"2026-03-01T15:34:01Z\",",
                "\"message\":{\"role\":\"user\",\"content\":\"selected workspace\"}}\n",
            ),
        )
        .unwrap();
        let workspace = storage
            .with_store(|store, signer, _| {
                use crate::schemas::{config::config, workspace};
                let config_collection = crate::collection_names::open(
                    store,
                    crate::schemas::config::DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                )?;
                let route = entity! { config::anchor: scope, config::selects: selected.handle() };
                let fragment = entity! {
                    metadata::tag: workspace::WORKSPACE, config::anchor: genid().id,
                    workspace::enrolled: true, metadata::name: "Archive fixture",
                    workspace::workspace_route*: route,
                };
                let revision = fragment.root().unwrap();
                store.commit(config_collection, signer, fragment)?;
                Ok(revision)
            })
            .unwrap();
        let revision = format!("{workspace:X}");
        let cli = Cli::try_parse_from([
            "archive",
            "--pile",
            storage.path().to_str().unwrap(),
            "--key",
            storage.key_path().unwrap().to_str().unwrap(),
            "import",
            source.to_str().unwrap(),
            "--workspace",
            &revision,
        ])
        .unwrap();
        execute(cli, &mut Out::new(&mut |_| Ok(()))).unwrap();
        storage
            .with_store(|store, _, _| {
                let snapshot = store.snapshot()?;
                let legacy_facts: TribleSet = legacy.read(&snapshot)?;
                let selected_facts: TribleSet = selected.read(&snapshot)?;
                assert!(
                    legacy_facts.is_empty(),
                    "workspace import must not write the legacy root"
                );
                assert!(!selected_facts.is_empty());
                Ok(())
            })
            .unwrap();
        let missing = storage.with_collections(BTreeMap::new());
        let before = std::fs::read(missing.path()).unwrap();
        let error = import_paths_with_storage(
            &missing,
            &[directory.path().join("missing-source")],
            CliImportSource::ClaudeCode,
            None,
            &mut Out::new(&mut |_| Ok(())),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unconfigured"), "{error:#}");
        assert_eq!(std::fs::read(missing.path()).unwrap(), before);
    }
}
