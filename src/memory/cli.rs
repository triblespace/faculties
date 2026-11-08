//! Tailored memory CLI grammar and host-only input/cache UX.
use super::operations::{parse_tai_timestamp, parse_time_range};
use super::{ChurnOptions, Memory};
use crate::memory_cover::{CoverOpts, CoverReport, DEFAULT_SIM_THRESHOLD};
use crate::out::Out;
use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, Parser};
use hifitime::Epoch;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "memory",
    about = "Show compacted context chunks (drill down by narrowing the time range).\n\n\
             Subcommands:\n  \
             memory <from>..<to>              — show best summary covering a time range\n  \
             memory raw <full-id>             — selected text bytes, without trimming or added newline\n  \
             memory meta <from>..<to>         — show structural metadata for a time range\n  \
             memory context [<budget>] [--chars N] [--chunk-overhead N] [--about <query>] [--filter <query>] [--remove <query>] [--sim-threshold <f>] [--tag <tag>]... [--without-tag <tag>]... — density-shaped, deliberately lossy recollection over journaled time in greedy SPACE order, fit to a CHARACTER budget (bare <budget>, --chars N, or the --tokens N alias all count CHARACTERS — there is no token estimate). A continuous logarithmic-age gradient maps each candidate length to an ideal temporal slot and chooses the actual range whose endpoints match best; wide old arcs emerge from that shape rather than a special coverage rule, so gaps, overlap, wobbling temporal centres, and unsampled detail are valid while the pile remains lossless. --chunk-overhead charges N additional character-equivalents per selected chunk for consumer framing/tokenization without changing stored summaries or rendered text; --about chooses the recollection most relevant to <query> by MEANING only when multiple memories have exactly the same temporal coverage (semantic through the WeMM index when WEMM_ROOT names the model and the index has rows; otherwise exact lexical BM25 is rebuilt automatically) and never changes the temporal sampler; --filter <query> keeps ONLY chunks whose positive similarity to <query> exceeds --sim-threshold (default 0.55); --remove <query> is the anti-filter — drops chunks whose similarity EXCEEDS the threshold (negate in the retrieval, NOT the query text; do not phrase a negation). --tag <tag> (repeatable) is the crisp counterpart of --filter: keeps ONLY memories carrying every given tag (AND); --without-tag <tag> (repeatable) drops memories carrying any of them; names are trimmed and lowercased as `create --tag` writes them, and no score or threshold is involved. They all compose. NOTE: gating is chunk-level — a surviving COARSE memory's pre-written summary may still mention removed material. Unindexed wordless images are kept (fail-open) with a stderr warning.\n  \
             memory cover start [--chars N] [--chunk-chars M] [--session KEY] — generate the context cover (exactly `memory context --chars N`; N=400000) and store it for cursor-chunked reading in ~M-char chunks (M=20000); state lives in `${XDG_CACHE_HOME:-~/.cache}/faculties/cover/<KEY>/`, NOT the pile\n  \
             memory cover continue [--session KEY] — print the next stored chunk and advance the cursor; the final chunk ends with `COVER COMPLETE K/K`\n  \
             memory cover status [--session KEY]  — one line: complete=<true|false> loaded=<i>/<K> chars=<X>/<Y>; exit 0 when complete, 1 when not (hook-friendly)\n  \
             memory cover reset [--session KEY]   — rewind the cursor to 0 (does NOT regenerate the stored cover)\n  \
             memory density [<grain>]        — inspect where the stored hierarchy is bushy, balanced, or coarse (a journal diagnostic; it does not drive recollection)\n  \
             memory search <query>           — exact lexical (BM25) search rebuilt from the frozen Memory view\n  \
             memory similar <query>           — semantic search: chunks, prose and images alike, ranked by MEANING in the one WeMM space (the rows are derived in the background by `faculties maintain`; a partial index says so) [needs the wemm build on a GB10 and WEMM_ROOT naming the complete root in the models collection]\n  \
             memory tag [<tag>]               — list the tags in use with their counts, or the memories carrying <tag> in time order (tag with `create --tag <tag>`)\n  \
             memory migrate-lens-tags [--dry-run] — one-off: give every memory written with the retired `--lens <theme>` the tag of the same name (append-only)\n  \
             memory list [<grain>]            — show chunk time-ranges only: containment outline, or one zoom layer (no content)\n  \
             memory check <grain>             — report coverage gaps at a coarseness level (chunks of width <= grain)\n  \
             memory churn [--chars N] [--steps K] [--step-units U] — replay density-shaped recollection over the pile's own observation history (K diagnostic steps back, default 160): per step the fill, previous rendered prefix surviving, and any unselected stretch longer than one quarter of LIFE\n  \
             memory create [--tag <tag>]... [<range>] <summary> — create a memory chunk, optionally tagged\n  \
             memory respan <id> <from>..<to>  — the same memory over corrected time coordinates: a new chunk with the identical text supersedes the old one, which stands aside from the cover and stays readable by id\n  \
             memory respan-instants [--dry-run] — give every zero-length memory the span its own text names, or a moment ending at its stamp; turn inverted ranges forward; one commit\n  \
             memory respan-seams [--dry-run]    — close one-second and one-minute seams between arcs written with rounded edges (an hour or wider): a coordinate correction, one commit\n  \
             memory image <when> <image-path> — create a WORDLESS image memory at a time-coordinate (`faculties maintain` places it beside text in `memory similar`)\n  \
             memory consolidate start <ts> | <ts> <summary> | stop — write chunks from an advancing edge ($PERSONA cursor)\n  \
             memory replay start <grain> [<from>] | [<count>] | stop — stream the memory at a zoom level ($PERSONA cursor)\n  \
             memory provenance <chunk-id>     — list cognition + archive events overlapping the chunk's time range\n\n\
             Time format: YYYY-MM-DDTHH:MM:SS..YYYY-MM-DDTHH:MM:SS (TAI)\n\
             Hex id prefixes also accepted as fallback."
)]
pub struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    /// Path to the pile file to use.
    #[arg(long, env = "PILE")]
    pub pile: PathBuf,
    /// Existing durable signing-key file. Reads and writes never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    pub key: Option<PathBuf>,
    /// One or more time ranges / id prefixes to show, or `turn <turn-id>`, or `create [<from>..<to>] <summary>`.
    #[arg(value_name = "ID", trailing_var_arg = true, allow_hyphen_values = true)]
    pub ids: Vec<String>,
    /// Index of the first literal positional token after an explicit `--`.
    #[arg(skip)]
    literal_from: Option<usize>,
}

// Memory deliberately keeps a manual trailing grammar. Move only whole
// selector arguments ahead of that grammar, never substrings of prose. An
// explicit `--` ends option recognition. The same path serves run and tests.
impl Cli {
    pub fn try_parse_from<I, T>(args: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let (args, literal_tail) = workspace_arguments(args);
        let mut cli = <Self as Parser>::try_parse_from(args)?;
        if let Some(tail) = literal_tail {
            let mut boundary = cli.ids.len().saturating_sub(tail);
            // Clap retains the delimiter once trailing capture has begun,
            // but consumes it when it precedes the first positional token.
            if boundary > 0 && cli.ids[boundary - 1] == "--" {
                boundary -= 1;
                cli.ids.remove(boundary);
            }
            cli.literal_from = Some(boundary);
        }
        Ok(cli)
    }

    fn parse() -> Self {
        Self::try_parse_from(std::env::args_os()).unwrap_or_else(|error| error.exit())
    }

    fn information_only(&self) -> bool {
        self.ids.is_empty()
            || matches!(
                self.ids.first().map(String::as_str),
                Some("create" | "cover")
            ) && self
                .ids
                .iter()
                .take(self.literal_from.unwrap_or(self.ids.len()))
                .skip(1)
                .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    }
}

fn workspace_arguments<I, T>(args: I) -> (Vec<std::ffi::OsString>, Option<usize>)
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString>,
{
    let mut input = args.into_iter().map(Into::into);
    let Some(program) = input.next() else {
        return (Vec::new(), None);
    };
    let mut selectors = vec![program];
    let mut rest = Vec::new();
    let mut literal_tail = None;
    while let Some(arg) = input.next() {
        if arg == "--" {
            rest.push(arg);
            let tail: Vec<_> = input.collect();
            literal_tail = Some(tail.len());
            rest.extend(tail);
            break;
        }
        if arg == "--workspace" {
            selectors.push(arg);
            if let Some(value) = input.next() {
                selectors.push(value);
            }
        } else if arg
            .to_str()
            .is_some_and(|arg| arg.starts_with("--workspace="))
        {
            selectors.push(arg);
        } else if matches!(arg.to_str(), Some("--pile" | "--key")) {
            rest.push(arg);
            if let Some(value) = input.next() {
                rest.push(value);
            }
        } else {
            rest.push(arg);
        }
    }
    selectors.extend(rest);
    (selectors, literal_tail)
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let exit_code = crate::cli::with_output("memory", |out| execute_to_exit(cli, out))?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// A nonzero successful value is the hook-friendly incomplete-cover status.
/// Output routing is finalized before run() applies that process exit code.
/// One-shot storage: every operation opens and closes its own store.
pub fn execute(cli: Cli, out: &mut Out<'_>) -> Result<i32> {
    let storage = if cli.information_only() {
        crate::storage::Storage::new(cli.pile.clone(), cli.key.clone())
    } else {
        cli.workspace.storage(cli.pile.clone(), cli.key.clone())?
    };
    execute_in(&storage, cli, out)
}

/// The process's whole body: one store, closed for process exit.
fn execute_to_exit(cli: Cli, out: &mut Out<'_>) -> Result<i32> {
    (if cli.information_only() {
        crate::storage::Storage::new(cli.pile.clone(), cli.key.clone())
    } else {
        cli.workspace.storage(cli.pile.clone(), cli.key.clone())?
    })
    .scope_to_exit(|storage| execute_in(storage, cli, out))
}

fn execute_in(storage: &crate::storage::Storage, cli: Cli, out: &mut Out<'_>) -> Result<i32> {
    if cli.ids.is_empty() {
        return out
            .line(Cli::command().render_long_help().to_string())
            .map(|()| 0);
    }
    let memory = Memory::with_storage(storage.clone());
    let args = &cli.ids[1..];
    let literal_from = cli.literal_from.map(|index| index.saturating_sub(1));
    let one = |usage: &str| -> Result<&str> {
        if args.len() != 1 {
            bail!("{usage}");
        }
        Ok(&args[0])
    };
    let dry_run = |usage: &str| -> Result<bool> {
        if args.iter().any(|arg| arg != "--dry-run") {
            bail!("{usage}");
        }
        Ok(!args.is_empty())
    };
    match cli.ids[0].as_str() {
        "raw" => {
            let input = one("usage: memory raw <full-id>")?;
            let id = triblespace::prelude::Id::from_hex(input)
                .context("memory raw requires one full nonzero 32-hex entity id")?;
            memory.raw(id, out)
        }
        "create" => cmd_create(&memory, args, literal_from, out),
        "respan" => cmd_respan(&memory, args, out),
        "respan-instants" => {
            memory.respan_instants(dry_run("usage: memory respan-instants [--dry-run]")?, out)
        }
        "respan-seams" => {
            memory.respan_seams(dry_run("usage: memory respan-seams [--dry-run]")?, out)
        }
        "image" => cmd_image(&memory, args, out),
        "meta" => memory.meta(one("usage: memory meta <id or range>")?, out),
        "provenance" => memory.provenance(one("usage: memory provenance <id>")?, out),
        "turn" => memory.turn(one("usage: memory turn <turn-id>")?, out),
        "consolidate" => cmd_consolidate(&memory, args, out),
        "replay" => cmd_replay(&memory, args, out),
        "search" => memory.search(&literal_or_input(args, "query")?, out),
        "similar" => memory.similar(&literal_or_input(args, "query")?, out),
        "context" => emit_context(memory.context(&context_options(args)?)?, out),
        "cover" => return super::cli_cover::execute(&memory, storage, args, literal_from, out),
        "tag" => memory.tag(args.first().map(String::as_str), out),
        "migrate-lens-tags" => {
            memory.migrate_lens_tags(dry_run("usage: memory migrate-lens-tags [--dry-run]")?, out)
        }
        "list" => memory.list(args.first().map(String::as_str), out),
        "check" => memory.check(one("usage: memory check <grain>")?, out),
        "density" => memory.density(args.first().map(String::as_str), out),
        "churn" => memory.churn(&churn_options(args)?, out),
        _ => memory.show_many(&cli.ids, out),
    }?;
    Ok(0)
}

/// The exact charged cover text is the answer. Each diagnostic is about
/// the cover (what it skipped, what a filter could not assess, how much of
/// the budget it filled), so it is a note beside it on every frontend.
pub(super) fn emit_context(report: CoverReport, out: &mut Out<'_>) -> Result<()> {
    out.text(report.text)?;
    note_diagnostics(report.diagnostics, out)
}

pub(super) fn note_diagnostics(diagnostics: Vec<String>, out: &mut Out<'_>) -> Result<()> {
    for diagnostic in diagnostics {
        out.note(diagnostic)?;
    }
    Ok(())
}

fn literal_or_input(args: &[String], label: &str) -> Result<String> {
    match args {
        [single] => crate::text_arg(single, label),
        [] => bail!("{label} is required"),
        _ => Ok(args.join(" ")),
    }
}

fn cmd_create(
    memory: &Memory,
    args: &[String],
    literal_from: Option<usize>,
    out: &mut Out<'_>,
) -> Result<()> {
    // A help flag must never be minted as memory content — print usage and stop
    // before any parsing, so `memory create --help` explains itself instead of
    // storing the literal "--help" as a chunk.
    if args
        .iter()
        .take(literal_from.unwrap_or(args.len()))
        .any(|a| a == "--help" || a == "-h")
    {
        out.line(format!(
            "usage: memory create [--tag <tag>]... [<from>..<to>] <summary...>\n\
             \n\
             Create a memory chunk and store it in the pile.\n\
             An optional time range as the first argument grounds the memory in\n\
             that period; without it, defaults to now. --tag <tag> (repeatable)\n\
             tags it for `memory tag <tag>`; a tagged memory sits in the\n\
             timeline like any other."
        ))?;
        return Ok(());
    }
    if args.is_empty() {
        bail!(
            "usage: memory create [<from>..<to>] <summary...>\n\
             \n\
             Create a memory chunk and store it in the pile.\n\
             An optional time range as the first argument grounds the\n\
             memory in that period. Without it, defaults to now.\n\
             References in prose: (memory:<from>..<to>) is a soft temporal\n\
             address, resolved by range query at read time, no fact minted.\n\
             [why it matters](memory:<hex>) is a hard reference to an exact\n\
             chunk, extracted into a queryable ctx::reference fact. Neither\n\
             affects the span or the hierarchy: containment relates chunks,\n\
             and the explicit range argument always wins."
        );
    }

    // Leading `--tag <tag>` options, repeatable. An unknown leading option is
    // refused rather than stored as the first word of the memory.
    let mut tags: Vec<String> = Vec::new();
    let mut option_tokens = literal_from.unwrap_or(args.len());
    let mut args = args;
    let is_option = |arg: &&String| {
        arg.strip_prefix("--")
            .is_some_and(|name| name.starts_with(|c: char| c.is_ascii_alphabetic()))
    };
    while let Some(first) = args
        .first()
        .filter(|arg| option_tokens > 0 && is_option(arg))
    {
        match (first.as_str(), args.get(1)) {
            ("--tag", Some(tag)) if option_tokens >= 2 => {
                tags.push(tag.clone());
                args = &args[2..];
                option_tokens -= 2;
            }
            _ => bail!("unknown option {first}: memory create [--tag <tag>]... [<from>..<to>] <summary...>"),
        }
    }
    if args.is_empty() {
        bail!(
            "summary text is required: memory create [--tag <tag>]... [<from>..<to>] <summary...>"
        );
    }

    // If the first argument looks like a time range, parse it.
    let mut explicit_range: Option<(Epoch, Epoch)> = None;
    let summary_start_idx;
    // A first argument shaped like a range IS the range: if it does not parse,
    // that is an error to fix, never a summary to store. (The old fallthrough
    // stored `2026-09-05T13:20:00Z..2026-09-05T13:33:00Z` as prose, three
    // times in one day.)
    let looks_like_range = |t: &str| {
        t.contains("..") && t.len() >= 10 && t.as_bytes()[4] == b'-' && t.as_bytes()[7] == b'-'
    };
    if option_tokens > 0 && looks_like_range(&args[0]) {
        explicit_range = Some(parse_time_range(&args[0]).with_context(|| {
            format!(
                "the first argument looks like a time range but does not parse: {}",
                args[0]
            )
        })?);
        summary_start_idx = 1;
    } else {
        summary_start_idx = 0;
    }

    // A single summary token may reference a file or stdin via the shared `@`
    // convention (`@-`, `@path`, `@@literal`); multiple tokens are a literal
    // space-joined summary, so every existing inline usage is unchanged. This
    // closes the footgun where `@- <<HEREDOC` silently stored the string "@-".
    let summary_tokens = &args[summary_start_idx..];
    let summary_text: String = if summary_tokens.len() == 1 {
        crate::text_arg(&summary_tokens[0], "summary")?
    } else {
        summary_tokens.join(" ")
    };
    if summary_text.is_empty() {
        bail!("summary text is required: memory create [<from>..<to>] <summary...>");
    }

    memory
        .create(&summary_text, explicit_range, &tags)?
        .emit(out)
}

fn cmd_respan(memory: &Memory, args: &[String], out: &mut Out<'_>) -> Result<()> {
    if args.len() != 2 || args.iter().any(|a| a == "--help" || a == "-h") {
        bail!(
            "usage: memory respan <id> <from>..<to>\n\
             \n\
             Write the same memory over corrected time coordinates: a new\n\
             chunk with the identical text supersedes <id>, the cover shows\n\
             the new range, and the old chunk stands aside but stays in the\n\
             journal, readable by id. Only the coordinates move; to say\n\
             something different, write another memory."
        );
    }
    memory
        .respan(&args[0], parse_time_range(&args[1])?)?
        .emit(out)
}

fn cmd_image(memory: &Memory, args: &[String], out: &mut Out<'_>) -> Result<()> {
    if args.len() != 2 {
        bail!(
            "usage: memory image <when> <image-path>\n\
             \n\
             Create a WORDLESS image memory chunk and store it in the pile.\n\
             <when> is a single TAI timestamp (YYYY-MM-DDTHH:MM:SS — a point\n\
             where start==end) or a `from..to` range. The image bytes are stored\n\
             as a blob; `faculties maintain` places it in the one WeMM space,\n\
             where it ranks in `memory similar` by meaning beside text memories.\n\
             Reference it from prose with [caption](memory:<hex>)."
        );
    }
    let when = &args[0];
    let image_path = Path::new(&args[1]);
    let range = if when.contains("..") {
        parse_time_range(when)?
    } else {
        let p = parse_tai_timestamp(when)?;
        (p, p)
    };
    let bytes = std::fs::read(image_path)
        .with_context(|| format!("read image {}", image_path.display()))?;
    if bytes.is_empty() {
        bail!("image file is empty: {}", image_path.display());
    }

    memory.image(&bytes, range)?.emit(out)?;
    out.line(format!(
        "({} image bytes stored; `faculties maintain` places it in the WeMM space)",
        bytes.len()
    ))?;
    Ok(())
}

fn context_options(args: &[String]) -> Result<CoverOpts> {
    // Parse `[<budget>] [--chars N] [--about <query words...>]`. The budget is a
    // CHARACTER count: a bare number, `--chars N`, or (as an alias) `--tokens N`
    // all set it directly — there is no separate token estimate anymore (the old
    // estimate ran ~2× off the real token count, which was confusing; characters
    // are exact). `--about` chooses among recollections with the exact same
    // temporal coverage; it never changes the temporal sampler.
    let mut budget_chars: usize = 200_000;
    let mut chunk_overhead: usize = 0;
    let mut about: Option<String> = None;
    // `--filter <query>` (include-only) and `--remove <query>` (anti-filter) gate
    // ELIGIBILITY by positive similarity to their query; `--sim-threshold <f>` is
    // the shared cosine cutoff (default `DEFAULT_SIM_THRESHOLD`).
    let mut filter_q: Option<String> = None;
    let mut remove_q: Option<String> = None;
    let mut sim_threshold: f32 = DEFAULT_SIM_THRESHOLD;
    // `--tag <tag>` / `--without-tag <tag>` (each repeatable, one argument per
    // occurrence as on `create --tag`) gate eligibility crisply by tag.
    let mut tags: Vec<String> = Vec::new();
    let mut without_tags: Vec<String> = Vec::new();
    // `--chars N` (canonical) / `--tokens N` (alias) name the budget explicitly;
    // when present either wins over the bare positional (a backward-compatible
    // fallback — it also means characters).
    let mut chars_explicit = false;
    {
        // A value flag consumes the following args up to the NEXT recognized flag
        // (or end), so multi-word queries stay unquoted AND `--about`/`--filter`/
        // `--remove` compose in any order.
        let is_flag = |s: &str| {
            matches!(
                s,
                "--about"
                    | "--filter"
                    | "--remove"
                    | "--tokens"
                    | "--chars"
                    | "--chunk-overhead"
                    | "--sim-threshold"
                    | "--tag"
                    | "--without-tag"
            )
        };
        let mut i = 0;
        while i < args.len() {
            if matches!(args[i].as_str(), "--about" | "--filter" | "--remove") {
                let mut j = i + 1;
                while j < args.len() && !is_flag(&args[j]) {
                    j += 1;
                }
                let q = args[i + 1..j].join(" ");
                let q = (!q.trim().is_empty()).then_some(q);
                match args[i].as_str() {
                    "--about" => about = q,
                    "--filter" => filter_q = q,
                    _ => remove_q = q,
                }
                i = j;
                continue;
            }
            if args[i] == "--tag" || args[i] == "--without-tag" {
                let flag = args[i].as_str();
                let tag = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("{flag} needs a tag name, e.g. `{flag} design`"))?;
                if flag == "--tag" {
                    tags.push(tag.clone());
                } else {
                    without_tags.push(tag.clone());
                }
                i += 2;
                continue;
            }
            if args[i] == "--sim-threshold" {
                let raw = args.get(i + 1).ok_or_else(|| {
                    anyhow!("--sim-threshold needs a number in [0,1], e.g. `--sim-threshold 0.55`")
                })?;
                sim_threshold = raw
                    .parse()
                    .map_err(|_| anyhow!("--sim-threshold expects a float, got `{raw}`"))?;
                i += 2;
                continue;
            }
            if args[i] == "--chunk-overhead" {
                let raw = args.get(i + 1).ok_or_else(|| {
                    anyhow!(
                        "--chunk-overhead needs a non-negative integer, e.g. `--chunk-overhead 64`"
                    )
                })?;
                chunk_overhead = raw.parse().map_err(|_| {
                    anyhow!("--chunk-overhead expects a non-negative integer, got `{raw}`")
                })?;
                i += 2;
                continue;
            }
            // `--tokens N` is a backward-compatible ALIAS for `--chars N` (the
            // budget is characters now; there is no separate token path).
            if args[i] == "--tokens" || args[i] == "--chars" {
                let flag = args[i].as_str();
                let raw = args.get(i + 1).ok_or_else(|| {
                    anyhow!("{flag} needs a number, e.g. `memory context --chars 80000`")
                })?;
                budget_chars = raw
                    .parse()
                    .map_err(|_| anyhow!("{flag} expects a positive integer, got `{raw}`"))?;
                chars_explicit = true;
                i += 2;
                continue;
            }
            if !chars_explicit {
                if let Ok(n) = args[i].parse::<usize>() {
                    budget_chars = n;
                }
            }
            i += 1;
        }
    }

    Ok(CoverOpts {
        budget_chars,
        chunk_overhead,
        raw_memory: false,
        about,
        filter: filter_q,
        remove: remove_q,
        sim_threshold,
        tags,
        without_tags,
    })
}

fn churn_options(args: &[String]) -> Result<ChurnOptions> {
    let mut budget: usize = 800_000;
    let mut steps: usize = 160;
    let mut step_units: i128 = 1;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = |i: usize| -> Result<&String> {
            args.get(i + 1)
                .ok_or_else(|| anyhow!("{} needs a value", args[i]))
        };
        match flag {
            "--chars" => budget = value(i)?.parse().context("--chars expects a number")?,
            "--steps" => steps = value(i)?.parse().context("--steps expects a number")?,
            "--step-units" => {
                step_units = value(i)?.parse().context("--step-units expects a number")?
            }
            other => bail!(
                "unknown flag {other}; usage: memory churn [--chars N] [--steps K] [--step-units U]"
            ),
        }
        i += 2;
    }

    Ok(ChurnOptions {
        budget_chars: budget,
        steps,
        step_units,
    })
}

fn cmd_consolidate(memory: &Memory, args: &[String], out: &mut Out<'_>) -> Result<()> {
    let persona = comb_persona()?;
    match args.first().map(String::as_str) {
        Some("start") => {
            let raw = args
                .get(1)
                .context("usage: memory consolidate start <timestamp>")?;
            memory.consolidate_start(&persona, parse_tai_timestamp(raw)?)?;
            out.line(format!(
                "consolidation edge set to {raw} (persona {persona})"
            ))
        }
        Some("stop") => {
            memory.consolidate_stop(&persona)?;
            out.line(format!("consolidation stopped (persona {persona})"))
        }
        Some(until) => {
            memory
                .consolidate(
                    &persona,
                    parse_tai_timestamp(until)?,
                    &literal_or_input(&args[1..], "summary")?,
                )?
                .emit(out)?;
            out.line(format!("edge → {until}"))
        }
        None => bail!("usage: memory consolidate start <timestamp> | <timestamp> <summary> | stop"),
    }
}

fn cmd_replay(memory: &Memory, args: &[String], out: &mut Out<'_>) -> Result<()> {
    let persona = comb_persona()?;
    match args.first().map(String::as_str) {
        Some("start") => {
            let grain = args
                .get(1)
                .context("usage: memory replay start <grain> [<from>]")?;
            let from = args
                .get(2)
                .map(|raw| parse_tai_timestamp(raw))
                .transpose()?;
            memory.replay_start(&persona, grain, from)?;
            out.line(format!(
                "memory replay started at grain {grain} (persona {persona})"
            ))
        }
        Some("stop") => {
            memory.replay_stop(&persona)?;
            out.line(format!("memory replay stopped (persona {persona})"))
        }
        other => memory.replay(
            &persona,
            other
                .map(str::parse)
                .transpose()
                .context("expected a batch count")?
                .unwrap_or(5),
            out,
        ),
    }
}

fn comb_persona() -> Result<String> {
    std::env::var("PERSONA").map_err(|_| {
        anyhow!(
            "no persona: set $PERSONA.\n\
             Cursors are session bookkeeping — no agent is defaulted as \
             \"the\" rememberer; the memories themselves belong to the one \
             enduring memory stream and are never persona-scoped."
        )
    })
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    #[test]
    fn trailing_selector_is_routing_and_delimited_or_quoted_prose_stays_literal() {
        let id = format!("{:X}", triblespace::prelude::genid().id);
        for tail in [
            vec!["--workspace".to_owned(), id.clone()],
            vec![format!("--workspace={id}")],
        ] {
            let mut args = vec![
                "memory".to_owned(),
                "--pile".into(),
                "/absent.pile".into(),
                "create".into(),
                "text".into(),
            ];
            args.extend(tail);
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(
                cli.workspace.workspace,
                Some(crate::cli_workspace::parse_revision(&id).unwrap())
            );
            assert_eq!(cli.ids, ["create", "text"]);
        }
        let prose = format!("a literal --workspace {id} in one argument");
        let cli =
            Cli::try_parse_from(["memory", "--pile", "/absent.pile", "create", &prose]).unwrap();
        assert!(cli.workspace.workspace.is_none());
        assert_eq!(cli.ids, ["create", &prose]);
        let cli = Cli::try_parse_from([
            "memory",
            "--pile",
            "/absent.pile",
            "create",
            "text",
            "--",
            "--workspace",
            &id,
        ])
        .unwrap();
        assert!(cli.workspace.workspace.is_none());
        assert_eq!(cli.ids, ["create", "text", "--workspace", &id]);
        assert_eq!(cli.literal_from, Some(2));
    }

    #[test]
    fn delimited_workspace_and_help_tokens_are_stored_as_exact_literal_text() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("node.pile");
        let key = directory.path().join("node.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let memory = Memory::new(pile.clone(), Some(key.clone()));
        let id = format!("{:X}", triblespace::prelude::genid().id);
        for (tokens, expected) in [
            (
                vec!["create", "text", "--", "--workspace", id.as_str()],
                format!("text --workspace {id}"),
            ),
            (vec!["create", "--", "--help"], "--help".to_owned()),
            (vec!["--", "create", "--help"], "--help".to_owned()),
        ] {
            let mut args = vec![
                "memory",
                "--pile",
                pile.to_str().unwrap(),
                "--key",
                key.to_str().unwrap(),
            ];
            args.extend(tokens);
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(!cli.information_only());
            let mut receipt = String::new();
            execute(
                cli,
                &mut Out::new(&mut |part| {
                    if let crate::out::Part::Text { text } = part {
                        receipt.push_str(&text);
                    }
                    Ok(())
                }),
            )
            .unwrap();
            let raw_id = receipt
                .lines()
                .find_map(|line| line.strip_prefix("id: "))
                .unwrap();
            let id = triblespace::prelude::Id::from_hex(raw_id).unwrap();
            let mut text = String::new();
            memory
                .raw(
                    id,
                    &mut Out::new(&mut |part| {
                        if let crate::out::Part::Text { text: part } = part {
                            text.push_str(&part);
                        }
                        Ok(())
                    }),
                )
                .unwrap();
            assert_eq!(text, expected);
        }
    }

    #[test]
    fn manual_help_with_a_selected_revision_never_opens_bootstrap() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("absent.pile");
        let key = directory.path().join("absent.key");
        let id = format!("{:X}", triblespace::prelude::genid().id);
        for command in [
            vec!["create", "--help"],
            vec!["cover", "--help"],
            vec!["cover", "start", "--help"],
        ] {
            let mut args = vec![
                "memory",
                "--pile",
                pile.to_str().unwrap(),
                "--key",
                key.to_str().unwrap(),
            ];
            args.extend(command);
            args.extend(["--workspace", &id]);
            let cli = Cli::try_parse_from(args).unwrap();
            execute(cli, &mut Out::new(&mut |_| Ok(()))).unwrap();
            assert!(!pile.exists());
            assert!(!key.exists());
        }
    }
}
