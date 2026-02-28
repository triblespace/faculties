#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4.2.3"
//! rand_core = "0.6.4"
//! triblespace = "0.16.0"
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use triblespace::core::metadata;
use triblespace::core::repo::PushResult;
use triblespace::core::repo::branch as branch_meta;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::macros::{attributes, find, id_hex, pattern};
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::valueschemas::{Blake3, GenId, Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

const DEFAULT_MODEL: &str = "gpt-oss:120b";
const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";
const DEFAULT_STREAM: bool = false;
const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 32 * 1024;
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 1024;
const DEFAULT_PROMPT_SAFETY_MARGIN_TOKENS: u64 = 512;
const DEFAULT_PROMPT_CHARS_PER_TOKEN: u64 = 4;
const DEFAULT_MEMORY_LENS_FACTUAL_PROMPT: &str = include_str!("../prompts/memory_lens_factual.md");
const DEFAULT_MEMORY_LENS_TECHNICAL_PROMPT: &str =
    include_str!("../prompts/memory_lens_technical.md");
const DEFAULT_MEMORY_LENS_EMOTIONAL_PROMPT: &str =
    include_str!("../prompts/memory_lens_emotional.md");
const DEFAULT_MEMORY_LENS_FACTUAL_COMPACTION_PROMPT: &str =
    include_str!("../prompts/memory_lens_factual_compaction.md");
const DEFAULT_MEMORY_LENS_TECHNICAL_COMPACTION_PROMPT: &str =
    include_str!("../prompts/memory_lens_technical_compaction.md");
const DEFAULT_MEMORY_LENS_EMOTIONAL_COMPACTION_PROMPT: &str =
    include_str!("../prompts/memory_lens_emotional_compaction.md");
const DEFAULT_MEMORY_LENS_FACTUAL_MAX_OUTPUT_TOKENS: u64 = 192;
const DEFAULT_MEMORY_LENS_TECHNICAL_MAX_OUTPUT_TOKENS: u64 = 224;
const DEFAULT_MEMORY_LENS_EMOTIONAL_MAX_OUTPUT_TOKENS: u64 = 96;
const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../prompts/system_prompt.md");

const DEFAULT_BRANCH: &str = "cognition";
const DEFAULT_EXEC_BRANCH: &str = "cognition";
const DEFAULT_COMPASS_BRANCH: &str = "compass";
const DEFAULT_LOCAL_MESSAGES_BRANCH: &str = "local-messages";
const DEFAULT_RELATIONS_BRANCH: &str = "relations";
const DEFAULT_TEAMS_BRANCH: &str = "teams";
const DEFAULT_WORKSPACE_BRANCH: &str = "workspace";
const DEFAULT_ARCHIVE_BRANCH: &str = "archive";
const DEFAULT_WEB_BRANCH: &str = "web";
const DEFAULT_MEDIA_BRANCH: &str = "media";
const DEFAULT_AUTHOR: &str = "agent";
const DEFAULT_AUTHOR_ROLE: &str = "user";
const DEFAULT_POLL_MS: u64 = 1;
const CONFIG_BRANCH: &str = "config";
const CONFIG_BRANCH_ID: Id = id_hex!("4790808CF044F979FC7C2E47FCCB4A64");
const KIND_CONFIG_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");
const KIND_LLM_PROFILE_ID: Id = id_hex!("B08E356C4B08F44AB7EC177D47129447");
const KIND_MEMORY_LENS_ID: Id = id_hex!("D982F64C48F263A312D6E342D09554B0");
const MEMORY_LENS_ID_FACTUAL: Id = id_hex!("E39414C1875CB127BC7E2F4C42CB3C17");
const MEMORY_LENS_ID_TECHNICAL: Id = id_hex!("6D6BC80F284B56CAA2AFDE8C841EE894");
const MEMORY_LENS_ID_EMOTIONAL: Id = id_hex!("1B7C34E5C9718DE01020CA3C0EF50387");

mod playground_config {
    use super::*;
    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: NsTAIInterval;
        "950B556A74F71AC7CB008AB23FBB6544" as system_prompt: Handle<Blake3, LongString>;
        "35E36AE7B60AD946661BD63B3CD64672" as branch: Handle<Blake3, LongString>;
        "4E2F9CA7A8456DED8C43A3BE741ADA58" as branch_id: GenId;
        "EDEFFF6AFF6318E44CCF6A602B012604" as compass_branch_id: GenId;
        "C188E12ABBDD83D283A23DBAD4B784AF" as exec_branch_id: GenId;
        "2ED6FF7EAB93CB5608555AE4B9664CF8" as local_messages_branch_id: GenId;
        "D35F4F02E29825FBC790E324EFCD1B34" as relations_branch_id: GenId;
        "22A0E76B8044311563369298306906E3" as teams_branch_id: GenId;
        "20D37D92C2AEF5C98899C4C35AA1E35E" as workspace_branch_id: GenId;
        "047112FC535518D289E64FBE0B60F06E" as archive_branch_id: GenId;
        "A4DFF7BE658B1EA16F866E3039FFF8D6" as web_branch_id: GenId;
        "229941B84503AAE4976A49E020D1282B" as media_branch_id: GenId;
        "F0F90572249284CD57E48580369DEB6D" as author: Handle<Blake3, LongString>;
        "98A194178CFD7CBB915C1BC9EB561A7F" as author_role: Handle<Blake3, LongString>;
        "D1DC11B303725409AB8A30C6B59DB2D7" as persona_id: GenId;
        "79E1B50756FB64A30916E9353225E179" as active_llm_profile_id: GenId;
        "B919F28377B1241E4275808DBB1D423D" as active_llm_compaction_profile_id: GenId;
        "698519DFB681FABC3F06160ACAC9DA8E" as poll_ms: U256BE;
        "6691CF3F872C6107DCFAD0BCF7CDC1A0" as llm_profile_id: GenId;
        "85BE7BDA465B3CB0F800F76EEF8FAC9B" as llm_model: Handle<Blake3, LongString>;
        "B216CFBBF85AA1350B142D510E26268B" as llm_base_url: Handle<Blake3, LongString>;
        "55F3FFD721AF7C1258E45BC91CDBF30F" as llm_api_key: Handle<Blake3, LongString>;
        "328B29CE81665EE719C5A6E91695D4D4" as tavily_api_key: Handle<Blake3, LongString>;
        "AB0DF9F03F28A27A6DB95B693CC0EC53" as exa_api_key: Handle<Blake3, LongString>;
        "BA4E05799CA2ACDCF3F9350FC8742F2F" as llm_reasoning_effort: Handle<Blake3, LongString>;
        "5F04F7A0EB4EBBE6161022B336F83513" as llm_stream: U256BE;
        "F9CEA1A2E81D738BB125B4D144B7A746" as llm_context_window_tokens: U256BE;
        "4200F6746B36F2784DEBA1555595D6AC" as llm_max_output_tokens: U256BE;
        "1FF004BB48F7A4F8F72541F4D4FA75FF" as llm_prompt_safety_margin_tokens: U256BE;
        "095FAECDB8FF205DF591DF594E593B01" as llm_prompt_chars_per_token: U256BE;
        "167BABF8DFCD69AB4DB69773AAB18C4B" as memory_compaction_arity: U256BE;
        "24CF9D532E03C44CF719546DDE7E0493" as memory_lens_id: GenId;
        "1F0A596CD677F732CD5C506F74C61F6B" as memory_lens_prompt: Handle<Blake3, LongString>;
        "1067F34FE4517B058A74BC2118868DA4" as memory_lens_compaction_prompt: Handle<Blake3, LongString>;
        "84F32838DC66B0FB6F774150854521F8" as memory_lens_max_output_tokens: U256BE;
        "120F9C6BBB103FAFFB31A66E2ABC15E6" as exec_default_cwd: Handle<Blake3, LongString>;
        "D18A351B6E03A460E4F400D97D285F96" as exec_sandbox_profile: GenId;
    }
}

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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Show active headspace settings and available profiles
    Show {
        #[arg(long, default_value_t = false)]
        show_secrets: bool,
    },
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

#[derive(Clone, Debug)]
struct Config {
    pile_path: PathBuf,
    llm: LlmConfig,
    llm_profile_id: Option<Id>,
    llm_profile_name: String,
    llm_compaction_profile_id: Option<Id>,
    memory_compaction_arity: u64,
    memory_lenses: Vec<MemoryLensConfig>,
    tavily_api_key: Option<String>,
    exa_api_key: Option<String>,
    exec: ExecConfig,
    system_prompt: String,
    branch_id: Option<Id>,
    branch: String,
    compass_branch_id: Option<Id>,
    exec_branch_id: Option<Id>,
    local_messages_branch_id: Option<Id>,
    relations_branch_id: Option<Id>,
    teams_branch_id: Option<Id>,
    workspace_branch_id: Option<Id>,
    archive_branch_id: Option<Id>,
    web_branch_id: Option<Id>,
    media_branch_id: Option<Id>,
    author: String,
    author_role: String,
    persona_id: Option<Id>,
    poll_ms: u64,
}

#[derive(Clone, Debug)]
struct LlmConfig {
    model: String,
    base_url: String,
    api_key: Option<String>,
    reasoning_effort: Option<String>,
    stream: bool,
    context_window_tokens: u64,
    max_output_tokens: u64,
    prompt_safety_margin_tokens: u64,
    prompt_chars_per_token: u64,
}

#[derive(Clone, Debug)]
struct ExecConfig {
    default_cwd: Option<PathBuf>,
    sandbox_profile: Option<Id>,
}

#[derive(Clone, Debug)]
struct MemoryLensConfig {
    id: Id,
    name: String,
    prompt: String,
    compaction_prompt: String,
    max_output_tokens: u64,
}

#[derive(Clone, Debug)]
struct LlmProfileSummary {
    id: Id,
    name: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command.as_ref() else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Command::Show { show_secrets } => {
            let config = load_config(cli.pile.as_path())?;
            print_headspace(&config, *show_secrets)?;
        }
        Command::List => {
            let config = load_config(cli.pile.as_path())?;
            print_profile_list(&config)?;
        }
        Command::Use { profile } => {
            let mut config = load_config(cli.pile.as_path())?;
            let profile_id = resolve_profile_selector(cli.pile.as_path(), profile.as_str())?;
            let Some((llm, name)) = load_llm_profile(cli.pile.as_path(), profile_id)? else {
                return Err(anyhow!("unknown profile {profile_id:x}"));
            };
            config.llm_profile_id = Some(profile_id);
            config.llm_profile_name = name;
            config.llm = llm;
            store_config_to_pile(config)?;
            let config = load_config(cli.pile.as_path())?;
            print_headspace(&config, false)?;
        }
        Command::Add(args) => {
            let mut config = load_config(cli.pile.as_path())?;
            config.llm_profile_id = Some(*genid());
            config.llm_profile_name = args.name.clone();
            apply_add_overrides(&mut config, args)?;
            store_config_to_pile(config)?;
            let config = load_config(cli.pile.as_path())?;
            print_headspace(&config, false)?;
        }
        Command::Set { field, value } => {
            let mut config = load_config(cli.pile.as_path())?;
            apply_set(&mut config, *field, value.as_str())?;
            store_config_to_pile(config)?;
            let config = load_config(cli.pile.as_path())?;
            print_headspace(&config, false)?;
        }
        Command::Unset { field } => {
            let mut config = load_config(cli.pile.as_path())?;
            apply_unset(&mut config, *field)?;
            store_config_to_pile(config)?;
            let config = load_config(cli.pile.as_path())?;
            print_headspace(&config, false)?;
        }
        Command::Lens { command } => {
            let mut config = load_config(cli.pile.as_path())?;
            if let LensCommand::List = command {
                let _ = apply_lens(&mut config, LensCommand::List)?;
                return Ok(());
            }
            let changed = apply_lens(&mut config, command.clone())?;
            if changed {
                store_config_to_pile(config)?;
            }
            let config = load_config(cli.pile.as_path())?;
            let mut config = config;
            let _ = apply_lens(&mut config, LensCommand::List)?;
        }
    }

    Ok(())
}

fn apply_add_overrides(config: &mut Config, args: &AddArgs) -> Result<()> {
    if let Some(value) = args.model.as_deref() {
        config.llm.model = value.to_string();
    }
    if let Some(value) = args.base_url.as_deref() {
        config.llm.base_url = value.to_string();
    }
    if let Some(value) = args.api_key.as_deref() {
        config.llm.api_key = Some(value.trim().to_string());
    }
    if let Some(value) = args.reasoning_effort.as_deref() {
        config.llm.reasoning_effort = Some(value.trim().to_string());
    }
    if let Some(value) = args.stream {
        config.llm.stream = value;
    }
    if let Some(value) = args.context_window_tokens {
        config.llm.context_window_tokens = value;
    }
    if let Some(value) = args.max_output_tokens {
        config.llm.max_output_tokens = value;
    }
    if let Some(value) = args.prompt_safety_margin_tokens {
        config.llm.prompt_safety_margin_tokens = value;
    }
    if let Some(value) = args.prompt_chars_per_token {
        config.llm.prompt_chars_per_token = value;
    }
    Ok(())
}

fn apply_set(config: &mut Config, field: SetField, value: &str) -> Result<()> {
    match field {
        SetField::Model => config.llm.model = load_value_or_file(value, "llm_model")?,
        SetField::BaseUrl => config.llm.base_url = load_value_or_file(value, "llm_base_url")?,
        SetField::ApiKey => {
            config.llm.api_key = Some(load_value_or_file_trimmed(value, "llm_api_key")?)
        }
        SetField::ReasoningEffort => {
            config.llm.reasoning_effort =
                Some(load_value_or_file_trimmed(value, "llm_reasoning_effort")?)
        }
        SetField::Stream => config.llm.stream = parse_bool(value, "llm_stream")?,
        SetField::ContextWindowTokens => {
            config.llm.context_window_tokens = parse_u64(value, "llm_context_window_tokens")?
        }
        SetField::MaxOutputTokens => {
            config.llm.max_output_tokens = parse_u64(value, "llm_max_output_tokens")?
        }
        SetField::PromptSafetyMarginTokens => {
            config.llm.prompt_safety_margin_tokens =
                parse_u64(value, "llm_prompt_safety_margin_tokens")?
        }
        SetField::PromptCharsPerToken => {
            config.llm.prompt_chars_per_token = parse_u64(value, "llm_prompt_chars_per_token")?
        }
        SetField::CompactionProfileId => {
            config.llm_compaction_profile_id =
                Some(parse_hex_id(value, "llm_compaction_profile_id")?)
        }
    }
    Ok(())
}

fn apply_unset(config: &mut Config, field: UnsetField) -> Result<()> {
    match field {
        UnsetField::ApiKey => config.llm.api_key = None,
        UnsetField::ReasoningEffort => config.llm.reasoning_effort = None,
        UnsetField::CompactionProfileId => config.llm_compaction_profile_id = None,
    }
    Ok(())
}

fn apply_lens(config: &mut Config, command: LensCommand) -> Result<bool> {
    match command {
        LensCommand::List => {
            sort_memory_lenses(config);
            for lens in &config.memory_lenses {
                println!(
                    "{}\t{:x}\tmax_output_tokens={}",
                    lens.name, lens.id, lens.max_output_tokens
                );
            }
            Ok(false)
        }
        LensCommand::Add(args) => {
            if config
                .memory_lenses
                .iter()
                .any(|lens| lens.name.eq_ignore_ascii_case(args.name.as_str()))
            {
                return Err(anyhow!("memory lens '{}' already exists", args.name));
            }
            let mut lens = default_memory_lens_template(args.name.as_str())?;
            if let Some(id) = args.id {
                lens.id = parse_hex_id(id.as_str(), "memory_lens_id")?;
            }
            if let Some(prompt) = args.prompt {
                lens.prompt = load_value_or_file(prompt.as_str(), "memory_lens_prompt")?;
            }
            if let Some(prompt) = args.compaction_prompt {
                lens.compaction_prompt =
                    load_value_or_file(prompt.as_str(), "memory_lens_compaction_prompt")?;
            }
            if let Some(tokens) = args.max_output_tokens {
                lens.max_output_tokens = tokens;
            }
            config.memory_lenses.push(lens);
            sort_memory_lenses(config);
            Ok(true)
        }
        LensCommand::Set(args) => {
            let lens = get_memory_lens_mut(config, args.name.as_str())?;
            apply_memory_lens_set(lens, args.field, args.value.as_str())?;
            Ok(true)
        }
        LensCommand::Reset(args) => {
            let lens = get_memory_lens_mut(config, args.name.as_str())?;
            apply_memory_lens_reset(lens, args.field)?;
            Ok(true)
        }
        LensCommand::Remove { name } => {
            if config.memory_lenses.len() <= 1 {
                return Err(anyhow!("cannot remove the last memory lens"));
            }
            let before = config.memory_lenses.len();
            config
                .memory_lenses
                .retain(|lens| !lens.name.eq_ignore_ascii_case(name.as_str()));
            if config.memory_lenses.len() == before {
                return Err(anyhow!("memory lens '{}' not configured", name));
            }
            Ok(true)
        }
    }
}

fn resolve_profile_selector(pile_path: &Path, raw: &str) -> Result<Id> {
    if let Ok(id) = parse_hex_id(raw, "profile_id") {
        return Ok(id);
    }

    let needle = raw.trim().to_lowercase();
    let profiles = list_llm_profiles(pile_path)?;
    let mut matches = profiles
        .into_iter()
        .filter(|profile| profile.name.to_lowercase() == needle);
    let Some(first) = matches.next() else {
        return Err(anyhow!("unknown profile '{raw}'"));
    };
    if matches.next().is_some() {
        return Err(anyhow!("profile name '{raw}' is ambiguous; use the hex id"));
    }
    Ok(first.id)
}

fn default_memory_lens_template(name: &str) -> Result<MemoryLensConfig> {
    if let Some(mut lens) = default_memory_lens_by_name(name) {
        lens.id = *genid();
        lens.name = name.to_string();
        return Ok(lens);
    }
    let mut lens = default_memory_lens_by_name("factual")
        .ok_or_else(|| anyhow!("missing default memory lens 'factual'"))?;
    lens.id = *genid();
    lens.name = name.to_string();
    Ok(lens)
}

fn memory_lens_defaults(name: &str) -> Result<MemoryLensConfig> {
    if let Some(lens) = default_memory_lens_by_name(name) {
        return Ok(lens);
    }
    default_memory_lens_by_name("factual")
        .ok_or_else(|| anyhow!("missing default memory lens 'factual'"))
}

fn get_memory_lens_mut<'a>(config: &'a mut Config, name: &str) -> Result<&'a mut MemoryLensConfig> {
    config
        .memory_lenses
        .iter_mut()
        .find(|lens| lens.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow!("memory lens '{name}' not configured"))
}

fn sort_memory_lenses(config: &mut Config) {
    config.memory_lenses.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn apply_memory_lens_set(lens: &mut MemoryLensConfig, field: LensField, value: &str) -> Result<()> {
    match field {
        LensField::Id => {
            lens.id = parse_hex_id(value, "memory_lens_id")?;
        }
        LensField::Prompt => {
            lens.prompt = load_value_or_file(value, "memory_lens_prompt")?;
        }
        LensField::CompactionPrompt => {
            lens.compaction_prompt = load_value_or_file(value, "memory_lens_compaction_prompt")?;
        }
        LensField::MaxOutputTokens => {
            lens.max_output_tokens = parse_u64(value, "memory_lens_max_output_tokens")?;
        }
    }
    Ok(())
}

fn apply_memory_lens_reset(lens: &mut MemoryLensConfig, field: Option<LensField>) -> Result<()> {
    let defaults = memory_lens_defaults(lens.name.as_str())?;
    match field {
        None => {
            lens.prompt = defaults.prompt;
            lens.compaction_prompt = defaults.compaction_prompt;
            lens.max_output_tokens = defaults.max_output_tokens;
        }
        Some(LensField::Prompt) => lens.prompt = defaults.prompt,
        Some(LensField::CompactionPrompt) => lens.compaction_prompt = defaults.compaction_prompt,
        Some(LensField::MaxOutputTokens) => lens.max_output_tokens = defaults.max_output_tokens,
        Some(LensField::Id) => {
            return Err(anyhow!(
                "cannot reset lens id automatically; set it explicitly with `headspace lens set <name> id <hex>`"
            ));
        }
    }
    Ok(())
}

fn format_option_quoted(value: Option<&str>) -> String {
    value
        .map(|v| format!("\"{v}\""))
        .unwrap_or_else(|| "null".to_string())
}

fn redact_option(value: Option<&str>) -> String {
    match value {
        Some(_) => "\"<redacted>\"".to_string(),
        None => "null".to_string(),
    }
}

fn print_headspace(config: &Config, show_secrets: bool) -> Result<()> {
    println!("active:");
    println!(
        "  profile_id = {}",
        config
            .llm_profile_id
            .map(|id| format!("\"{id:x}\""))
            .unwrap_or_else(|| "null".to_string())
    );
    println!("  profile_name = \"{}\"", config.llm_profile_name);
    println!("  model = \"{}\"", config.llm.model);
    println!("  base_url = \"{}\"", config.llm.base_url);
    println!(
        "  api_key = {}",
        if show_secrets {
            format_option_quoted(config.llm.api_key.as_deref())
        } else {
            redact_option(config.llm.api_key.as_deref())
        }
    );
    println!(
        "  reasoning_effort = {}",
        format_option_quoted(config.llm.reasoning_effort.as_deref())
    );
    println!("  stream = {}", config.llm.stream);
    println!(
        "  context_window_tokens = {}",
        config.llm.context_window_tokens
    );
    println!("  max_output_tokens = {}", config.llm.max_output_tokens);
    println!(
        "  prompt_safety_margin_tokens = {}",
        config.llm.prompt_safety_margin_tokens
    );
    println!(
        "  prompt_chars_per_token = {}",
        config.llm.prompt_chars_per_token
    );
    println!(
        "  compaction_profile_id = {}",
        config
            .llm_compaction_profile_id
            .map(|id| format!("\"{id:x}\""))
            .unwrap_or_else(|| "null".to_string())
    );
    println!();
    println!("profiles:");
    print_profile_list(config)
}

fn print_profile_list(config: &Config) -> Result<()> {
    let profiles = list_llm_profiles(config.pile_path.as_path())?;
    for profile in profiles {
        let active = (config.llm_profile_id == Some(profile.id)).then_some("*");
        let active = active.unwrap_or(" ");
        println!("{active} {}\t{:x}", profile.name, profile.id);
    }
    Ok(())
}

fn now_epoch() -> Epoch {
    Epoch::now().unwrap_or_else(|_| Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0))
}

fn epoch_interval(epoch: Epoch) -> Value<NsTAIInterval> {
    (epoch, epoch).to_value()
}

fn interval_key(interval: Value<NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.from_value();
    lower.to_tai_duration().total_nanoseconds()
}

fn ensure_branch(repo: &mut Repository<Pile<Blake3>>, branch_id: Id, name: &str) -> Result<()> {
    if repo
        .storage_mut()
        .head(branch_id)
        .map_err(|err| anyhow!("read branch {branch_id:x} head: {err:?}"))?
        .is_some()
    {
        return Ok(());
    }

    let name_blob = name.to_owned().to_blob();
    let name_handle = name_blob.get_handle::<Blake3>();
    repo.storage_mut()
        .put(name_blob)
        .map_err(|err| anyhow!("store branch name blob for {name}: {err:?}"))?;

    let metadata = branch_meta::branch_unsigned(branch_id, name_handle, None);
    let metadata_handle = repo
        .storage_mut()
        .put(metadata.to_blob())
        .map_err(|err| anyhow!("store branch metadata for {name}: {err:?}"))?;

    let push_result = repo
        .storage_mut()
        .update(branch_id, None, Some(metadata_handle))
        .map_err(|err| anyhow!("create branch {name} ({branch_id:x}): {err:?}"))?;
    match push_result {
        PushResult::Success() | PushResult::Conflict(_) => Ok(()),
    }
}

fn push_workspace(
    repo: &mut Repository<Pile<Blake3>>,
    ws: &mut Workspace<Pile<Blake3>>,
) -> Result<()> {
    while let Some(mut conflict) = repo
        .try_push(ws)
        .map_err(|err| anyhow!("push workspace: {err:?}"))?
    {
        conflict
            .merge(ws)
            .map_err(|err| anyhow!("merge workspace: {err:?}"))?;
        *ws = conflict;
    }
    Ok(())
}

fn close_repo(repo: Repository<Pile<Blake3>>) -> Result<()> {
    repo.into_storage().close().context("close pile")
}

fn open_config_repo(pile_path: &Path) -> Result<(Repository<Pile<Blake3>>, Id)> {
    if let Some(parent) = pile_path.parent() {
        fs::create_dir_all(parent).context("create pile directory")?;
    }

    let mut pile = Pile::<Blake3>::open(pile_path).context("open pile")?;
    if let Err(err) = pile.restore().context("restore pile") {
        let close_res = pile.close().context("close pile after restore failure");
        if let Err(close_err) = close_res {
            eprintln!("warning: failed to close pile cleanly: {close_err:#}");
        }
        return Err(err);
    }

    let mut repo = Repository::new(pile, SigningKey::generate(&mut OsRng));
    let branch_id = match ensure_config_branch(&mut repo) {
        Ok(branch_id) => branch_id,
        Err(err) => {
            let close_res = repo.close().context("close pile after init failure");
            if let Err(close_err) = close_res {
                eprintln!("warning: failed to close pile cleanly: {close_err:#}");
            }
            return Err(err);
        }
    };
    Ok((repo, branch_id))
}

fn ensure_config_branch(repo: &mut Repository<Pile<Blake3>>) -> Result<Id> {
    ensure_branch(repo, CONFIG_BRANCH_ID, CONFIG_BRANCH)
        .context("materialize fixed config branch")?;
    Ok(CONFIG_BRANCH_ID)
}

fn load_config(pile_path: &Path) -> Result<Config> {
    let (mut repo, branch_id) = open_config_repo(pile_path)?;
    let result = (|| -> Result<Config> {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|err| anyhow!("pull config workspace: {err:?}"))?;
        let catalog = ws.checkout(..).context("checkout config workspace")?;

        let mut config = if let Some(config) = load_latest_config(&mut ws, &catalog, pile_path)? {
            config
        } else {
            default_config(pile_path.to_path_buf())
        };

        let ids_changed = ensure_registered_branch_ids(&mut config);
        let lenses_missing = !has_memory_lens_entries(&catalog);
        if ids_changed || lenses_missing {
            store_config(&mut ws, &config).context("store config with branch ids")?;
            push_workspace(&mut repo, &mut ws).context("push config with branch ids")?;
        }
        ensure_registered_branches_exist(&mut repo, &config)?;
        Ok(config)
    })();

    if let Err(err) = close_repo(repo).context("close config pile") {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }

    result
}

fn store_config_to_pile(config: Config) -> Result<()> {
    let (mut repo, branch_id) = open_config_repo(config.pile_path.as_path())?;
    let result = (|| -> Result<()> {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|err| anyhow!("pull config workspace: {err:?}"))?;
        store_config(&mut ws, &config).context("store config")?;
        push_workspace(&mut repo, &mut ws).context("push config")?;
        Ok(())
    })();

    if let Err(err) = close_repo(repo).context("close config pile") {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }

    result
}

fn list_llm_profiles(pile_path: &Path) -> Result<Vec<LlmProfileSummary>> {
    let (mut repo, branch_id) = open_config_repo(pile_path)?;
    let result = (|| -> Result<Vec<LlmProfileSummary>> {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|err| anyhow!("pull config workspace: {err:?}"))?;
        let catalog = ws.checkout(..).context("checkout config workspace")?;

        let mut latest: HashMap<Id, (Id, i128)> = HashMap::new();
        for (entry_id, profile_id, updated_at) in find!(
            (entry_id: Id, profile_id: Value<GenId>, updated_at: Value<NsTAIInterval>),
            pattern!(&catalog, [{
                ?entry_id @
                playground_config::kind: KIND_LLM_PROFILE_ID,
                playground_config::updated_at: ?updated_at,
                playground_config::llm_profile_id: ?profile_id,
            }])
        ) {
            let profile_id = Id::from_value(&profile_id);
            let key = interval_key(updated_at);
            latest
                .entry(profile_id)
                .and_modify(|slot| {
                    if key > slot.1 {
                        *slot = (entry_id, key);
                    }
                })
                .or_insert((entry_id, key));
        }

        let mut profiles = Vec::new();
        for (profile_id, (entry_id, _updated_key)) in latest {
            let name = load_string_attr(&mut ws, &catalog, entry_id, metadata::name)?
                .unwrap_or_else(|| format!("profile-{profile_id:x}"));
            profiles.push(LlmProfileSummary {
                id: profile_id,
                name,
            });
        }
        profiles.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(profiles)
    })();

    if let Err(err) = close_repo(repo).context("close config pile") {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }

    result
}

fn load_llm_profile(pile_path: &Path, profile_id: Id) -> Result<Option<(LlmConfig, String)>> {
    let (mut repo, branch_id) = open_config_repo(pile_path)?;
    let result = (|| -> Result<Option<(LlmConfig, String)>> {
        let mut ws = repo
            .pull(branch_id)
            .map_err(|err| anyhow!("pull config workspace: {err:?}"))?;
        let catalog = ws.checkout(..).context("checkout config workspace")?;
        load_latest_llm_profile(&mut ws, &catalog, profile_id)
    })();

    if let Err(err) = close_repo(repo).context("close config pile") {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }

    result
}

fn ensure_registered_branch_ids(config: &mut Config) -> bool {
    let mut changed = false;

    changed |= ensure_registered_branch_id(&mut config.branch_id);
    changed |= ensure_registered_branch_id(&mut config.exec_branch_id);
    changed |= ensure_registered_branch_id(&mut config.compass_branch_id);
    changed |= ensure_registered_branch_id(&mut config.local_messages_branch_id);
    changed |= ensure_registered_branch_id(&mut config.relations_branch_id);
    changed |= ensure_registered_branch_id(&mut config.workspace_branch_id);
    changed |= ensure_registered_branch_id(&mut config.archive_branch_id);
    changed |= ensure_registered_branch_id(&mut config.web_branch_id);
    changed |= ensure_registered_branch_id(&mut config.media_branch_id);
    changed |= ensure_registered_llm_profile_id(&mut config.llm_profile_id);

    changed
}

fn ensure_registered_branch_id(slot: &mut Option<Id>) -> bool {
    if slot.is_some() {
        return false;
    }
    *slot = Some(*genid());
    true
}

fn ensure_registered_llm_profile_id(slot: &mut Option<Id>) -> bool {
    if slot.is_some() {
        return false;
    }
    *slot = Some(*genid());
    true
}

fn ensure_registered_branches_exist(
    repo: &mut Repository<Pile<Blake3>>,
    config: &Config,
) -> Result<()> {
    let required = [
        (config.branch_id, config.branch.as_str()),
        (config.exec_branch_id, DEFAULT_EXEC_BRANCH),
        (config.compass_branch_id, DEFAULT_COMPASS_BRANCH),
        (
            config.local_messages_branch_id,
            DEFAULT_LOCAL_MESSAGES_BRANCH,
        ),
        (config.relations_branch_id, DEFAULT_RELATIONS_BRANCH),
        (config.workspace_branch_id, DEFAULT_WORKSPACE_BRANCH),
        (config.archive_branch_id, DEFAULT_ARCHIVE_BRANCH),
        (config.web_branch_id, DEFAULT_WEB_BRANCH),
        (config.media_branch_id, DEFAULT_MEDIA_BRANCH),
    ];

    for (id, name) in required {
        let id = id.ok_or_else(|| anyhow!("config missing id for branch '{name}'"))?;
        ensure_branch(repo, id, name)
            .with_context(|| format!("materialize branch '{name}' ({id:x})"))?;
    }

    if let Some(id) = config.teams_branch_id {
        ensure_branch(repo, id, DEFAULT_TEAMS_BRANCH)
            .with_context(|| format!("materialize branch '{}' ({id:x})", DEFAULT_TEAMS_BRANCH))?;
    }
    Ok(())
}

fn has_memory_lens_entries(catalog: &TribleSet) -> bool {
    find!(
        (
            entity_id: Id,
            prompt: Value<Handle<Blake3, LongString>>,
            compaction_prompt: Value<Handle<Blake3, LongString>>,
            max_output_tokens: Value<U256BE>
        ),
        pattern!(catalog, [{
            ?entity_id @
            playground_config::kind: KIND_MEMORY_LENS_ID,
            playground_config::memory_lens_prompt: ?prompt,
            playground_config::memory_lens_compaction_prompt: ?compaction_prompt,
            playground_config::memory_lens_max_output_tokens: ?max_output_tokens,
        }])
    )
    .into_iter()
    .next()
    .is_some()
}

fn latest_memory_lens_entries(catalog: &TribleSet) -> HashMap<Id, (Id, i128)> {
    let mut latest: HashMap<Id, (Id, i128)> = HashMap::new();
    for (entry_id, lens_id, updated_at) in find!(
        (
            entry_id: Id,
            lens_id: Value<GenId>,
            updated_at: Value<NsTAIInterval>
        ),
        pattern!(catalog, [{
            ?entry_id @
            playground_config::kind: KIND_MEMORY_LENS_ID,
            playground_config::updated_at: ?updated_at,
            playground_config::memory_lens_id: ?lens_id,
        }])
    ) {
        let lens_id = Id::from_value(&lens_id);
        let key = interval_key(updated_at);
        latest
            .entry(lens_id)
            .and_modify(|slot| {
                if key > slot.1 || (key == slot.1 && entry_id > slot.0) {
                    *slot = (entry_id, key);
                }
            })
            .or_insert((entry_id, key));
    }
    latest
}

fn load_latest_config(
    ws: &mut Workspace<Pile<Blake3>>,
    catalog: &TribleSet,
    pile_path: &Path,
) -> Result<Option<Config>> {
    let mut latest: Option<(Id, i128)> = None;

    for (config_id, updated_at) in find!(
        (config_id: Id, updated_at: Value<NsTAIInterval>),
        pattern!(catalog, [{
            ?config_id @
            playground_config::kind: KIND_CONFIG_ID,
            playground_config::updated_at: ?updated_at,
        }])
    ) {
        let key = interval_key(updated_at);
        match latest {
            Some((current_id, current_key))
                if current_key > key || (current_key == key && current_id >= config_id) => {}
            _ => latest = Some((config_id, key)),
        }
    }

    let Some((config_id, config_updated_key)) = latest else {
        return Ok(None);
    };

    let mut config = default_config(pile_path.to_path_buf());

    if let Some(prompt) =
        load_string_attr(ws, catalog, config_id, playground_config::system_prompt)?
    {
        config.system_prompt = prompt;
    }
    if let Some(branch) = load_string_attr(ws, catalog, config_id, playground_config::branch)? {
        config.branch = branch;
    }
    if let Some(author) = load_string_attr(ws, catalog, config_id, playground_config::author)? {
        config.author = author;
    }
    if let Some(role) = load_string_attr(ws, catalog, config_id, playground_config::author_role)? {
        config.author_role = role;
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::persona_id) {
        config.persona_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::active_llm_profile_id) {
        config.llm_profile_id = Some(id);
    }
    if let Some(id) = load_id_attr(
        catalog,
        config_id,
        playground_config::active_llm_compaction_profile_id,
    ) {
        config.llm_compaction_profile_id = Some(id);
    }
    if let Some(model) = load_string_attr(ws, catalog, config_id, playground_config::llm_model)? {
        config.llm.model = model;
    }
    if let Some(url) = load_string_attr(ws, catalog, config_id, playground_config::llm_base_url)? {
        config.llm.base_url = url;
    }
    if let Some(effort) = load_string_attr(
        ws,
        catalog,
        config_id,
        playground_config::llm_reasoning_effort,
    )? {
        config.llm.reasoning_effort = Some(effort);
    }
    if let Some(key) = load_string_attr(ws, catalog, config_id, playground_config::llm_api_key)? {
        config.llm.api_key = Some(key);
    }
    if let Some(key) = load_string_attr(ws, catalog, config_id, playground_config::tavily_api_key)?
    {
        config.tavily_api_key = Some(key);
    }
    if let Some(key) = load_string_attr(ws, catalog, config_id, playground_config::exa_api_key)? {
        config.exa_api_key = Some(key);
    }
    if let Some(cwd) =
        load_string_attr(ws, catalog, config_id, playground_config::exec_default_cwd)?
    {
        config.exec.default_cwd = Some(PathBuf::from(cwd));
    }

    if let Some(id) = load_id_attr(catalog, config_id, playground_config::branch_id) {
        config.branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::compass_branch_id) {
        config.compass_branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::exec_branch_id) {
        config.exec_branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(
        catalog,
        config_id,
        playground_config::local_messages_branch_id,
    ) {
        config.local_messages_branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::relations_branch_id) {
        config.relations_branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::teams_branch_id) {
        config.teams_branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::workspace_branch_id) {
        config.workspace_branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::archive_branch_id) {
        config.archive_branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::web_branch_id) {
        config.web_branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::media_branch_id) {
        config.media_branch_id = Some(id);
    }
    if let Some(id) = load_id_attr(catalog, config_id, playground_config::exec_sandbox_profile) {
        config.exec.sandbox_profile = Some(id);
    }
    if let Some(poll_ms) =
        load_u256_attr(catalog, config_id, playground_config::poll_ms).and_then(u256be_to_u64)
    {
        config.poll_ms = poll_ms;
    }
    if let Some(stream) =
        load_u256_attr(catalog, config_id, playground_config::llm_stream).and_then(u256be_to_u64)
    {
        config.llm.stream = stream != 0;
    }
    if let Some(tokens) = load_u256_attr(
        catalog,
        config_id,
        playground_config::llm_context_window_tokens,
    )
    .and_then(u256be_to_u64)
    {
        config.llm.context_window_tokens = tokens;
    }
    if let Some(tokens) =
        load_u256_attr(catalog, config_id, playground_config::llm_max_output_tokens)
            .and_then(u256be_to_u64)
    {
        config.llm.max_output_tokens = tokens;
    }
    if let Some(tokens) = load_u256_attr(
        catalog,
        config_id,
        playground_config::llm_prompt_safety_margin_tokens,
    )
    .and_then(u256be_to_u64)
    {
        config.llm.prompt_safety_margin_tokens = tokens;
    }
    if let Some(chars) = load_u256_attr(
        catalog,
        config_id,
        playground_config::llm_prompt_chars_per_token,
    )
    .and_then(u256be_to_u64)
    {
        config.llm.prompt_chars_per_token = chars;
    }
    if let Some(factor) = load_u256_attr(
        catalog,
        config_id,
        playground_config::memory_compaction_arity,
    )
    .and_then(u256be_to_u64)
    {
        config.memory_compaction_arity = factor.max(2);
    }

    if let Some(profile_id) = config.llm_profile_id {
        if let Some((llm, name)) = load_latest_llm_profile(ws, catalog, profile_id)? {
            config.llm = llm;
            config.llm_profile_name = name;
        }
    }

    let lenses = load_memory_lenses_for_snapshot(ws, catalog, config_updated_key)?;
    if !lenses.is_empty() {
        config.memory_lenses = lenses;
    }

    Ok(Some(config))
}

fn load_latest_llm_profile(
    ws: &mut Workspace<Pile<Blake3>>,
    catalog: &TribleSet,
    profile_id: Id,
) -> Result<Option<(LlmConfig, String)>> {
    let mut latest: Option<(Id, i128)> = None;

    for (entry_id, updated_at) in find!(
        (entry_id: Id, updated_at: Value<NsTAIInterval>),
        pattern!(catalog, [{
            ?entry_id @
            playground_config::kind: KIND_LLM_PROFILE_ID,
            playground_config::updated_at: ?updated_at,
            playground_config::llm_profile_id: profile_id,
        }])
    ) {
        let key = interval_key(updated_at);
        match latest {
            Some((current_id, current_key))
                if current_key > key || (current_key == key && current_id >= entry_id) => {}
            _ => latest = Some((entry_id, key)),
        }
    }

    let Some((entry_id, _)) = latest else {
        return Ok(None);
    };

    let mut llm = LlmConfig::default();
    if let Some(model) = load_string_attr(ws, catalog, entry_id, playground_config::llm_model)? {
        llm.model = model;
    }
    if let Some(url) = load_string_attr(ws, catalog, entry_id, playground_config::llm_base_url)? {
        llm.base_url = url;
    }
    if let Some(effort) = load_string_attr(
        ws,
        catalog,
        entry_id,
        playground_config::llm_reasoning_effort,
    )? {
        llm.reasoning_effort = Some(effort);
    }
    if let Some(key) = load_string_attr(ws, catalog, entry_id, playground_config::llm_api_key)? {
        llm.api_key = Some(key);
    }
    if let Some(stream) =
        load_u256_attr(catalog, entry_id, playground_config::llm_stream).and_then(u256be_to_u64)
    {
        llm.stream = stream != 0;
    }
    if let Some(tokens) = load_u256_attr(
        catalog,
        entry_id,
        playground_config::llm_context_window_tokens,
    )
    .and_then(u256be_to_u64)
    {
        llm.context_window_tokens = tokens;
    }
    if let Some(tokens) =
        load_u256_attr(catalog, entry_id, playground_config::llm_max_output_tokens)
            .and_then(u256be_to_u64)
    {
        llm.max_output_tokens = tokens;
    }
    if let Some(tokens) = load_u256_attr(
        catalog,
        entry_id,
        playground_config::llm_prompt_safety_margin_tokens,
    )
    .and_then(u256be_to_u64)
    {
        llm.prompt_safety_margin_tokens = tokens;
    }
    if let Some(chars) = load_u256_attr(
        catalog,
        entry_id,
        playground_config::llm_prompt_chars_per_token,
    )
    .and_then(u256be_to_u64)
    {
        llm.prompt_chars_per_token = chars;
    }
    let name = load_string_attr(ws, catalog, entry_id, metadata::name)?
        .unwrap_or_else(|| format!("profile-{profile_id:x}"));
    Ok(Some((llm, name)))
}

fn load_memory_lenses_for_snapshot(
    ws: &mut Workspace<Pile<Blake3>>,
    catalog: &TribleSet,
    snapshot_key: i128,
) -> Result<Vec<MemoryLensConfig>> {
    let latest = latest_memory_lens_entries(catalog);
    let mut lenses_by_name: HashMap<String, (MemoryLensConfig, i128)> = HashMap::new();
    for (lens_id, (entry_id, updated_key)) in latest {
        if updated_key != snapshot_key {
            continue;
        }
        let name = load_string_attr(ws, catalog, entry_id, metadata::name)?
            .unwrap_or_else(|| format!("lens-{lens_id:x}"));
        let prompt =
            load_string_attr(ws, catalog, entry_id, playground_config::memory_lens_prompt)?
                .ok_or_else(|| anyhow!("memory lens {lens_id:x} missing prompt"))?;
        let compaction_prompt = load_string_attr(
            ws,
            catalog,
            entry_id,
            playground_config::memory_lens_compaction_prompt,
        )?
        .ok_or_else(|| anyhow!("memory lens {lens_id:x} missing compaction_prompt"))?;
        let max_output_tokens = load_u256_attr(
            catalog,
            entry_id,
            playground_config::memory_lens_max_output_tokens,
        )
        .and_then(u256be_to_u64)
        .ok_or_else(|| anyhow!("memory lens {lens_id:x} missing max_output_tokens"))?;
        let lens = MemoryLensConfig {
            id: lens_id,
            name: name.clone(),
            prompt,
            compaction_prompt,
            max_output_tokens,
        };
        let key = name.to_lowercase();
        lenses_by_name
            .entry(key)
            .and_modify(|slot| {
                if updated_key > slot.1 {
                    *slot = (lens.clone(), updated_key);
                }
            })
            .or_insert((lens, updated_key));
    }
    let mut lenses: Vec<MemoryLensConfig> =
        lenses_by_name.into_values().map(|(lens, _)| lens).collect();
    lenses.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(lenses)
}

fn store_config(ws: &mut Workspace<Pile<Blake3>>, config: &Config) -> Result<()> {
    let now = epoch_interval(now_epoch());
    let config_id = ufoid();
    let profile_id = config
        .llm_profile_id
        .ok_or_else(|| anyhow!("config missing active LLM profile id"))?;
    let mut memory_lenses = if config.memory_lenses.is_empty() {
        default_memory_lenses()
    } else {
        config.memory_lenses.clone()
    };
    memory_lenses.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });

    let system_prompt = ws.put(config.system_prompt.clone());
    let branch = ws.put(config.branch.clone());
    let author = ws.put(config.author.clone());
    let author_role = ws.put(config.author_role.clone());
    let poll_ms: Value<U256BE> = config.poll_ms.to_value();

    let mut change = TribleSet::new();
    change += entity! { &config_id @
        playground_config::kind: KIND_CONFIG_ID,
        playground_config::updated_at: now,
        playground_config::system_prompt: system_prompt,
        playground_config::branch: branch,
        playground_config::author: author,
        playground_config::author_role: author_role,
        playground_config::poll_ms: poll_ms,
        playground_config::active_llm_profile_id: profile_id,
    };

    let memory_compaction_arity: Value<U256BE> =
        config.memory_compaction_arity.max(2).to_value();
    change += entity! { &config_id @
        playground_config::memory_compaction_arity: memory_compaction_arity,
    };

    if let Some(id) = config.branch_id {
        change += entity! { &config_id @ playground_config::branch_id: id };
    }
    if let Some(id) = config.compass_branch_id {
        change += entity! { &config_id @ playground_config::compass_branch_id: id };
    }
    if let Some(id) = config.exec_branch_id {
        change += entity! { &config_id @ playground_config::exec_branch_id: id };
    }
    if let Some(id) = config.local_messages_branch_id {
        change += entity! { &config_id @ playground_config::local_messages_branch_id: id };
    }
    if let Some(id) = config.relations_branch_id {
        change += entity! { &config_id @ playground_config::relations_branch_id: id };
    }
    if let Some(id) = config.teams_branch_id {
        change += entity! { &config_id @ playground_config::teams_branch_id: id };
    }
    if let Some(id) = config.workspace_branch_id {
        change += entity! { &config_id @ playground_config::workspace_branch_id: id };
    }
    if let Some(id) = config.archive_branch_id {
        change += entity! { &config_id @ playground_config::archive_branch_id: id };
    }
    if let Some(id) = config.web_branch_id {
        change += entity! { &config_id @ playground_config::web_branch_id: id };
    }
    if let Some(id) = config.media_branch_id {
        change += entity! { &config_id @ playground_config::media_branch_id: id };
    }
    if let Some(id) = config.persona_id {
        change += entity! { &config_id @ playground_config::persona_id: id };
    }
    if let Some(id) = config.llm_compaction_profile_id {
        change += entity! { &config_id @ playground_config::active_llm_compaction_profile_id: id };
    }
    if let Some(key) = config.tavily_api_key.as_ref() {
        let handle = ws.put(key.clone());
        change += entity! { &config_id @ playground_config::tavily_api_key: handle };
    }
    if let Some(key) = config.exa_api_key.as_ref() {
        let handle = ws.put(key.clone());
        change += entity! { &config_id @ playground_config::exa_api_key: handle };
    }
    if let Some(cwd) = config.exec.default_cwd.as_ref() {
        let handle = ws.put(cwd.to_string_lossy().to_string());
        change += entity! { &config_id @ playground_config::exec_default_cwd: handle };
    }
    if let Some(profile) = config.exec.sandbox_profile {
        change += entity! { &config_id @ playground_config::exec_sandbox_profile: profile };
    }

    let profile_entry_id = ufoid();
    let profile_name = ws.put(config.llm_profile_name.clone());
    let llm_model = ws.put(config.llm.model.clone());
    let llm_base_url = ws.put(config.llm.base_url.clone());
    let llm_stream: Value<U256BE> = if config.llm.stream { 1u64 } else { 0u64 }.to_value();
    let llm_context_window_tokens: Value<U256BE> = config.llm.context_window_tokens.to_value();
    let llm_max_output_tokens: Value<U256BE> = config.llm.max_output_tokens.to_value();
    let llm_prompt_safety_margin_tokens: Value<U256BE> =
        config.llm.prompt_safety_margin_tokens.to_value();
    let llm_prompt_chars_per_token: Value<U256BE> = config.llm.prompt_chars_per_token.to_value();

    change += entity! { &profile_entry_id @
        playground_config::kind: KIND_LLM_PROFILE_ID,
        playground_config::updated_at: now,
        playground_config::llm_profile_id: profile_id,
        metadata::name: profile_name,
        playground_config::llm_model: llm_model,
        playground_config::llm_base_url: llm_base_url,
        playground_config::llm_stream: llm_stream,
        playground_config::llm_context_window_tokens: llm_context_window_tokens,
        playground_config::llm_max_output_tokens: llm_max_output_tokens,
        playground_config::llm_prompt_safety_margin_tokens: llm_prompt_safety_margin_tokens,
        playground_config::llm_prompt_chars_per_token: llm_prompt_chars_per_token,
    };

    if let Some(key) = config.llm.api_key.as_ref() {
        let handle = ws.put(key.clone());
        change += entity! { &profile_entry_id @ playground_config::llm_api_key: handle };
    }
    if let Some(effort) = config.llm.reasoning_effort.as_ref() {
        let handle = ws.put(effort.clone());
        change += entity! { &profile_entry_id @ playground_config::llm_reasoning_effort: handle };
    }

    for lens in &memory_lenses {
        let lens_entry_id = ufoid();
        let lens_name = ws.put(lens.name.clone());
        let lens_prompt = ws.put(lens.prompt.clone());
        let lens_compaction_prompt = ws.put(lens.compaction_prompt.clone());
        let lens_max_output_tokens: Value<U256BE> = lens.max_output_tokens.to_value();
        change += entity! { &lens_entry_id @
            playground_config::kind: KIND_MEMORY_LENS_ID,
            playground_config::updated_at: now,
            playground_config::memory_lens_id: lens.id,
            metadata::name: lens_name,
            playground_config::memory_lens_prompt: lens_prompt,
            playground_config::memory_lens_compaction_prompt: lens_compaction_prompt,
            playground_config::memory_lens_max_output_tokens: lens_max_output_tokens,
        };
    }

    ws.commit(change, None, Some("playground config"));
    Ok(())
}

fn load_string_attr(
    ws: &mut Workspace<Pile<Blake3>>,
    catalog: &TribleSet,
    entity_id: Id,
    attr: Attribute<Handle<Blake3, LongString>>,
) -> Result<Option<String>> {
    let mut handles = find!(
        (entity: Id, handle: Value<Handle<Blake3, LongString>>),
        pattern!(catalog, [{ ?entity @ attr: ?handle }])
    )
    .into_iter()
    .filter(|(entity, _)| *entity == entity_id);

    let Some((_, handle)) = handles.next() else {
        return Ok(None);
    };
    if handles.next().is_some() {
        let attr_id = attr.id();
        return Err(anyhow!(
            "entity {entity_id:x} has multiple values for attribute {attr_id:x}"
        ));
    }

    let view: View<str> = ws.get(handle).context("read config text")?;
    Ok(Some(view.as_ref().to_string()))
}

fn load_id_attr(catalog: &TribleSet, entity_id: Id, attr: Attribute<GenId>) -> Option<Id> {
    find!(
        (entity: Id, value: Value<GenId>),
        pattern!(catalog, [{ ?entity @ attr: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == entity_id).then_some(Id::from_value(&value)))
}

fn load_u256_attr(
    catalog: &TribleSet,
    entity_id: Id,
    attr: Attribute<U256BE>,
) -> Option<Value<U256BE>> {
    find!(
        (entity: Id, value: Value<U256BE>),
        pattern!(catalog, [{ ?entity @ attr: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == entity_id).then_some(value))
}

fn u256be_to_u64(value: Value<U256BE>) -> Option<u64> {
    let raw = value.raw;
    if raw[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    let bytes: [u8; 8] = raw[24..32].try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            default_cwd: Some(PathBuf::from("/workspace")),
            sandbox_profile: None,
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: None,
            reasoning_effort: None,
            stream: DEFAULT_STREAM,
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            prompt_safety_margin_tokens: DEFAULT_PROMPT_SAFETY_MARGIN_TOKENS,
            prompt_chars_per_token: DEFAULT_PROMPT_CHARS_PER_TOKEN,
        }
    }
}

fn default_config(pile_path: PathBuf) -> Config {
    Config {
        pile_path,
        llm: LlmConfig::default(),
        llm_profile_id: None,
        llm_profile_name: "default".to_string(),
        llm_compaction_profile_id: None,
        memory_compaction_arity: 8,
        memory_lenses: default_memory_lenses(),
        tavily_api_key: None,
        exa_api_key: None,
        exec: ExecConfig::default(),
        system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        branch_id: None,
        branch: DEFAULT_BRANCH.to_string(),
        compass_branch_id: None,
        exec_branch_id: None,
        local_messages_branch_id: None,
        relations_branch_id: None,
        teams_branch_id: None,
        workspace_branch_id: None,
        archive_branch_id: None,
        web_branch_id: None,
        media_branch_id: None,
        author: DEFAULT_AUTHOR.to_string(),
        author_role: DEFAULT_AUTHOR_ROLE.to_string(),
        persona_id: None,
        poll_ms: DEFAULT_POLL_MS,
    }
}

fn default_memory_lenses() -> Vec<MemoryLensConfig> {
    vec![
        MemoryLensConfig {
            id: MEMORY_LENS_ID_FACTUAL,
            name: "factual".to_string(),
            prompt: DEFAULT_MEMORY_LENS_FACTUAL_PROMPT.to_string(),
            compaction_prompt: DEFAULT_MEMORY_LENS_FACTUAL_COMPACTION_PROMPT.to_string(),
            max_output_tokens: DEFAULT_MEMORY_LENS_FACTUAL_MAX_OUTPUT_TOKENS,
        },
        MemoryLensConfig {
            id: MEMORY_LENS_ID_TECHNICAL,
            name: "technical".to_string(),
            prompt: DEFAULT_MEMORY_LENS_TECHNICAL_PROMPT.to_string(),
            compaction_prompt: DEFAULT_MEMORY_LENS_TECHNICAL_COMPACTION_PROMPT.to_string(),
            max_output_tokens: DEFAULT_MEMORY_LENS_TECHNICAL_MAX_OUTPUT_TOKENS,
        },
        MemoryLensConfig {
            id: MEMORY_LENS_ID_EMOTIONAL,
            name: "emotional".to_string(),
            prompt: DEFAULT_MEMORY_LENS_EMOTIONAL_PROMPT.to_string(),
            compaction_prompt: DEFAULT_MEMORY_LENS_EMOTIONAL_COMPACTION_PROMPT.to_string(),
            max_output_tokens: DEFAULT_MEMORY_LENS_EMOTIONAL_MAX_OUTPUT_TOKENS,
        },
    ]
}

fn default_memory_lens_by_name(name: &str) -> Option<MemoryLensConfig> {
    default_memory_lenses()
        .into_iter()
        .find(|lens| lens.name.eq_ignore_ascii_case(name))
}

fn parse_hex_id(raw: &str, label: &str) -> Result<Id> {
    let raw = raw.trim();
    Id::from_hex(raw).ok_or_else(|| anyhow!("invalid {label} {raw}"))
}

fn load_value_or_file(raw: &str, label: &str) -> Result<String> {
    if let Some(path) = raw.strip_prefix('@') {
        return fs::read_to_string(path).with_context(|| format!("read {label} from {}", path));
    }
    Ok(raw.to_string())
}

fn load_value_or_file_trimmed(raw: &str, label: &str) -> Result<String> {
    Ok(load_value_or_file(raw, label)?.trim().to_string())
}

fn parse_u64(raw: &str, label: &str) -> Result<u64> {
    raw.parse::<u64>()
        .map_err(|_| anyhow!("invalid {label} {raw}"))
}

fn parse_bool(raw: &str, label: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(anyhow!("invalid {label} {raw} (expected true/false)")),
    }
}
