//! Wiki command-line UX: Clap, host paths, and @file/@- expansion live here.

use super::operations::{
    ImportDocument, ImportPlan, ListOptions, PlanAttachment, PlanCommit, PlanRevision, Wiki,
};
use crate::out::Out;
use anyhow::{anyhow, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::prelude::Id;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "wiki",
    about = "A fork-visible knowledge wiki over a signed revision-DAG collection"
)]
struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    /// Path to the pile file.
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable signing-key file. Reads and writes never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create an unanchored native entry.
    Create {
        title: String,
        /// Content text. Use @path or @-.
        content: String,
        #[arg(long)]
        tag: Vec<String>,
        /// Permit well-formed links whose targets are not present yet.
        #[arg(long)]
        force: bool,
    },
    /// Join an entry's complete current frontier with a successor revision.
    Edit {
        id: String,
        content: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Replacement tag set; omitted means inherit the agreed current set.
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        force: bool,
    },
    /// Show what an id's entry says NOW, following the revision forward.
    ///
    /// A citation names a revision — immutable, pinned to the text its author
    /// read — and `wiki lint` deliberately never rewrites one forward, so a
    /// corpus that links liberally accumulates pinned ids the frontier has
    /// since moved past. Following the entry is therefore the reading a reader
    /// almost always wants, and it was behind an opt-in flag until 2026-08-27.
    /// Per citation carried by one production pile's live frontier
    /// (3264 entries, `cargo run --example reference_census`): 11823 of 14555
    /// (81.2%) named a superseded revision, and 99.6% of those carry text
    /// DIFFERING from what their entry says today. Freezing by default made
    /// that a silent wrong answer that looks exactly like a right one.
    ///
    /// `--exact` opts back into the frozen revision, for inspecting history.
    /// A forked entry prints every head rather than choosing one.
    Show {
        id: String,
        /// Show the named revision itself, not its entry's current frontier.
        #[arg(long)]
        exact: bool,
    },
    /// Print content without a metadata header. Fails on a fork.
    ///
    /// Follows the entry forward exactly as `show` does, so the two never
    /// disagree about what one id says; `--exact` pins the named revision.
    Export {
        id: String,
        /// Print the named revision itself, not its entry's current frontier.
        #[arg(long)]
        exact: bool,
    },
    /// Compare two deterministically ordered revisions in an entry.
    Diff {
        id: String,
        #[arg(long)]
        from: Option<usize>,
        #[arg(long)]
        to: Option<usize>,
    },
    Archive {
        id: String,
    },
    Restore {
        id: String,
    },
    Revert {
        id: String,
        #[arg(long)]
        to: usize,
    },
    /// Audit every frontier link, or show one entry's links when given an id.
    ///
    /// With no id this classifies the whole live frontier's citations: a
    /// target that never existed is a forward reference, one whose entry is
    /// archived is real breakage, and a legacy fragment anchor is a migration
    /// signal. Diagnostic by default; `--strict` is the opt-in exit code.
    ///
    /// With an id, incoming links are revision-scoped: a citation records what
    /// its author read, so a superseded revision that cited this page is still
    /// listed. `wiki show <revision>` says whether the citation survived.
    Links {
        id: Option<String>,
        /// Rows to print per class, and unreferenced entries to name.
        #[arg(long, default_value = "15")]
        top: usize,
        /// Exit non-zero when a link points into an archived entry. OPT-IN:
        /// nothing else here ever fails, forward references least of all.
        #[arg(long)]
        strict: bool,
    },
    List {
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        with_backlink_tag: Vec<String>,
        #[arg(long)]
        without_backlink_tag: Vec<String>,
        #[arg(long)]
        with_backlink_type: Vec<String>,
        #[arg(long)]
        without_backlink_type: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    History {
        id: String,
    },
    Tag {
        #[command(subcommand)]
        command: TagCommand,
    },
    Import {
        #[arg(required_unless_present = "plan")]
        path: Option<PathBuf>,
        /// Replay a translated foreign history from a JSON plan instead; see
        /// `ImportPlan`. Content and attachment paths are relative to the plan.
        #[arg(long, conflicts_with = "path")]
        plan: Option<PathBuf>,
        #[arg(long)]
        tag: Vec<String>,
    },
    Search {
        query: String,
        #[arg(long, short = 'c')]
        context: bool,
        #[arg(long)]
        all: bool,
    },
    /// Rank current revisions by meaning against the WeMM index, whose rows
    /// `faculties maintain` derives in the background; a partial index says
    /// so.
    Similar {
        query: String,
    },
    Batch {
        #[command(subcommand)]
        action: BatchAction,
    },
    Check {
        #[arg(long)]
        compile: bool,
    },
    /// Republish one exact revision from one workspace's Wiki into another's.
    ///
    /// The revision keeps its id, author and authorship time; the destination
    /// commit only records that this node curated it there. By default only
    /// that revision crosses: its parent revision ids stay in its identity as
    /// references, and its links and files stay references too. Every relation
    /// is listed as carried or as a reference. Repeating converges. The
    /// source is never written. `--workspace` does not apply here.
    Transfer {
        /// Exact 32-hex revision id; never a prefix.
        #[arg(value_parser = parse_exact_revision)]
        revision: Id,
        /// Exact source workspace revision from `faculties workspace list`.
        #[arg(long, value_parser = crate::cli_workspace::parse_revision)]
        from: Id,
        /// Exact destination workspace revision from `faculties workspace list`.
        #[arg(long, value_parser = crate::cli_workspace::parse_revision)]
        to: Id,
        /// Also carry every source ancestor of the revision.
        #[arg(long)]
        with_history: bool,
        /// Also carry the exact source revisions it cites (one hop).
        #[arg(long)]
        with_links: bool,
        /// Also carry the Files leaves it links, with their bytes.
        #[arg(long)]
        with_files: bool,
        /// Report the exact scope without publishing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Resolve one scheme:prefix line per input line.
    FixTruncated {
        input: String,
    },
    /// Apply markdown-to-Typst and reference normalization.
    Lint {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum TagCommand {
    Add { id: String, name: String },
    Remove { id: String, name: String },
    List,
    Mint { name: String },
}

#[derive(Subcommand)]
enum BatchAction {
    Export { dir: PathBuf },
    Import { dir: PathBuf },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    crate::cli::with_output("wiki", |out| execute(cli, out))
}

/// Parse explicit CLI arguments. This seam keeps frontend tests off process
/// stdout while preserving the same path/text expansion as the executable.
pub fn execute_from<I, T>(arguments: I, out: &mut Out<'_>) -> Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    execute(Cli::try_parse_from(arguments)?, out)
}

fn parse_exact_revision(value: &str) -> Result<Id, String> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("a transfer names an exact 32-hex revision id, never a prefix".into());
    }
    Id::from_hex(value).ok_or_else(|| "invalid revision id".into())
}

fn transfer_report(report: &super::TransferReport, out: &mut Out<'_>) -> Result<()> {
    use super::Presence;
    let outcome = if report.dry_run {
        "dry run, nothing published".to_owned()
    } else if report.publications == 0 {
        "nothing new to publish".to_owned()
    } else {
        format!("published {} commit(s)", report.publications)
    };
    out.line(format!(
        "transfer {:x} from {:X} to {:X}: {outcome}",
        report.revision, report.from, report.to
    ))?;
    if report.same_root() {
        out.line("both workspaces bind the same Wiki root")?;
    }
    out.line("carried:")?;
    for item in &report.carried {
        let kind = if item.reason == super::Reason::Attachment {
            "file"
        } else {
            "revision"
        };
        out.line(format!(
            "  {kind} {:x} ({}) {}: {} facts, {} new",
            item.id,
            item.reason.label(),
            serde_json::to_string(&item.label)?,
            item.facts,
            item.new_facts
        ))?;
    }
    if report.references.is_empty() {
        out.line("references: none")?;
    } else {
        out.line("references (only the id or hash crosses, never facts or bytes):")?;
        for reference in &report.references {
            let at = match reference.at_destination {
                Presence::Present => "present at destination",
                Presence::Absent => "absent at destination",
                Presence::Unknown => "destination has no Files role",
            };
            out.line(format!(
                "  {} {} from {:x}: {at}",
                reference.kind.label(),
                reference.target,
                reference.from
            ))?;
        }
    }
    out.line(format!(
        "new facts: {} Wiki, {} Files",
        report.new_wiki_facts, report.new_file_facts
    ))?;
    if report.verified {
        out.line("read back from the destination: every carried fact and text")?;
    }
    out.line(format!(
        "the source is unchanged; archiving it is separate: `wiki --workspace {:X} archive {:x}` archives that entry's whole source frontier, not only this revision",
        report.from, report.revision
    ))
}

fn transfer(cli: &Cli, command: &Command, out: &mut Out<'_>) -> Result<()> {
    let Command::Transfer {
        revision,
        from,
        to,
        with_history,
        with_links,
        with_files,
        dry_run,
    } = command
    else {
        unreachable!("only called for transfer");
    };
    if cli.workspace.workspace.is_some() {
        anyhow::bail!(
            "wiki transfer binds only --from and --to; unset --workspace / FACULTIES_WORKSPACE"
        );
    }
    let bind = |workspace: Id| {
        crate::cli_workspace::WorkspaceArgs {
            workspace: Some(workspace),
        }
        .storage(cli.pile.clone(), cli.key.clone())
        .map(Wiki::with_storage)
    };
    let source = bind(*from).context("bind the source workspace")?;
    let destination = bind(*to).context("bind the destination workspace")?;
    let report = source.transfer(
        &destination,
        *revision,
        super::TransferOptions {
            history: *with_history,
            links: *with_links,
            files: *with_files,
            dry_run: *dry_run,
        },
    )?;
    transfer_report(&report, out)
}

fn revision(out: &mut Out<'_>, id: Id) -> Result<()> {
    out.line(format!("revision {id:x}"))
}
fn tag_change(out: &mut Out<'_>, result: Option<Id>, name: &str, add: bool) -> Result<()> {
    match result {
        Some(id) => revision(out, id),
        None => out.line(format!(
            "already {} #{}",
            if add { "tagged" } else { "untagged" },
            name.trim().to_ascii_lowercase()
        )),
    }
}

fn execute(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let Some(command) = &cli.command else {
        return Ok(());
    };
    if matches!(command, Command::Transfer { .. }) {
        return transfer(&cli, command, out);
    }
    let Some(command) = cli.command else {
        return Ok(());
    };
    let wiki = Wiki::with_storage(cli.workspace.storage(cli.pile, cli.key)?);
    match command {
        Command::Create {
            title,
            content,
            tag,
            force,
        } => {
            let title = crate::text_arg(&title, "title")?;
            let content = crate::text_arg(&content, "content")?;
            revision(out, wiki.create(&title, &content, &tag, force)?)
        }
        Command::Edit {
            id,
            content,
            title,
            tag,
            force,
        } => {
            let title = title
                .map(|value| crate::text_arg(&value, "title"))
                .transpose()?;
            let content = content
                .map(|value| crate::text_arg(&value, "content"))
                .transpose()?;
            revision(
                out,
                wiki.edit(&id, content.as_deref(), title.as_deref(), &tag, force)?,
            )
        }
        Command::Show { id, exact } => out.text(wiki.show(&id, exact)?),
        Command::Export { id, exact } => {
            let export = wiki.export(&id, exact)?;
            let uri = export.uri();
            out.blob(export.bytes, "text/plain; charset=utf-8", uri)
        }
        Command::Diff { id, from, to } => out.text(wiki.diff(&id, from, to)?),
        Command::Archive { id } => tag_change(out, wiki.archive(&id)?, "archived", true),
        Command::Restore { id } => tag_change(out, wiki.restore(&id)?, "archived", false),
        Command::Revert { id, to } => revision(out, wiki.revert(&id, to)?),
        Command::Links { id, top, strict } => wiki.links(id.as_deref(), top, strict, out),
        Command::List {
            tag,
            with_backlink_tag,
            without_backlink_tag,
            with_backlink_type,
            without_backlink_type,
            all,
        } => out.text(wiki.list(&ListOptions {
            tags: tag,
            with_backlink_tag,
            without_backlink_tag,
            with_backlink_type,
            without_backlink_type,
            all,
        })?),
        Command::History { id } => out.text(wiki.history(&id)?),
        Command::Tag { command } => match command {
            TagCommand::Add { id, name } => {
                tag_change(out, wiki.tag(&id, &name, true)?, &name, true)
            }
            TagCommand::Remove { id, name } => {
                tag_change(out, wiki.tag(&id, &name, false)?, &name, false)
            }
            TagCommand::List => wiki.tags(out),
            TagCommand::Mint { name } => out.line(format!(
                "{:x}  {}",
                wiki.mint_tag(&name)?,
                name.trim().to_ascii_lowercase()
            )),
        },
        Command::Import {
            plan: Some(plan),
            tag,
            ..
        } => import_plan(&wiki, &plan, &tag, out),
        Command::Import { path, tag, .. } => import(
            &wiki,
            &path.expect("clap requires a path without --plan"),
            &tag,
            out,
        ),
        Command::Search {
            query,
            context,
            all,
        } => out.text(wiki.search(&query, context, all)?),
        Command::Similar { query } => out.text(wiki.similar(&query)?),
        Command::Batch {
            action: BatchAction::Export { dir },
        } => {
            let exports = wiki.export_all()?;
            fs::create_dir_all(&dir)?;
            for (id, content) in exports {
                fs::write(dir.join(format!("{id:x}.typ")), content)?;
            }
            Ok(())
        }
        Command::Batch {
            action: BatchAction::Import { dir },
        } => {
            let mut imports = Vec::new();
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().is_none_or(|ext| ext != "typ") {
                    continue;
                }
                let raw = path.file_stem().unwrap_or_default().to_string_lossy();
                let id = Id::from_hex(&raw)
                    .ok_or_else(|| anyhow!("invalid revision filename {}", path.display()))?;
                imports.push((id, fs::read_to_string(&path)?));
            }
            wiki.import_revisions(imports)
        }
        Command::Check { compile } => wiki.check(compile, out),
        Command::FixTruncated { input } => {
            wiki.fix_truncated(&crate::text_arg(&input, "input")?, out)
        }
        Command::Lint { fix, check } => wiki.lint(fix, check, out),
        Command::Transfer { .. } => unreachable!("transfer binds its own workspaces"),
    }
}

fn collect_typ_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        output.push(path.to_owned());
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_typ_files(&path, output)?;
        } else if path.extension().is_some_and(|ext| ext == "typ") {
            output.push(path);
        }
    }
    Ok(())
}

fn import(wiki: &Wiki, path: &Path, tags: &[String], out: &mut Out<'_>) -> Result<()> {
    let mut paths = Vec::new();
    collect_typ_files(path, &mut paths)?;
    paths.sort();
    let documents = paths
        .iter()
        .map(|path| {
            Ok(ImportDocument {
                title: path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                content: fs::read_to_string(path)
                    .with_context(|| format!("read {}", path.display()))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let ids = wiki.import_texts(documents, tags)?;
    for (id, path) in ids.into_iter().zip(paths) {
        out.line(format!("{id:x}  {}", path.display()))?;
    }
    Ok(())
}

/// The plan file: plain JSON, every path relative to the plan's directory.
///
/// ```json
/// { "bundle": "<32-hex Files id of the git bundle>",
///   "commits": [ { "commit": "<sha>", "author": "Name <email>",
///                  "date": "2019-03-01T10:00:00+01:00",
///                  "attachments": [ { "key": "uploads/a.png", "path": "uploads/a.png",
///                                     "file": "blobs/a.png", "media_type": "image/png" } ],
///                  "revisions": [ { "key": "c1:Home", "page": "home", "path": "Home.md",
///                                   "title": "Home", "content": "pages/c1-home.typ",
///                                   "predecessors": [] } ] } ] }
/// ```
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanFile {
    #[serde(default)]
    bundle: Option<String>,
    commits: Vec<PlanFileCommit>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanFileCommit {
    commit: String,
    author: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    attachments: Vec<PlanFileAttachment>,
    #[serde(default)]
    revisions: Vec<PlanFileRevision>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanFileAttachment {
    /// Defaults to `path`.
    #[serde(default)]
    key: Option<String>,
    path: String,
    file: PathBuf,
    /// Inferred from `path` when absent.
    #[serde(default)]
    media_type: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanFileRevision {
    key: String,
    page: String,
    path: String,
    title: String,
    content: PathBuf,
    #[serde(default)]
    predecessors: Vec<String>,
}

/// An RFC 3339 source date (git's `%aI`) as a point interval; the offset only
/// locates the instant, and the exact original stays in the source bundle.
pub(crate) fn parse_source_date(
    raw: &str,
) -> Result<triblespace::prelude::Inline<triblespace::prelude::inlineencodings::NsTAIInterval>> {
    use chrono::{Datelike, Timelike};
    let at = chrono::DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("source date '{raw}' is not RFC 3339"))?
        .with_timezone(&chrono::Utc);
    let epoch = hifitime::Epoch::maybe_from_gregorian_utc(
        at.year(),
        at.month() as u8,
        at.day() as u8,
        at.hour() as u8,
        at.minute() as u8,
        at.second() as u8,
        at.nanosecond(),
    )
    .map_err(|error| anyhow!("source date '{raw}': {error}"))?;
    crate::clock::point(epoch)
}

/// Read a plan file and everything it names into a resident [`ImportPlan`].
pub(crate) fn read_plan(path: &Path) -> Result<ImportPlan> {
    let text = fs::read_to_string(path).with_context(|| format!("read plan {}", path.display()))?;
    let file: PlanFile =
        serde_json::from_str(&text).with_context(|| format!("parse plan {}", path.display()))?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let bundle = file
        .bundle
        .map(|raw| {
            Id::from_hex(raw.trim())
                .ok_or_else(|| anyhow!("plan bundle '{raw}' is not a 32-hex Files id"))
        })
        .transpose()?;
    let mut commits = Vec::with_capacity(file.commits.len());
    for commit in file.commits {
        let authored_at = commit.date.as_deref().map(parse_source_date).transpose()?;
        let mut attachments = Vec::with_capacity(commit.attachments.len());
        for attachment in commit.attachments {
            let source = base.join(&attachment.file);
            let bytes = fs::read(&source).with_context(|| format!("read {}", source.display()))?;
            attachments.push(PlanAttachment {
                key: attachment.key.unwrap_or_else(|| attachment.path.clone()),
                media_type: attachment.media_type.unwrap_or_else(|| {
                    crate::files::infer_media_type(Path::new(&attachment.path)).to_owned()
                }),
                path: attachment.path,
                bytes: bytes.into(),
            });
        }
        let mut revisions = Vec::with_capacity(commit.revisions.len());
        for revision in commit.revisions {
            let source = base.join(&revision.content);
            revisions.push(PlanRevision {
                content: fs::read_to_string(&source)
                    .with_context(|| format!("read {}", source.display()))?,
                key: revision.key,
                page: revision.page,
                path: revision.path,
                title: revision.title,
                predecessors: revision.predecessors,
            });
        }
        commits.push(PlanCommit {
            commit: commit.commit,
            author: commit.author,
            authored_at,
            attachments,
            revisions,
        });
    }
    Ok(ImportPlan { bundle, commits })
}

fn import_plan(wiki: &Wiki, path: &Path, tags: &[String], out: &mut Out<'_>) -> Result<()> {
    let report = wiki.import_plan(read_plan(path)?, tags)?;
    for (key, id) in &report.revisions {
        out.line(format!("{id:x}  {key}"))?;
    }
    out.line(format!(
        "{} revisions in plan: {} new revisions, {} new provenance records, {} new files, {} publications",
        report.revisions.len(),
        report.new_revisions,
        report.new_provenance,
        report.new_files,
        report.publications,
    ))
}
