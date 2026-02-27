#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ```

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "headspace",
    bin_name = "headspace",
    about = "Manage active LLM headspace (profile/model/reasoning)."
)]
struct Cli {
    /// Path to the pile file to use
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Optional explicit playground binary path
    #[arg(long, global = true)]
    playground_bin: Option<PathBuf>,
    /// Optional explicit playground Cargo.toml path
    #[arg(long, global = true)]
    manifest_path: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Show active headspace settings and available profiles
    Show,
    /// List available profiles
    List,
    /// Switch active profile by id or name
    Use {
        #[arg(value_name = "PROFILE")]
        profile: String,
    },
    /// Add a new profile and make it active
    Add(AddArgs),
    /// Set one field on the active profile
    Set {
        #[arg(value_enum, value_name = "FIELD")]
        field: SetField,
        #[arg(value_name = "VALUE")]
        value: String,
    },
    /// Clear one optional field on the active profile
    Unset {
        #[arg(value_enum, value_name = "FIELD")]
        field: UnsetField,
    },
    /// Manage memory lenses used by headspace/memory compaction
    Lens {
        #[command(subcommand)]
        command: LensCommand,
    },
}

#[derive(Args, Debug, Clone)]
struct AddArgs {
    #[arg(value_name = "NAME")]
    name: String,
    #[arg(long)]
    model: Option<String>,
    #[arg(long = "base-url")]
    base_url: Option<String>,
    #[arg(long = "api-key")]
    api_key: Option<String>,
    #[arg(long = "reasoning-effort")]
    reasoning_effort: Option<String>,
    #[arg(long)]
    stream: Option<bool>,
    #[arg(long = "context-window-tokens")]
    context_window_tokens: Option<u64>,
    #[arg(long = "max-output-tokens")]
    max_output_tokens: Option<u64>,
    #[arg(long = "prompt-safety-margin-tokens")]
    prompt_safety_margin_tokens: Option<u64>,
    #[arg(long = "prompt-chars-per-token")]
    prompt_chars_per_token: Option<u64>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
#[value(rename_all = "kebab-case")]
enum SetField {
    Model,
    BaseUrl,
    ApiKey,
    ReasoningEffort,
    Stream,
    ContextWindowTokens,
    MaxOutputTokens,
    PromptSafetyMarginTokens,
    PromptCharsPerToken,
    CompactionProfileId,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
#[value(rename_all = "kebab-case")]
enum UnsetField {
    ApiKey,
    ReasoningEffort,
    CompactionProfileId,
}

#[derive(Subcommand, Debug, Clone)]
enum LensCommand {
    /// List configured memory lenses
    List,
    /// Add a memory lens
    Add(LensAddArgs),
    /// Set one field on a memory lens
    Set(LensSetArgs),
    /// Reset one field (or all fields) to defaults for a memory lens
    Reset(LensResetArgs),
    /// Remove a memory lens
    Remove {
        #[arg(value_name = "NAME")]
        name: String,
    },
}

#[derive(Args, Debug, Clone)]
struct LensAddArgs {
    #[arg(value_name = "NAME")]
    name: String,
    #[arg(long, value_name = "ID")]
    id: Option<String>,
    #[arg(long, value_name = "PROMPT")]
    prompt: Option<String>,
    #[arg(long = "compaction-prompt", value_name = "PROMPT")]
    compaction_prompt: Option<String>,
    #[arg(long = "max-output-tokens", value_name = "TOKENS")]
    max_output_tokens: Option<u64>,
}

#[derive(Args, Debug, Clone)]
struct LensSetArgs {
    #[arg(value_name = "NAME")]
    name: String,
    #[arg(value_enum, value_name = "FIELD")]
    field: LensField,
    #[arg(value_name = "VALUE")]
    value: String,
}

#[derive(Args, Debug, Clone)]
struct LensResetArgs {
    #[arg(value_name = "NAME")]
    name: String,
    #[arg(value_enum, value_name = "FIELD")]
    field: Option<LensField>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
#[value(rename_all = "kebab-case")]
enum LensField {
    Id,
    Prompt,
    CompactionPrompt,
    MaxOutputTokens,
}

enum PlaygroundRunner {
    Binary(PathBuf),
    CargoManifest(PathBuf),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command.as_ref() else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };

    let runner = resolve_runner(&cli)?;
    match command {
        Command::Show => {
            let output = run_capture(&runner, &cli.pile, &["headspace", "show"])?;
            print!("{output}");
            Ok(())
        }
        Command::List => {
            let output = run_capture(&runner, &cli.pile, &["headspace", "list"])?;
            print!("{output}");
            Ok(())
        }
        Command::Use { profile } => {
            run_status(&runner, &cli.pile, &["headspace", "use", profile.as_str()])
        }
        Command::Add(args) => {
            run_status(
                &runner,
                &cli.pile,
                &["headspace", "add", args.name.as_str()],
            )?;
            apply_add_overrides(&runner, &cli.pile, &args)?;
            Ok(())
        }
        Command::Set { field, value } => {
            let key = set_field_key(*field);
            run_status(
                &runner,
                &cli.pile,
                &["headspace", "set", key, value.as_str()],
            )?;
            Ok(())
        }
        Command::Unset { field } => {
            let key = unset_field_key(*field);
            run_status(&runner, &cli.pile, &["headspace", "unset", key])
        }
        Command::Lens { command } => {
            let mut args = vec!["headspace".to_string(), "lens".to_string()];
            extend_lens_args(&mut args, command.clone());
            run_status_vec(&runner, &cli.pile, &args)
        }
    }
}

fn apply_add_overrides(runner: &PlaygroundRunner, pile: &Path, args: &AddArgs) -> Result<()> {
    if let Some(value) = args.model.as_deref() {
        run_status(runner, pile, &["headspace", "set", "model", value])?;
    }
    if let Some(value) = args.base_url.as_deref() {
        run_status(runner, pile, &["headspace", "set", "base-url", value])?;
    }
    if let Some(value) = args.api_key.as_deref() {
        run_status(runner, pile, &["headspace", "set", "api-key", value])?;
    }
    if let Some(value) = args.reasoning_effort.as_deref() {
        run_status(runner, pile, &["headspace", "set", "reasoning-effort", value])?;
    }
    if let Some(value) = args.stream {
        run_status(
            runner,
            pile,
            &["headspace", "set", "stream", if value { "true" } else { "false" }],
        )?;
    }
    if let Some(value) = args.context_window_tokens {
        run_status(
            runner,
            pile,
            &["headspace", "set", "context-window-tokens", value.to_string().as_str()],
        )?;
    }
    if let Some(value) = args.max_output_tokens {
        run_status(
            runner,
            pile,
            &["headspace", "set", "max-output-tokens", value.to_string().as_str()],
        )?;
    }
    if let Some(value) = args.prompt_safety_margin_tokens {
        run_status(
            runner,
            pile,
            &[
                "headspace",
                "set",
                "prompt-safety-margin-tokens",
                value.to_string().as_str(),
            ],
        )?;
    }
    if let Some(value) = args.prompt_chars_per_token {
        run_status(
            runner,
            pile,
            &["headspace", "set", "prompt-chars-per-token", value.to_string().as_str()],
        )?;
    }
    Ok(())
}

fn set_field_key(field: SetField) -> &'static str {
    match field {
        SetField::Model => "model",
        SetField::BaseUrl => "base-url",
        SetField::ApiKey => "api-key",
        SetField::ReasoningEffort => "reasoning-effort",
        SetField::Stream => "stream",
        SetField::ContextWindowTokens => "context-window-tokens",
        SetField::MaxOutputTokens => "max-output-tokens",
        SetField::PromptSafetyMarginTokens => "prompt-safety-margin-tokens",
        SetField::PromptCharsPerToken => "prompt-chars-per-token",
        SetField::CompactionProfileId => "compaction-profile-id",
    }
}

fn unset_field_key(field: UnsetField) -> &'static str {
    match field {
        UnsetField::ApiKey => "api-key",
        UnsetField::ReasoningEffort => "reasoning-effort",
        UnsetField::CompactionProfileId => "compaction-profile-id",
    }
}

fn lens_field_key(field: LensField) -> &'static str {
    match field {
        LensField::Id => "id",
        LensField::Prompt => "prompt",
        LensField::CompactionPrompt => "compaction-prompt",
        LensField::MaxOutputTokens => "max-output-tokens",
    }
}

fn extend_lens_args(args: &mut Vec<String>, command: LensCommand) {
    match command {
        LensCommand::List => {
            args.push("list".to_string());
        }
        LensCommand::Add(add) => {
            args.push("add".to_string());
            args.push(add.name);
            if let Some(id) = add.id {
                args.push("--id".to_string());
                args.push(id);
            }
            if let Some(prompt) = add.prompt {
                args.push("--prompt".to_string());
                args.push(prompt);
            }
            if let Some(prompt) = add.compaction_prompt {
                args.push("--compaction-prompt".to_string());
                args.push(prompt);
            }
            if let Some(tokens) = add.max_output_tokens {
                args.push("--max-output-tokens".to_string());
                args.push(tokens.to_string());
            }
        }
        LensCommand::Set(set) => {
            args.push("set".to_string());
            args.push(set.name);
            args.push(lens_field_key(set.field).to_string());
            args.push(set.value);
        }
        LensCommand::Reset(reset) => {
            args.push("reset".to_string());
            args.push(reset.name);
            if let Some(field) = reset.field {
                args.push(lens_field_key(field).to_string());
            }
        }
        LensCommand::Remove { name } => {
            args.push("remove".to_string());
            args.push(name);
        }
    }
}

fn resolve_runner(cli: &Cli) -> Result<PlaygroundRunner> {
    if let Some(path) = cli.playground_bin.as_ref() {
        return Ok(PlaygroundRunner::Binary(path.clone()));
    }
    if let Ok(path) = std::env::var("PLAYGROUND_BIN") {
        return Ok(PlaygroundRunner::Binary(PathBuf::from(path)));
    }
    for candidate in [
        PathBuf::from("/opt/playground/target/debug/playground"),
        PathBuf::from("playground/target/debug/playground"),
        PathBuf::from("target/debug/playground"),
    ] {
        if candidate.exists() {
            return Ok(PlaygroundRunner::Binary(candidate));
        }
    }

    if let Some(path) = cli.manifest_path.as_ref() {
        return Ok(PlaygroundRunner::CargoManifest(path.clone()));
    }
    if let Ok(path) = std::env::var("PLAYGROUND_MANIFEST_PATH") {
        return Ok(PlaygroundRunner::CargoManifest(PathBuf::from(path)));
    }
    for candidate in [
        PathBuf::from("/opt/playground/Cargo.toml"),
        PathBuf::from("playground/Cargo.toml"),
        PathBuf::from("Cargo.toml"),
    ] {
        if candidate.exists() && candidate.file_name().is_some_and(|name| name == "Cargo.toml") {
            return Ok(PlaygroundRunner::CargoManifest(candidate));
        }
    }

    bail!(
        "unable to locate playground runner; pass --playground-bin <path> or --manifest-path <path>"
    );
}

fn run_capture(runner: &PlaygroundRunner, pile: &Path, args: &[&str]) -> Result<String> {
    let output = build_command(runner, pile, args)
        .output()
        .with_context(|| format!("run `{}`", render_invocation(runner, pile, args)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(anyhow!(
            "command failed ({}):\n{}\n{}",
            output.status,
            stdout,
            stderr
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_status(runner: &PlaygroundRunner, pile: &Path, args: &[&str]) -> Result<()> {
    let output = build_command(runner, pile, args)
        .output()
        .with_context(|| format!("run `{}`", render_invocation(runner, pile, args)))?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.is_empty() {
            print!("{stdout}");
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            eprint!("{stderr}");
        }
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(anyhow!(
        "command failed ({}):\n{}\n{}",
        output.status,
        stdout,
        stderr
    ))
}

fn run_status_vec(runner: &PlaygroundRunner, pile: &Path, args: &[String]) -> Result<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_status(runner, pile, refs.as_slice())
}

fn build_command(runner: &PlaygroundRunner, pile: &Path, args: &[&str]) -> ProcessCommand {
    match runner {
        PlaygroundRunner::Binary(path) => {
            let mut cmd = ProcessCommand::new(path);
            cmd.arg("--pile").arg(pile);
            cmd.args(args);
            cmd
        }
        PlaygroundRunner::CargoManifest(manifest) => {
            let mut cmd = ProcessCommand::new("cargo");
            cmd.arg("run")
                .arg("--quiet")
                .arg("--manifest-path")
                .arg(manifest)
                .arg("--")
                .arg("--pile")
                .arg(pile);
            cmd.args(args);
            cmd
        }
    }
}

fn render_invocation(runner: &PlaygroundRunner, pile: &Path, args: &[&str]) -> String {
    match runner {
        PlaygroundRunner::Binary(path) => format!(
            "{} --pile {} {}",
            path.display(),
            pile.display(),
            args.join(" ")
        ),
        PlaygroundRunner::CargoManifest(manifest) => format!(
            "cargo run --quiet --manifest-path {} -- --pile {} {}",
            manifest.display(),
            pile.display(),
            args.join(" ")
        ),
    }
}
