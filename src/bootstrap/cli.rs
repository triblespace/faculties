use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "bootstrap",
    version = crate::GIT_VERSION,
    about = "Import the portable onboarding seed under this pile's own signer"
)]
struct Cli {
    /// Destination pile. It must already exist and have a durable signing key.
    #[arg(long, env = "PILE")]
    pile: PathBuf,

    /// Explicit durable signing-key path instead of TRIBLESPACE_KEY or self.key.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,

    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Import 21 Wiki fragments and seven Compass goals idempotently.
    Import,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let Some(Command::Import) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    crate::cli::with_output("bootstrap", |out| {
        let storage = cli.workspace.storage(cli.pile, cli.key)?;
        let report = super::import_with_storage(&storage)?;
        super::render_import(&report, out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_is_global_and_help_requires_no_existing_pile() {
        let revision = triblespace::prelude::genid().id;
        let id = format!("{revision:X}");
        for args in [
            vec![
                "bootstrap",
                "--pile",
                "/absent.pile",
                "--workspace",
                &id,
                "import",
            ],
            vec![
                "bootstrap",
                "--pile",
                "/absent.pile",
                "import",
                "--workspace",
                &id,
            ],
        ] {
            assert_eq!(
                Cli::try_parse_from(args).unwrap().workspace.workspace,
                Some(revision)
            );
        }
        let help = Cli::try_parse_from(["bootstrap", "--workspace", &id, "--help"])
            .err()
            .unwrap();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(Cli::try_parse_from([
            "bootstrap",
            "--pile",
            "/absent.pile",
            "--workspace",
            "label",
            "import",
        ])
        .is_err());
    }
}
