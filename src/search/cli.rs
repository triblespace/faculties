use super::{Options, Search, DEFAULT_LIMIT};
use crate::out::Out;
use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version = crate::GIT_VERSION, name = "search",
    about = "Query Wiki, Memory and Files with explicit per-source lexical results")]
pub struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Search current Wiki text, selected Memory summaries and Files metadata.
    Query {
        query: String,
        /// Maximum readings returned from each source, after deduplication.
        #[arg(long, default_value_t = DEFAULT_LIMIT)]
        limit_per_source: usize,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    crate::cli::with_output("search", |out| execute(cli, out))
}

pub fn execute(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let Some(Command::Query {
        query,
        limit_per_source,
    }) = cli.command
    else {
        return out.line(Cli::command().render_long_help().to_string());
    };
    let options = Options {
        query: &query,
        limit_per_source,
    };
    options.validate()?;
    let search = Search::with_storage(cli.workspace.storage(cli.pile, cli.key)?);
    out.line(search.query(options)?.json()?)
}
