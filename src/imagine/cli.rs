use super::auth::{Credential, Provider, Selector};
use super::openai::{self, Background, Moderation, OutputFormat, Quality, Size};
use super::{
    operations::remember_range, Flux, FluxOptions, Imagine, ModelSources, Request, Variant,
};
use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use triblespace::prelude::*;

#[derive(Parser, Debug)]
#[command(
    version = crate::GIT_VERSION,
    name = "imagine",
    about = "Generate an image (OpenAI's Image API, or the local FLUX.2 pipeline) and store it in Files with its provenance",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    #[command(subcommand)]
    command: Option<Command>,
    /// Literal prompt, @file, @- stdin, or @@escaped leading at-sign.
    prompt: Option<String>,
    /// Which backend generates. openai needs a bound credential (`imagine auth set openai`).
    #[arg(long, value_enum, default_value_t = ProviderArg::Openai)]
    provider: ProviderArg,
    /// OpenAI model.
    #[arg(long, help_heading = "OpenAI")]
    model: Option<String>,
    /// OpenAI size: auto or WIDTHxHEIGHT (multiples of 16, edge <= 3840, aspect 1:3..3:1).
    #[arg(long, help_heading = "OpenAI")]
    size: Option<Size>,
    /// OpenAI quality: low, medium, high, xhigh, max or auto.
    #[arg(long, help_heading = "OpenAI")]
    quality: Option<Quality>,
    /// OpenAI output format: png, jpeg or webp.
    #[arg(long, help_heading = "OpenAI")]
    output_format: Option<OutputFormat>,
    /// OpenAI output compression 0-100 (jpeg and webp).
    #[arg(long, help_heading = "OpenAI")]
    output_compression: Option<u8>,
    /// OpenAI background: transparent (png/webp), opaque or auto.
    #[arg(long, help_heading = "OpenAI")]
    background: Option<Background>,
    /// OpenAI moderation: auto or low.
    #[arg(long, help_heading = "OpenAI")]
    moderation: Option<Moderation>,
    #[arg(long, conflicts_with = "dev", help_heading = "FLUX")]
    klein: bool,
    #[arg(long, help_heading = "FLUX")]
    dev: bool,
    #[arg(long, help_heading = "FLUX")]
    steps: Option<usize>,
    #[arg(long, help_heading = "FLUX")]
    seed: Option<u64>,
    #[arg(long, help_heading = "FLUX")]
    width: Option<usize>,
    #[arg(long, help_heading = "FLUX")]
    height: Option<usize>,
    #[arg(long, help_heading = "FLUX")]
    guidance: Option<f32>,
    /// Also save the original bytes here. FLUX without a pile saves to
    /// /tmp/imagine/<timestamp>_<seed>.png when this is absent.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Explicit TAI timestamp or from..to Memory range; requires a configured pile.
    #[arg(long)]
    remember: Option<String>,
    #[arg(long, env = "PILE", global = true)]
    pile: Option<PathBuf>,
    #[arg(long, env = "TRIBLESPACE_KEY", global = true)]
    key: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ProviderArg {
    Openai,
    Flux,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Provider credentials: exact Secrets selectors, never names.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}

#[derive(Subcommand, Debug)]
enum AuthCommand {
    /// Show each provider's bound credential (metadata only; nothing is opened).
    Status,
    /// Bind an exact raw Secrets version (--secret), or one field of an exact
    /// structured item version (--secret ITEM --body BLAKE3 --field ID, as
    /// `secrets item-open` and `secrets item-inspect` show them). The
    /// credential is opened once to prove it works; nothing else is sent.
    Set {
        /// Provider: openai.
        provider: Provider,
        #[arg(long, value_parser = parse_id)]
        secret: Id,
        #[arg(long, value_parser = parse_handle, requires = "field")]
        body: Option<[u8; 32]>,
        #[arg(long, value_parser = parse_id, requires = "body")]
        field: Option<Id>,
    },
}

fn parse_id(raw: &str) -> std::result::Result<Id, String> {
    Id::from_hex(raw.trim()).ok_or_else(|| "expected an exact 32-hex id".to_owned())
}

fn parse_handle(raw: &str) -> std::result::Result<[u8; 32], String> {
    let raw = raw.trim();
    hex::decode(raw.strip_prefix("blake3:").unwrap_or(raw))
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "expected a 64-hex blake3 handle".to_owned())
}

/// The storage generation and remembering write to, if a pile is configured.
fn storage_for(cli: &Cli) -> Result<Option<crate::storage::Storage>> {
    let Some(pile) = cli.pile.clone() else {
        anyhow::ensure!(
            cli.workspace.workspace.is_none(),
            "workspace selection needs --pile or PILE"
        );
        anyhow::ensure!(cli.remember.is_none(), "--remember requires --pile or PILE");
        return Ok(None);
    };
    cli.workspace.storage(pile, cli.key.clone()).map(Some)
}

/// The typed request, refusing the other provider's options. The prompt is
/// read (possibly from stdin) only after the provider is known to be usable.
fn request(cli: &Cli, prompt: &str) -> Result<Request> {
    match cli.provider {
        ProviderArg::Openai => {
            let flux = [
                ("--klein", cli.klein),
                ("--dev", cli.dev),
                ("--steps", cli.steps.is_some()),
                ("--seed", cli.seed.is_some()),
                ("--width", cli.width.is_some()),
                ("--height", cli.height.is_some()),
                ("--guidance", cli.guidance.is_some()),
            ];
            if let Some((flag, _)) = flux.iter().find(|(_, given)| *given) {
                bail!("{flag} is a FLUX option; pass --provider flux, or use --size/--quality for openai");
            }
            let mut options = openai::Options::new(crate::text_arg(prompt, "image prompt")?);
            if let Some(model) = &cli.model {
                options.model = model.clone();
            }
            options.size = cli.size.unwrap_or_default();
            options.quality = cli.quality.unwrap_or_default();
            options.output_format = cli.output_format.unwrap_or_default();
            options.output_compression = cli.output_compression;
            options.background = cli.background.unwrap_or_default();
            options.moderation = cli.moderation.unwrap_or_default();
            Ok(Request::OpenAi(options))
        }
        ProviderArg::Flux => {
            let foreign = [
                ("--model", cli.model.is_some()),
                ("--size", cli.size.is_some()),
                ("--quality", cli.quality.is_some()),
                ("--output-format", cli.output_format.is_some()),
                ("--output-compression", cli.output_compression.is_some()),
                ("--background", cli.background.is_some()),
                ("--moderation", cli.moderation.is_some()),
            ];
            if let Some((flag, _)) = foreign.iter().find(|(_, given)| *given) {
                bail!("{flag} is an OpenAI option; it does not apply to --provider flux");
            }
            // A disabled generator must not acquire a file or wait for stdin first.
            anyhow::ensure!(
                cfg!(feature = "imagine"),
                "FLUX generation requires a build with the `imagine` feature"
            );
            Ok(Request::Flux(FluxOptions {
                prompt: crate::text_arg(prompt, "image prompt")?,
                variant: if cli.dev {
                    Variant::Dev
                } else {
                    Variant::Klein
                },
                steps: cli.steps,
                seed: cli.seed.unwrap_or(0),
                width: cli.width.unwrap_or(1024),
                height: cli.height.unwrap_or(1024),
                guidance: cli.guidance.unwrap_or(4.0),
            }))
        }
    }
}

fn save(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("write image to {}", path.display()))
}

fn run_auth(cli: &Cli, command: &AuthCommand) -> Result<()> {
    let storage = storage_for(cli)?.context("imagine auth needs --pile or PILE")?;
    let imagine = Imagine::with_storage(storage);
    crate::cli::with_output("imagine", |out| match command {
        AuthCommand::Status => {
            for provider in Provider::ALL {
                let status = imagine.auth_status(*provider)?;
                let line = match status.selected(*provider) {
                    Ok(credential) => credential.describe(),
                    Err(_) if status.heads.is_empty() => "unbound".to_owned(),
                    Err(_) => format!(
                        "unsettled: {} revisions naming {}",
                        status.heads.len(),
                        status
                            .credentials
                            .iter()
                            .map(Credential::describe)
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                };
                out.line(format!("{}: {line}", provider.label()))?;
            }
            Ok(())
        }
        AuthCommand::Set {
            provider,
            secret,
            body,
            field,
        } => {
            let selector = match (body, field) {
                (Some(body), Some(field)) => Selector::ItemField {
                    secret: *secret,
                    body: Inline::new(*body),
                    field: *field,
                },
                _ => Selector::Version(*secret),
            };
            let (revision, credential) = imagine.auth_set(*provider, selector)?;
            out.line(format!(
                "{}: {} (revision {revision:x})",
                provider.label(),
                credential.describe()
            ))
        }
    })
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Auth { command }) = &cli.command {
        return run_auth(&cli, command);
    }
    let Some(prompt) = cli.prompt.as_deref() else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    let range = cli.remember.as_deref().map(remember_range).transpose()?;
    let storage = storage_for(&cli)?;
    let request = request(&cli, prompt)?;
    request.validate()?;
    let Some(storage) = storage else {
        // Nowhere to store: only FLUX can run, and it saves a readable file.
        let Request::Flux(options) = &request else {
            bail!("openai generation stores its result in Files and reads its credential from Secrets; it needs --pile or PILE");
        };
        let generated = Flux::new(ModelSources::from_environment()).generate(options)?;
        let path = match &cli.out {
            Some(path) => path.clone(),
            None => PathBuf::from(format!(
                "/tmp/imagine/{}_{}.png",
                crate::clock::tai_nanoseconds_now()?,
                options.seed
            )),
        };
        generated.save(&path)?;
        eprintln!("imagine: {}", generated.diagnostic());
        return crate::cli::with_output("imagine", |out| {
            if std::env::var_os("DRIVE_ENDPOINT").is_some() {
                out.image(generated.png.clone(), "image/png")?;
            }
            out.line(path.display().to_string())
        });
    };
    let receipt = Imagine::with_storage(storage.clone()).generate(&request)?;
    let generated = &receipt.generated;
    if let Some(path) = &cli.out {
        save(path, generated.bytes()).with_context(|| {
            format!(
                "the image is stored as files:{:x}, but saving a copy failed",
                receipt.file
            )
        })?;
    }
    eprintln!("imagine: {}", generated.diagnostic());
    crate::cli::with_output("imagine", |out| {
        if std::env::var_os("DRIVE_ENDPOINT").is_some() {
            out.image(generated.bytes().clone(), generated.media_type())?;
        }
        out.line(format!(
            "files:{:x} ({}, {} bytes; generation {:x})",
            receipt.file,
            generated.media_type(),
            generated.bytes().len(),
            receipt.generation
        ))?;
        if let Some(path) = &cli.out {
            out.line(path.display().to_string())?;
        }
        Ok(())
    })?;
    if let Some(range) = range {
        let receipt_memory = crate::memory::Memory::with_storage(storage)
            .image(generated.bytes().as_ref(), range)
            .with_context(|| {
                format!(
                    "remembering failed; the image remains stored as files:{:x}",
                    receipt.file
                )
            })?;
        crate::cli::with_output("imagine", |out| receipt_memory.emit(out))?;
    }
    Ok(())
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    #[test]
    fn selected_workspace_without_a_pile_is_refused() {
        let revision = format!("{:X}", triblespace::prelude::genid().id);
        let cli = Cli::try_parse_from(["imagine", "--workspace", &revision, "@-"]).unwrap();
        let error = storage_for(&cli).unwrap_err();
        assert!(error.to_string().contains("needs --pile"), "{error}");
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
        assert!(storage_for(&cli).is_err());
        assert!(!pile.exists());
        assert!(!key.exists());
    }

    #[test]
    fn options_of_the_other_provider_are_refused_before_the_prompt_is_read() {
        let cli = Cli::try_parse_from(["imagine", "--steps", "4", "a tree"]).unwrap();
        assert!(request(&cli, "a tree")
            .unwrap_err()
            .to_string()
            .contains("FLUX option"));
        let cli = Cli::try_parse_from([
            "imagine",
            "--provider",
            "flux",
            "--quality",
            "high",
            "a tree",
        ])
        .unwrap();
        assert!(request(&cli, "a tree")
            .unwrap_err()
            .to_string()
            .contains("OpenAI option"));
    }

    #[test]
    fn openai_flags_parse_into_typed_options() {
        let cli = Cli::try_parse_from([
            "imagine",
            "--size",
            "1536x1024",
            "--quality",
            "xhigh",
            "--output-format",
            "webp",
            "--output-compression",
            "70",
            "--background",
            "transparent",
            "--moderation",
            "low",
            "--model",
            "gpt-image-2.5-flare",
            "a fox",
        ])
        .unwrap();
        let Request::OpenAi(options) = request(&cli, "a fox").unwrap() else {
            panic!("openai is the default provider");
        };
        assert_eq!(
            options.size,
            Size::Exact {
                width: 1536,
                height: 1024
            }
        );
        assert_eq!(options.quality, Quality::Xhigh);
        assert_eq!(options.output_format, OutputFormat::Webp);
        assert_eq!(options.output_compression, Some(70));
        assert_eq!(options.background, Background::Transparent);
        assert_eq!(options.moderation, Moderation::Low);
        assert_eq!(options.model, "gpt-image-2.5-flare");
        options.validate().unwrap();
    }

    #[test]
    fn auth_set_parses_the_item_field_selector() {
        let cli = Cli::try_parse_from([
            "imagine",
            "auth",
            "set",
            "openai",
            "--secret",
            "19a2cadf03f6308b800003b96a928ef9",
            "--body",
            "blake3:f62405b32c4863bd85bbdcc32f707c8f64184d2d26e21badb2abff4e74a8b401",
            "--field",
            "eaf56d1f889a9b4707fc22c302642695",
        ])
        .unwrap();
        let Some(Command::Auth {
            command: AuthCommand::Set { body, field, .. },
        }) = cli.command
        else {
            panic!("auth set parses");
        };
        assert!(body.is_some() && field.is_some());
        assert!(
            Cli::try_parse_from([
                "imagine",
                "auth",
                "set",
                "openai",
                "--secret",
                "19a2cadf03f6308b800003b96a928ef9",
                "--field",
                "eaf56d1f889a9b4707fc22c302642695",
            ])
            .is_err(),
            "a field needs its body"
        );
    }
}
