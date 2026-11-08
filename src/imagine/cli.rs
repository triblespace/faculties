use super::{operations::remember_range, Imagine, ModelSources, Options, Variant};
use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version = crate::GIT_VERSION, name = "imagine", about = "Generate an image with the local FLUX.2 pipeline")]
struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    /// Literal prompt, @file, @- stdin, or @@escaped leading at-sign.
    prompt: Option<String>,
    #[arg(long, conflicts_with = "dev")]
    klein: bool,
    #[arg(long)]
    dev: bool,
    #[arg(long)]
    steps: Option<usize>,
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long, default_value_t = 1024)]
    width: usize,
    #[arg(long, default_value_t = 1024)]
    height: usize,
    #[arg(long, default_value_t = 4.0)]
    guidance: f32,
    /// Save original PNG here; otherwise /tmp/imagine/<timestamp>_<seed>.png.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Explicit TAI timestamp or from..to Memory range; requires a configured pile.
    #[arg(long)]
    remember: Option<String>,
    #[arg(long, env = "PILE")]
    pile: Option<PathBuf>,
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
}

fn validate_workspace_use(cli: &Cli) -> Result<()> {
    anyhow::ensure!(
        cli.workspace.workspace.is_none() || cli.remember.is_some(),
        "workspace selection applies only to --remember; image generation and model assets are not workspace-scoped. Omit --workspace and unset FACULTIES_WORKSPACE when not remembering"
    );
    Ok(())
}

fn remembering(
    cli: &Cli,
) -> Result<Option<(crate::memory::Memory, (hifitime::Epoch, hifitime::Epoch))>> {
    validate_workspace_use(cli)?;
    cli.remember
        .as_deref()
        .map(|raw| {
            let pile = cli
                .pile
                .clone()
                .context("--remember requires --pile or PILE")?;
            Ok((
                crate::memory::Memory::with_storage(cli.workspace.storage(pile, cli.key.clone())?),
                remember_range(raw)?,
            ))
        })
        .transpose()
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let Some(prompt) = cli.prompt.as_deref() else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    validate_workspace_use(&cli)?;
    // A disabled generator must not acquire a file or wait for stdin first.
    anyhow::ensure!(
        cfg!(feature = "imagine"),
        "image generation requires a build with the `imagine` feature"
    );
    let options = Options {
        prompt: crate::text_arg(&prompt, "image prompt")?,
        variant: if cli.dev {
            Variant::Dev
        } else {
            Variant::Klein
        },
        steps: cli.steps,
        seed: cli.seed,
        width: cli.width,
        height: cli.height,
        guidance: cli.guidance,
    };
    options.validate()?;
    let remember = remembering(&cli)?;
    let generated = Imagine::new(ModelSources::from_environment()).generate(&options)?;
    let path = match cli.out {
        Some(path) => path,
        None => PathBuf::from(format!(
            "/tmp/imagine/{}_{}.png",
            crate::clock::tai_nanoseconds_now()?,
            cli.seed
        )),
    };
    generated.save(&path)?;
    eprintln!("imagine: {}", generated.diagnostic());
    crate::cli::with_output("imagine", |out| {
        // Plain CLI keeps its useful readable artifact path; a configured Drive
        // receives the actual image as well as the saved-file receipt.
        if std::env::var_os("DRIVE_ENDPOINT").is_some() {
            out.image(generated.png.clone(), "image/png")?;
        }
        out.line(path.display().to_string())
    })?;
    if let Some((memory, range)) = remember {
        let receipt = memory
            .image(generated.png.as_ref(), range)
            .with_context(|| {
                format!(
                    "remembering failed; generated PNG remains at {}",
                    path.display()
                )
            })?;
        crate::cli::with_output("imagine", |out| receipt.emit(out))?;
    }
    Ok(())
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    #[test]
    fn selected_generation_without_remember_is_not_a_workspace_sandbox() {
        let revision = format!("{:X}", triblespace::prelude::genid().id);
        let cli = Cli::try_parse_from(["imagine", "--workspace", &revision, "@-"]).unwrap();
        let error = validate_workspace_use(&cli).unwrap_err();
        assert!(error.to_string().contains("only to --remember"));
    }

    #[test]
    fn selected_remember_requires_existing_bootstrap_without_loading_a_model() {
        let revision = format!("{:X}", triblespace::prelude::genid().id);
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("absent.pile");
        let key = directory.path().join("absent.key");
        let cli = Cli::try_parse_from([
            "imagine",
            "--workspace",
            &revision,
            "--pile",
            pile.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "--remember",
            "2026-10-30T12:00:00 TAI",
            "a tree",
        ])
        .unwrap();
        assert!(remembering(&cli).is_err());
        assert!(!pile.exists());
        assert!(!key.exists());
    }
}
