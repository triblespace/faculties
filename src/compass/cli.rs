//! Compass's explicit CLI grammar, file/stdin input, and output presentation.

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

use super::{render, AddOptions, Compass, ListOptions, NoteOptions};
use crate::out::Out;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "compass",
    about = "A small TribleSpace kanban faculty",
    long_about = "A small TribleSpace kanban faculty.\n\n\
        Addressing people: an `@label` in a goal title or note (a persona label, alias \
        or Relations group name, matched case-insensitively) mentions that person or \
        every member of that group, and `orient` reports it to them once. A mention \
        starts the text or follows a non-alphanumeric character, so `ada@example.org` \
        mentions nobody; an `@label` matching nobody is warned about on stderr and the \
        write still succeeds. A title or note that BEGINS with a mention must start \
        with `@@` (`@@ada please look`), because a leading `@` reads the text from a \
        file. Tagging a goal with a persona's own label assigns it: \
        that persona hears about other people's status changes and notes on it. Group \
        names as tags are only topics, and writing on a goal subscribes nobody to it."
)]
pub struct Cli {
    /// Path to the pile file to use
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable signing-key file. Reads and writes never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    /// Acting persona (relations label or 32-char hex id). When set,
    /// status and note events record who made them — the audit trail gains the
    /// actor, and `orient wait` watchers can absorb their own edits.
    #[arg(long, env = "PERSONA")]
    persona: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Add a new goal
    Add {
        #[arg(
            help = "Goal title. Use @path for file input or @- for stdin. An `@label` inside the title mentions that persona or group."
        )]
        title: String,
        #[arg(long, default_value = "todo")]
        status: String,
        /// Parent goal id (full 32-char hex id; use `compass resolve` to look up by prefix)
        #[arg(long)]
        parent: Option<String>,
        /// Tag (repeatable). A persona's own Relations label assigns the goal
        /// to that persona; any other tag, a group name included, is a topic.
        #[arg(long)]
        tag: Vec<String>,
        #[arg(
            long,
            help = "Initial note. Use @path for file input or @- for stdin. `@label` mentions inside it are recorded."
        )]
        note: Option<String>,
    },
    /// List goals in kanban columns (hides done by default)
    List {
        /// Show done goals too
        #[arg(long)]
        all: bool,
        /// Filter by tag (repeatable, shows goals matching any)
        #[arg(long)]
        tag: Vec<String>,
        #[arg(value_name = "STATUS")]
        status: Vec<String>,
    },
    /// Move a goal to a new status
    Move {
        /// Full 32-char hex id
        id: String,
        status: String,
    },
    /// Add a note to a goal
    Note {
        /// Full 32-char hex id
        id: String,
        #[arg(
            help = "Note text. Use @path for file input or @- for stdin. An `@label` inside the note mentions that persona or every member of that group."
        )]
        note: String,
        /// Short note tag (repeatable). A persona's own Relations label also
        /// directs the note to that persona; group names are topics (use an
        /// `@group` mention in the text to address a group).
        #[arg(long)]
        tag: Vec<String>,
        /// Opaque exact reference stored on the note (repeatable). Recognized
        /// inline `[text](faculty:hex)` links are stored automatically too.
        #[arg(long = "ref", value_name = "REFERENCE")]
        reference: Vec<String>,
        /// Existing note this note supersedes (repeatable). The edge is
        /// provenance only: Compass keeps and displays every note.
        #[arg(long, value_name = "NOTE_ID")]
        supersedes: Vec<String>,
    },
    /// Show a goal with history and notes
    Show {
        /// Full 32-char hex id
        id: String,
    },
    /// Mark a goal as more important than another
    Prioritize {
        /// The more important goal (full 32-char hex id)
        higher: String,
        /// The less important goal (full 32-char hex id)
        #[arg(long)]
        over: String,
    },
    /// Remove a priority relationship
    Deprioritize {
        /// The goal that was marked more important (full 32-char hex id)
        higher: String,
        /// The goal it was prioritized over (full 32-char hex id)
        #[arg(long)]
        over: String,
    },
    /// Resolve a hex prefix to a full 32-char goal id
    Resolve {
        /// Hex prefix to search for
        prefix: String,
    },
}

pub fn execute(cli: Cli, output: &mut Out<'_>) -> Result<()> {
    let Some(command) = cli.command else {
        return output.line(Cli::command().render_help().to_string());
    };
    let compass = Compass::new(cli.pile, cli.key);
    let persona = cli.persona.as_deref();
    match command {
        Command::Add {
            title,
            status,
            parent,
            tag,
            note,
        } => {
            let title = crate::text_arg(&title, "goal title")?;
            let note = note
                .as_deref()
                .map(|value| crate::text_arg(value, "goal note"))
                .transpose()?;
            let receipt = compass.add(
                &title,
                AddOptions {
                    status: &status,
                    parent: parent.as_deref(),
                    tags: &tag,
                    note: note.as_deref(),
                    persona,
                },
            )?;
            render::added(&receipt, output)
        }
        Command::List { status, tag, all } => output.text(compass.list(ListOptions {
            statuses: &status,
            tags: &tag,
            all,
        })?),
        Command::Move { id, status } => {
            render::moved(&compass.move_goal(&id, &status, persona)?, output)
        }
        Command::Note {
            id,
            note,
            tag,
            reference,
            supersedes,
        } => {
            let note = crate::text_arg(&note, "goal note")?;
            let receipt = compass.note(
                &id,
                &note,
                NoteOptions {
                    tags: &tag,
                    references: &reference,
                    supersedes: &supersedes,
                    persona,
                },
            )?;
            render::noted(&receipt, output)
        }
        Command::Show { id } => output.text(compass.show(&id)?),
        Command::Prioritize { higher, over } => {
            render::prioritized(&compass.prioritize(&higher, &over)?, output)
        }
        Command::Deprioritize { higher, over } => {
            render::prioritized(&compass.deprioritize(&higher, &over)?, output)
        }
        Command::Resolve { prefix } => output.line(format!("{:x}", compass.resolve(&prefix)?)),
    }
}

/// Preserve the tailored CLI grammar; delivery uses the shared sensory/stdout
/// emitter only after parsing and help handling have finished.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    crate::cli::with_output("compass", |output| execute(cli, output))
}
