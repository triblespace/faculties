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
}

#[derive(ValueEnum, Debug, Clone, Copy)]
#[value(rename_all = "kebab-case")]
enum UnsetField {
    ApiKey,
    ReasoningEffort,
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
        Command::Show => show_headspace(&runner, &cli.pile),
        Command::List => {
            let output = run_capture(&runner, &cli.pile, &["config", "profile", "list"])?;
            print!("{output}");
            Ok(())
        }
        Command::Use { profile } => {
            run_status(&runner, &cli.pile, &["config", "profile", "use", profile.as_str()])?;
            show_headspace(&runner, &cli.pile)
        }
        Command::Add(args) => {
            run_status(
                &runner,
                &cli.pile,
                &["config", "profile", "add", args.name.as_str()],
            )?;
            apply_add_overrides(&runner, &cli.pile, &args)?;
            show_headspace(&runner, &cli.pile)
        }
        Command::Set { field, value } => {
            let key = set_field_key(*field);
            run_status(
                &runner,
                &cli.pile,
                &["config", "set", key, value.as_str()],
            )?;
            show_headspace(&runner, &cli.pile)
        }
        Command::Unset { field } => {
            let key = unset_field_key(*field);
            run_status(&runner, &cli.pile, &["config", "unset", key])?;
            show_headspace(&runner, &cli.pile)
        }
    }
}

fn show_headspace(runner: &PlaygroundRunner, pile: &Path) -> Result<()> {
    let config = run_capture(runner, pile, &["config", "show"])?;
    let profiles = run_capture(runner, pile, &["config", "profile", "list"])?;

    println!("active:");
    let mut in_llm = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed == "[llm]" {
            in_llm = true;
            continue;
        }
        if in_llm && trimmed.starts_with('[') {
            break;
        }
        if !in_llm {
            continue;
        }
        if matches_key(trimmed, "profile_id")
            || matches_key(trimmed, "profile_name")
            || matches_key(trimmed, "model")
            || matches_key(trimmed, "base_url")
            || matches_key(trimmed, "reasoning_effort")
            || matches_key(trimmed, "stream")
            || matches_key(trimmed, "context_window_tokens")
            || matches_key(trimmed, "max_output_tokens")
            || matches_key(trimmed, "prompt_safety_margin_tokens")
            || matches_key(trimmed, "prompt_chars_per_token")
            || matches_key(trimmed, "compaction_profile_id")
        {
            println!("  {trimmed}");
        }
    }

    println!();
    println!("profiles:");
    for line in profiles.lines() {
        if line.trim().is_empty() {
            continue;
        }
        println!("  {line}");
    }
    Ok(())
}

fn matches_key(line: &str, key: &str) -> bool {
    let Some((lhs, _rhs)) = line.split_once('=') else {
        return false;
    };
    lhs.trim() == key
}

fn apply_add_overrides(runner: &PlaygroundRunner, pile: &Path, args: &AddArgs) -> Result<()> {
    if let Some(value) = args.model.as_deref() {
        run_status(runner, pile, &["config", "set", "llm-model", value])?;
    }
    if let Some(value) = args.base_url.as_deref() {
        run_status(runner, pile, &["config", "set", "llm-base-url", value])?;
    }
    if let Some(value) = args.api_key.as_deref() {
        run_status(runner, pile, &["config", "set", "llm-api-key", value])?;
    }
    if let Some(value) = args.reasoning_effort.as_deref() {
        run_status(runner, pile, &["config", "set", "llm-reasoning-effort", value])?;
    }
    if let Some(value) = args.stream {
        run_status(
            runner,
            pile,
            &["config", "set", "llm-stream", if value { "true" } else { "false" }],
        )?;
    }
    if let Some(value) = args.context_window_tokens {
        run_status(
            runner,
            pile,
            &[
                "config",
                "set",
                "llm-context-window-tokens",
                value.to_string().as_str(),
            ],
        )?;
    }
    if let Some(value) = args.max_output_tokens {
        run_status(
            runner,
            pile,
            &[
                "config",
                "set",
                "llm-max-output-tokens",
                value.to_string().as_str(),
            ],
        )?;
    }
    if let Some(value) = args.prompt_safety_margin_tokens {
        run_status(
            runner,
            pile,
            &[
                "config",
                "set",
                "llm-prompt-safety-margin-tokens",
                value.to_string().as_str(),
            ],
        )?;
    }
    if let Some(value) = args.prompt_chars_per_token {
        run_status(
            runner,
            pile,
            &[
                "config",
                "set",
                "llm-prompt-chars-per-token",
                value.to_string().as_str(),
            ],
        )?;
    }
    Ok(())
}

fn set_field_key(field: SetField) -> &'static str {
    match field {
        SetField::Model => "llm-model",
        SetField::BaseUrl => "llm-base-url",
        SetField::ApiKey => "llm-api-key",
        SetField::ReasoningEffort => "llm-reasoning-effort",
        SetField::Stream => "llm-stream",
        SetField::ContextWindowTokens => "llm-context-window-tokens",
        SetField::MaxOutputTokens => "llm-max-output-tokens",
        SetField::PromptSafetyMarginTokens => "llm-prompt-safety-margin-tokens",
        SetField::PromptCharsPerToken => "llm-prompt-chars-per-token",
    }
}

fn unset_field_key(field: UnsetField) -> &'static str {
    match field {
        UnsetField::ApiKey => "llm-api-key",
        UnsetField::ReasoningEffort => "llm-reasoning-effort",
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
