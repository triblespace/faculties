#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4"
//! rand_core = "0.6.4"
//! triblespace = "0.12.0"
//! ```

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{CommandFactory, Parser};


#[path = "archive_common.rs"]
mod common;

#[derive(Parser)]
#[command(name = "archive-import-codex", about = "Import Codex exports into TribleSpace")]
struct Cli {
    /// Path to the pile file to write into.
    #[arg(long, global = true)]
    pile: Option<PathBuf>,
    /// Branch name to write into (created if missing).
    #[arg(long, default_value = "archive", global = true)]
    branch: String,
    /// Import path shortcut.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let pile_path = cli.pile.clone().unwrap_or_else(common::default_pile_path);
    if let Err(err) = common::emit_schema_to_atlas(&pile_path) {
        eprintln!("atlas emit: {err}");
    }
    if cli.path.is_none() {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    }

    bail!("Codex importer not implemented yet.")
}
