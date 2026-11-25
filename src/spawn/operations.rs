//! Spawn operations: launch a task zooid, settle its end, read what happened.
//!
//! A launch touches four things in order. Relations gives the zooid its
//! persona (refusing while a live one has the name), the opening context is
//! rendered, the unit is started through a [`Launcher`], and only then is the
//! spawn and its run published, so a launch that never started leaves no run
//! behind. The unit is pinned to every collection route the launch resolved,
//! so the zooid and its settlement read and write where the run was recorded.
//!
//! Settlement, the unit's stop-post step, records the end on the run,
//! messages the spawner as the zooid, and only then retires the persona: a
//! persona retired before its message went out could never send it. A failed
//! step is recorded on the run and the persona stays live, so settlement can
//! simply be run again. It is idempotent: an end already recorded is not
//! recorded twice, and a spawner already messaged is not messaged again.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anybytes::View;
use anyhow::{anyhow, bail, Context, Result};
use hifitime::Epoch;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{CollectionHandle, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::*;

use super::cover::{self, CoverRecord, MISTRAL_CONTEXT_TOKENS, PLAIN_MEMORY_CHARS};
use super::{Exit, HostKey, IntervalValue, Reported, RunFacts, ScriptFacts, TextHandle};
use crate::goals::{Goals, NoteOptions};
use crate::orient::WakeTint;
use crate::out::{Out, Part};
use crate::relations::{ProfileInput, Relations};
use crate::schemas::archive::archive as archive_attrs;
use crate::schemas::imagine::imagine as imagine_attrs;
use crate::schemas::spawn::{
    spawn as attrs, DEFAULT_SCOPE_ID, KIND_OUTCOME, KIND_REQUEST, KIND_RUN, KIND_SPAWN,
    OUTCOME_ACCEPTED, OUTCOME_REFUSED, OUTCOME_RUNNING, OUTCOME_TERMINAL,
};
use crate::storage::{AcquiringReader, FactArchive, FacultySnapshot, Storage};

/// Starts a unit and reports its state. The systemd implementation is
/// [`SystemdLauncher`]; tests substitute their own.
pub trait Launcher: Send + Sync {
    /// Start the unit and return once systemd has accepted it.
    fn start(&self, unit: &UnitSpec) -> Result<()>;
    /// systemd's `ActiveState` for the unit. A collected unit that has ended
    /// reads `inactive`.
    fn state(&self, unit: &str) -> Result<String>;
}

/// Whether an `ActiveState` means the unit has not finished yet. A unit in
/// its stop-post step is `deactivating`: settlement is still running.
pub fn is_running(state: &str) -> bool {
    matches!(
        state.trim(),
        "active" | "activating" | "deactivating" | "reloading" | "refreshing"
    )
}

/// One transient unit: the script run twice, once as the zooid and once
/// with `settle` as its stop-post step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitSpec {
    pub unit: String,
    pub workdir: PathBuf,
    pub script: PathBuf,
    pub log: PathBuf,
    pub memory_max: String,
    /// `RuntimeMaxSec`, when the unit has a runtime limit. Only local units
    /// have one ([`LaunchConfig::local_max_runtime`]).
    pub runtime_max: Option<u64>,
    /// Stop the unit by interrupting its main process (`KillSignal=SIGINT`,
    /// `KillMode=mixed`): a local zooid lives in Drive's loop, which ends
    /// on SIGINT after its ordinary teardown. Any stop, its runtime limit
    /// included, is then that end; the rest of the unit goes after it.
    pub interrupt: bool,
}

impl UnitSpec {
    /// The `systemd-run` arguments that start this unit.
    pub fn systemd_run_args(&self) -> Vec<String> {
        let script = self.script.display().to_string();
        let log = self.log.display().to_string();
        let mut args = vec![
            "--user".to_owned(),
            "--unit".to_owned(),
            self.unit.clone(),
            "--collect".to_owned(),
            "--quiet".to_owned(),
            "-p".to_owned(),
            format!("MemoryMax={}", self.memory_max),
            "-p".to_owned(),
            // Settlement opens the pile and publishes three times.
            "TimeoutStopSec=15min".to_owned(),
            "-p".to_owned(),
            format!("WorkingDirectory={}", self.workdir.display()),
            "-p".to_owned(),
            format!("StandardOutput=append:{log}"),
            "-p".to_owned(),
            format!("StandardError=append:{log}"),
            "-p".to_owned(),
            format!("ExecStopPost=/bin/bash {script} settle"),
        ];
        if let Some(seconds) = self.runtime_max {
            // Settlement runs after the limit fires: ExecStopPost is not
            // counted against RuntimeMaxSec.
            args.extend(["-p".to_owned(), format!("RuntimeMaxSec={seconds}")]);
        }
        if self.interrupt {
            // TimeoutStopSec above bounds the teardown before SIGKILL.
            // A local zooid's shell sandbox (playground's bubblewrap) needs a
            // delegated cgroup: the unit's own processes live in its
            // `supervisor` subgroup, so the unit's cgroup has no processes of
            // its own and can hand cpu, memory and pids to the sandbox's
            // groups beside it.
            args.extend([
                "-p".to_owned(),
                "KillSignal=SIGINT".to_owned(),
                "-p".to_owned(),
                "KillMode=mixed".to_owned(),
                "-p".to_owned(),
                "Delegate=cpu memory pids".to_owned(),
                "-p".to_owned(),
                "DelegateSubgroup=supervisor".to_owned(),
            ]);
        }
        args.extend(["--".to_owned(), "/bin/bash".to_owned(), script]);
        args
    }
}

/// `systemd-run --user` and `systemctl --user`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemdLauncher;

impl Launcher for SystemdLauncher {
    fn start(&self, unit: &UnitSpec) -> Result<()> {
        let status = Command::new("systemd-run")
            .args(unit.systemd_run_args())
            .status()
            .context("run systemd-run")?;
        if !status.success() {
            bail!("systemd-run refused unit {}: {status}", unit.unit);
        }
        Ok(())
    }

    fn state(&self, unit: &str) -> Result<String> {
        let output = Command::new("systemctl")
            .args(["--user", "show", "-p", "ActiveState", "--value", unit])
            .output()
            .context("run systemctl")?;
        if !output.status.success() {
            bail!(
                "systemctl could not read {unit}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

/// Renders the pile-derived part of the opening context.
pub trait Renderer: Send + Sync {
    /// Scores the memory cover's recollections against a topic. Its own
    /// step, so a launch can tell a tint that failed from a wake that did.
    fn tint(&self, storage: &Storage, about: &str) -> Result<WakeTint>;
    /// Render one wake. `memory_chars` is the journal's budget; a task's Knowledge
    /// readings come beside it, in an allowance of their own. Zero measures the
    /// fixed sections for cover sizing.
    /// A personal wake without a tint carries the complete standing beliefs.
    fn wake(
        &self,
        storage: &Storage,
        tint: Option<&WakeTint>,
        memory_chars: usize,
    ) -> Result<String>;
}

/// The tint through the Memory library and the wake through Orient's, on
/// the caller's storage.
#[derive(Clone, Copy, Debug, Default)]
pub struct LibraryRenderer;

impl Renderer for LibraryRenderer {
    fn tint(&self, storage: &Storage, about: &str) -> Result<WakeTint> {
        Ok(crate::orient::Orient::with_storage(storage.clone()).topic(about, PLAIN_MEMORY_CHARS))
    }

    fn wake(
        &self,
        storage: &Storage,
        tint: Option<&WakeTint>,
        memory_chars: usize,
    ) -> Result<String> {
        let orient = crate::orient::Orient::with_storage(storage.clone());
        let mut text = String::new();
        let mut collect = |part: Part| {
            if let Part::Text { text: part } = part {
                text.push_str(&part);
            }
            Ok(())
        };
        // No persona: the wake is the shared self, and a launch records no
        // presentation for anyone.
        orient.wake_unpresented(
            None,
            &crate::orient::WakeOptions {
                chars: memory_chars,
                tint: tint.cloned(),
                ..Default::default()
            },
            &mut Out::new(&mut collect),
        )?;
        Ok(text)
    }
}

/// Where a zooid's files live and what its unit runs.
#[derive(Clone, Debug)]
pub struct LaunchConfig {
    /// Prompt, script, report and log, one set per zooid name.
    pub state_dir: PathBuf,
    pub claude: PathBuf,
    /// The model claude zooids are told to use.
    pub model: String,
    /// The codex command, for `--provider codex`.
    pub codex: PathBuf,
    /// The model codex zooids are told to use (`codex -m`). `None` names
    /// none: codex then picks its own default, and the run records that no
    /// model was configured rather than a model it cannot know.
    pub codex_model: Option<String>,
    /// The file codex keeps its login in, outside any run. A codex run gets
    /// its own `CODEX_HOME` holding only a link to it, so it reads none of
    /// the ambient configuration (MCP servers, hook trust, model). `None`
    /// links nothing, for a codex that authenticates from its environment.
    pub codex_auth: Option<PathBuf>,
    /// The `vibe` command, for `--provider mistral`.
    pub vibe: PathBuf,
    /// The model mistral zooids run. The per-run vibe configuration prices
    /// it at [`MISTRAL_INPUT_PRICE`] and [`MISTRAL_OUTPUT_PRICE`], which are
    /// Large 4's, so this is not overridable from the environment.
    pub mistral_model: String,
    /// The exact Secrets version holding the Mistral API key. The unit reads
    /// it with `secrets get` just before vibe starts; it is never written to
    /// the prompt, a configuration file or a log. `SPAWN_MISTRAL_KEY_SECRET`
    /// overrides it.
    pub mistral_key_secret: String,
    /// The most one mistral run may spend, in US dollars (`vibe --max-price`,
    /// by vibe's own estimate from the configured prices).
    /// `SPAWN_MISTRAL_MAX_PRICE` overrides it.
    pub mistral_max_price: f64,
    /// The command the unit reads secrets with, found on `path` inside it.
    pub secrets: String,
    /// The colony's routes file. The unit sources it for what the launch
    /// did not resolve itself, and a launch refuses while it names a route
    /// the launch lacks.
    pub colony_env: Option<PathBuf>,
    pub path: String,
    pub memory_max: String,
    /// The command settlement runs, found on `path` inside the unit.
    pub spawn: String,
    /// The `faculties` binary each zooid's own MCP server runs. A relative
    /// name is resolved on `path` at launch, so the unit names an absolute
    /// file.
    pub faculties: PathBuf,
    /// The local executor, for `--provider local`: an argv template whose
    /// words may hold [`LOCAL_PLACEHOLDERS`]. `None` means this host has
    /// none, and local is refused. `SPAWN_LOCAL_EXECUTOR` sets it.
    pub local: Option<Vec<String>>,
    /// The longest a local unit may run, in seconds: its `RuntimeMaxSec`.
    /// `None` is no limit. Units of other providers never get one.
    /// `SPAWN_LOCAL_MAX_RUNTIME` sets it.
    pub local_max_runtime: Option<u64>,
    /// codex's model catalog (`models_cache.json` of the ambient codex
    /// home), read for a `--cover` launch's window when a codex model is
    /// named. `None` reads none.
    pub codex_models: Option<PathBuf>,
    /// claude's session transcripts (`~/.claude/projects`), read at
    /// settlement for the first model call's input tokens. `None` reads none.
    pub claude_projects: Option<PathBuf>,
}

/// The placeholders a local executor template may name, each replaced inside
/// a word: the prompt file (also the executor's stdin), the message file it
/// may write the zooid's closing words to, the zooid's name, the effort, the
/// unit script, which run as `{script} mcp` is the zooid's own faculties MCP
/// server, and the unit's
/// runtime limit in seconds (`SPAWN_LOCAL_MAX_RUNTIME`), which a template
/// may name only when the limit is set, and the archived node the zooid's
/// history ends at (32 hex digits), or nothing when it has none: a template
/// that names `{resume}` makes the launch look the zooid up in the archive,
/// and a lookup that refuses fails the launch. `{brief}` is a file holding
/// the zooid's first input without the wake (who she is and the brief), for
/// an executor that reads its own wake, as Drive does; `{spawner}` is the
/// spawning persona's label.
pub const LOCAL_PLACEHOLDERS: [&str; 9] = [
    "{prompt}",
    "{message}",
    "{name}",
    "{effort}",
    "{script}",
    "{runtime}",
    "{resume}",
    "{brief}",
    "{spawner}",
];

/// Why a template that names `{runtime}` cannot run without a limit.
const RUNTIME_UNSET: &str = "SPAWN_LOCAL_EXECUTOR names {runtime}, but SPAWN_LOCAL_MAX_RUNTIME \
     is unset, so the unit has no runtime limit to pass on. Nothing was started";

/// What to do when a host has no local executor.
const LOCAL_UNCONFIGURED: &str = "no local executor is configured on this host. Set \
     SPAWN_LOCAL_EXECUTOR to its argv template, words separated by whitespace, with {prompt}, \
     {message}, {name}, {effort}, {script}, {runtime}, {resume}, {brief} and {spawner} replaced at launch; the executor reads the \
     prompt on stdin and may write the zooid's closing message to {message}. Nothing falls back \
     to a hosted model";

/// A local executor template from `SPAWN_LOCAL_EXECUTOR`: whitespace
/// separates words, and there is no quoting (a wrapper script holds anything
/// more). Every `{...}` must be one of [`LOCAL_PLACEHOLDERS`]. None is
/// required: a closing message is optional, so a template that names no
/// `{message}` only means its runs never leave one.
fn parse_local_executor(template: &str) -> Result<Vec<String>> {
    let words: Vec<String> = template.split_whitespace().map(str::to_owned).collect();
    if words.is_empty() {
        bail!("SPAWN_LOCAL_EXECUTOR is empty");
    }
    for word in &words {
        let mut rest = word.as_str();
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                bail!("SPAWN_LOCAL_EXECUTOR word {word:?} has an unclosed {{");
            };
            let placeholder = &rest[open..open + close + 1];
            if !LOCAL_PLACEHOLDERS.contains(&placeholder) {
                bail!(
                    "SPAWN_LOCAL_EXECUTOR names {placeholder}, which is not one of {}",
                    LOCAL_PLACEHOLDERS.join(", ")
                );
            }
            rest = &rest[open + close + 1..];
        }
    }
    Ok(words)
}

/// Whether a local executor template names `{runtime}`.
fn names_runtime(template: &[String]) -> bool {
    template.iter().any(|word| word.contains("{runtime}"))
}

/// Whether a local executor template names `{resume}`.
fn names_resume(template: &[String]) -> bool {
    template.iter().any(|word| word.contains("{resume}"))
}

impl LaunchConfig {
    /// Sky's layout under one home directory: the pinned claude build,
    /// `worker-unit.sh`'s PATH, and the colony routes file.
    pub fn under_home(home: &Path) -> Self {
        let pinned = home.join(".local/share/claude/versions/2.1.312");
        let path = [".local/bin", "bin", ".cargo/bin"]
            .iter()
            .map(|dir| home.join(dir).display().to_string())
            .chain(["/usr/local/bin", "/usr/bin", "/bin"].map(str::to_owned))
            .collect::<Vec<_>>()
            .join(":");
        Self {
            state_dir: home.join(".local/state/spawn"),
            claude: if pinned.exists() {
                pinned
            } else {
                PathBuf::from("claude")
            },
            model: "claude-opus-5-5".to_owned(),
            codex: PathBuf::from("codex"),
            codex_model: None,
            codex_auth: Some(home.join(".codex/auth.json")),
            vibe: PathBuf::from("vibe"),
            mistral_model: "mistral-large-4".to_owned(),
            mistral_key_secret: "c2c7b31676a61315928103941b04d487".to_owned(),
            mistral_max_price: 2.0,
            secrets: "secrets".to_owned(),
            colony_env: Some(home.join(".config/triblespace/faculty-collections.env")),
            path,
            memory_max: "24G".to_owned(),
            spawn: "zooid".to_owned(),
            faculties: PathBuf::from("faculties"),
            local: None,
            local_max_runtime: None,
            codex_models: Some(home.join(".codex/models_cache.json")),
            claude_projects: Some(home.join(".claude/projects")),
        }
    }

    /// [`Self::under_home`] for `$HOME`, read when a frontend calls this,
    /// with the overrides [`Self::override_from`] reads from the environment,
    /// and the directory of the running executable first on the zooid's PATH
    /// ([`Self::own_build_first`]).
    pub fn from_home_env() -> Result<Self> {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        let mut config = Self::under_home(Path::new(&home));
        config.override_from(|name| std::env::var(name).ok())?;
        if let Some(dir) = std::env::current_exe()
            .ok()
            .as_deref()
            .and_then(Path::parent)
        {
            config.own_build_first(dir);
        }
        Ok(config)
    }

    /// Put `dir`, the directory of the program launching the zooid, first on
    /// its PATH, so its settlement, inbox hook and `faculties` MCP server come
    /// from the same build that started it, not from whatever the host has
    /// installed. A release generation copied to a host without activating it
    /// (for example one whose installed tools predate `zooid`) then works as
    /// it is.
    fn own_build_first(&mut self, dir: &Path) {
        let dir = dir.display().to_string();
        if !self
            .path
            .split(':')
            .next()
            .is_some_and(|first| first == dir)
        {
            self.path = format!("{dir}:{}", self.path);
        }
    }

    /// The codex model from `SPAWN_CODEX_MODEL`, the codex login file from
    /// `SPAWN_CODEX_AUTH`, the vibe command from `SPAWN_VIBE`, the Mistral
    /// key's Secrets version from `SPAWN_MISTRAL_KEY_SECRET`, a mistral
    /// run's price cap from `SPAWN_MISTRAL_MAX_PRICE`, the local executor
    /// from `SPAWN_LOCAL_EXECUTOR` and a local unit's runtime limit from
    /// `SPAWN_LOCAL_MAX_RUNTIME`, each when set and not blank. A price cap
    /// that is not a positive number, a malformed executor template and a
    /// runtime limit that is not a positive whole number of seconds are
    /// refused.
    fn override_from(&mut self, var: impl Fn(&str) -> Option<String>) -> Result<()> {
        let set = |name: &str| {
            var(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        self.codex_model = set("SPAWN_CODEX_MODEL");
        if let Some(auth) = set("SPAWN_CODEX_AUTH") {
            self.codex_auth = Some(PathBuf::from(auth));
        }
        if let Some(vibe) = set("SPAWN_VIBE") {
            self.vibe = PathBuf::from(vibe);
        }
        if let Some(secret) = set("SPAWN_MISTRAL_KEY_SECRET") {
            self.mistral_key_secret = secret;
        }
        if let Some(price) = set("SPAWN_MISTRAL_MAX_PRICE") {
            self.mistral_max_price = match price.parse::<f64>() {
                Ok(price) if price.is_finite() && price > 0.0 => price,
                _ => bail!("SPAWN_MISTRAL_MAX_PRICE {price:?} is not a positive number of dollars"),
            };
        }
        if let Some(template) = set("SPAWN_LOCAL_EXECUTOR") {
            self.local = Some(parse_local_executor(&template)?);
        }
        if let Some(seconds) = set("SPAWN_LOCAL_MAX_RUNTIME") {
            self.local_max_runtime = match seconds.parse::<u64>() {
                Ok(seconds) if seconds > 0 => Some(seconds),
                _ => bail!(
                    "SPAWN_LOCAL_MAX_RUNTIME {seconds:?} is not a positive whole number of seconds"
                ),
            };
        }
        Ok(())
    }

    /// The model a provider's zooid is told to use, or why it names none.
    fn model_for(&self, provider: Provider) -> (Option<String>, Option<String>) {
        match provider {
            Provider::Codex => match &self.codex_model {
                Some(model) => (Some(model.clone()), None),
                None => (
                    None,
                    Some(
                        "no codex model is configured (SPAWN_CODEX_MODEL), so codex chose its \
                         own default"
                            .to_owned(),
                    ),
                ),
            },
            Provider::Mistral => (Some(self.mistral_model.clone()), None),
            Provider::Local => (
                None,
                Some("the local executor (SPAWN_LOCAL_EXECUTOR) chooses its own model".to_owned()),
            ),
            _ => (Some(self.model.clone()), None),
        }
    }

    /// The absolute `faculties` binary: [`Self::faculties`] itself when
    /// absolute, else the first executable of that name on [`Self::path`].
    fn faculties_binary(&self) -> Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        let executable = |path: &Path| {
            std::fs::metadata(path)
                .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        };
        if self.faculties.is_absolute() {
            if executable(&self.faculties) {
                return Ok(self.faculties.clone());
            }
            bail!(
                "the faculties binary {} is not an executable file; a zooid would have no MCP \
                 tools. Nothing was started.",
                self.faculties.display()
            );
        }
        self.path
            .split(':')
            .filter(|dir| dir.starts_with('/'))
            .map(|dir| Path::new(dir).join(&self.faculties))
            .find(|candidate| executable(candidate))
            .ok_or_else(|| {
                anyhow!(
                    "no executable {} on the zooid's PATH ({}); a zooid would have no MCP tools. \
                     Nothing was started.",
                    self.faculties.display(),
                    self.path
                )
            })
    }

    fn file(&self, name: &str, suffix: &str) -> PathBuf {
        self.state_dir.join(format!("{name}.{suffix}"))
    }
}

/// The MCP server every zooid gets, as a claude `--mcp-config` file: its
/// unit script run as `mcp`, which execs `faculties mcp` in the
/// environment the script exports for the CLI.
pub fn claude_mcp_config(script: &Path) -> String {
    let config = serde_json::json!({
        "mcpServers": {
            "faculties": {
                "type": "stdio",
                "command": "/bin/bash",
                "args": [script.display().to_string(), "mcp"],
            }
        }
    });
    format!("{config}\n")
}

/// How long the hook gives `zooid inbox` before the agent stops waiting:
/// one cold pile read takes seconds, and a post hook may first wait for
/// another's turn on the run's inbox state.
pub const INBOX_STOP_TIMEOUT_SECS: u64 = 120;
pub const INBOX_POST_TIMEOUT_SECS: u64 = 60;

/// The hooks every claude zooid gets, as a `--settings` file: its unit
/// script run as `inbox stop` on Stop and `inbox post` after every tool call
/// (no matcher, so every tool). Both run inside the unit, in the CLI's
/// environment, and read the zooid's unread messages with `zooid inbox`.
pub fn claude_hook_settings(script: &Path) -> String {
    let hook = |kind: &str, timeout: u64| {
        serde_json::json!([{ "hooks": [{
            "type": "command",
            "command": format!("/bin/bash {} inbox {kind}", super::shell_word(&script.display().to_string())),
            "timeout": timeout,
        }]}])
    };
    let config = serde_json::json!({
        "hooks": {
            "Stop": hook("stop", INBOX_STOP_TIMEOUT_SECS),
            "PostToolUse": hook("post", INBOX_POST_TIMEOUT_SECS),
        }
    });
    format!("{config}\n")
}

/// The same server as codex `config.toml`, the whole of it: no model, hook
/// trust or other server. `script` is unit-safe (no quotes or backslashes),
/// so a basic TOML string holds it verbatim. codex's defaults would cut a
/// tool call at 60 s and a cold first call can take longer.
pub fn codex_mcp_config(script: &Path) -> String {
    format!(
        "# Written by `zooid` for this run. Nothing else is configured here.\n\
         [mcp_servers.faculties]\n\
         command = \"/bin/bash\"\n\
         args = [\"{}\", \"mcp\"]\n\
         startup_timeout_sec = 120\n\
         tool_timeout_sec = 3600\n",
        script.display()
    )
}

/// Mistral Large 4's list price, US dollars per million input tokens.
pub const MISTRAL_INPUT_PRICE: f64 = 0.70;
/// Mistral Large 4's list price, US dollars per million output tokens.
pub const MISTRAL_OUTPUT_PRICE: f64 = 2.10;

/// The vibe thinking level for a spawn effort. Large 4 accepts only low,
/// medium and high and answers 400 to `off`, so the higher efforts are high.
pub fn vibe_thinking(effort: &str) -> &'static str {
    match effort {
        "low" => "low",
        "medium" => "medium",
        _ => "high",
    }
}

/// A mistral run's vibe `config.toml`, the whole of it: one provider whose
/// key vibe reads from `MISTRAL_API_KEY`, one model at Large 4's list
/// prices, telemetry, update checks and subagents off, and one MCP server,
/// the unit script run as `mcp`. No cached-input price is configured:
/// Mistral publishes none for Large 4, and vibe then bills cached tokens at
/// the input price, so its cost estimate and the `--max-price` cap err high.
/// `script` and `model` are unit-safe, so basic TOML strings hold them.
pub fn vibe_config(script: &Path, model: &str, thinking: &str) -> String {
    format!(
        "# Written by `zooid` for this run. Nothing else is configured here.\n\
         active_model = \"{model}\"\n\
         enable_telemetry = false\n\
         enable_update_checks = false\n\
         enable_auto_update = false\n\
         enable_subagents = false\n\
         enable_notifications = false\n\
         include_commit_signature = false\n\
         \n\
         [[providers]]\n\
         name = \"mistral\"\n\
         api_base = \"https://api.mistral.ai/v1\"\n\
         api_key_env_var = \"MISTRAL_API_KEY\"\n\
         backend = \"mistral\"\n\
         \n\
         [[models]]\n\
         name = \"{model}\"\n\
         provider = \"mistral\"\n\
         alias = \"{model}\"\n\
         temperature = 1.0\n\
         input_price = {MISTRAL_INPUT_PRICE:?}\n\
         output_price = {MISTRAL_OUTPUT_PRICE:?}\n\
         thinking = \"{thinking}\"\n\
         thinking_levels = [\"low\", \"medium\", \"high\"]\n\
         max_context_length = {MISTRAL_CONTEXT_TOKENS}\n\
         \n\
         [[mcp_servers]]\n\
         name = \"faculties\"\n\
         transport = \"stdio\"\n\
         command = \"/bin/bash\"\n\
         args = [\"{}\", \"mcp\"]\n\
         startup_timeout_sec = 120\n\
         tool_timeout_sec = 3600\n",
        script.display()
    )
}

/// Make `home` a vibe home for one run: created if missing, whatever an
/// earlier run left there kept (its logs, its trusted folders), and
/// `config.toml` written fresh, so nothing from an ambient `~/.vibe` applies.
fn vibe_home_for_run(home: &Path, config: &str) -> Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    let settings = home.join("config.toml");
    std::fs::write(&settings, config).with_context(|| format!("write {}", settings.display()))
}

/// Make `home` a codex home holding nothing ambient: whatever an earlier
/// run of this zooid left there stays (its sessions and logs), the login is
/// a link to `auth`, so a token codex refreshes is the one every codex on
/// this host reads, and `config.toml` is `config`, written fresh, so no MCP
/// server, hook trust or model from the ambient `~/.codex/config.toml`
/// applies.
fn codex_home_for_run(home: &Path, auth: Option<&Path>, config: &str) -> Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    let settings = home.join("config.toml");
    std::fs::write(&settings, config).with_context(|| format!("write {}", settings.display()))?;
    let Some(auth) = auth else {
        return Ok(());
    };
    let link = home.join("auth.json");
    match std::fs::symlink_metadata(&link) {
        Ok(_) => {
            std::fs::remove_file(&link).with_context(|| format!("replace {}", link.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", link.display())),
    }
    std::os::unix::fs::symlink(auth, &link)
        .with_context(|| format!("link {} to {}", link.display(), auth.display()))
}

/// Which agent runs a zooid. The provider annotates the run; it is never
/// part of the brief or the name.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Provider {
    #[default]
    Claude,
    Codex,
    /// Mistral Large 4 through Mistral's `vibe` CLI. Opt-in only: Mistral
    /// bills money per token rather than drawing on a plan quota, so it is
    /// not in the quota table and `auto` never chooses it. A run is capped
    /// by [`LaunchConfig::mistral_max_price`].
    Mistral,
    /// Whichever of claude and codex the latest fresh quota readings show
    /// with the most headroom. Never mistral.
    Auto,
    /// A local model, through the host's configured executor
    /// ([`LaunchConfig::local`]). Refused on a host without one; never falls
    /// back to a hosted model.
    Local,
}

impl Provider {
    pub const NAMES: [&'static str; 5] = ["claude", "codex", "mistral", "auto", "local"];

    pub fn parse(text: &str) -> Result<Self> {
        match text.trim().to_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "mistral" => Ok(Self::Mistral),
            "auto" => Ok(Self::Auto),
            "local" => Ok(Self::Local),
            other => bail!(
                "provider {other:?} is not one of {}",
                Self::NAMES.join(", ")
            ),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Mistral => "mistral",
            Self::Auto => "auto",
            Self::Local => "local",
        }
    }
}

/// The provider `auto` picks from one quota reading: the one with the most
/// headroom left in its tightest fresh window. Only claude and codex are
/// candidates: they draw on plan quotas, and mistral, which bills money, is
/// only ever run when it is named. A reading older than the quota
/// faculty's stale age, or whose window has already reset, says nothing about
/// now: such a provider is unknown, and unknown is never spare. Returns the
/// provider and the readings that decided it.
pub fn choose_provider(view: &crate::quota::operations::QuotaView) -> Result<(Provider, String)> {
    use crate::schemas::quota::{PROVIDER_CLAUDE, PROVIDER_CODEX};
    let stale_after = i128::from(crate::quota::STALE_AFTER_SECONDS) * 1_000_000_000;
    let mut known = Vec::new();
    let mut said = Vec::new();
    for (provider, id) in [
        (Provider::Claude, PROVIDER_CLAUDE),
        (Provider::Codex, PROVIDER_CODEX),
    ] {
        let tightest = view
            .groups
            .iter()
            .filter(|group| group.provider == id)
            .filter(|group| view.now - group.observed <= stale_after)
            .filter(|group| group.resets_at.iter().any(|at| *at > view.now))
            .filter_map(|group| {
                let used = group.used_percent.iter().copied().fold(f64::NAN, f64::max);
                (!used.is_nan()).then_some((100.0 - used, group))
            })
            .min_by(|(a, _), (b, _)| a.total_cmp(b));
        match tightest {
            Some((headroom, group)) => {
                said.push(format!(
                    "{} {headroom:.1}% left (reading {:x}, {} s window{})",
                    provider.name(),
                    group.event,
                    group.window_secs,
                    group
                        .bucket
                        .as_deref()
                        .map(|bucket| format!(", bucket {bucket}"))
                        .unwrap_or_default()
                ));
                known.push((headroom, provider));
            }
            None => said.push(format!("{} unknown (no fresh reading)", provider.name())),
        }
    }
    let readings = said.join("; ");
    match known
        .into_iter()
        .filter(|(headroom, _)| *headroom > 0.0)
        .max_by(|(a, _), (b, _)| a.total_cmp(b))
    {
        Some((_, provider)) => Ok((provider, format!("auto: {readings}"))),
        None => bail!(
            "--provider auto found no provider with known headroom ({readings}); name one with \
             --provider claude or --provider codex"
        ),
    }
}

#[derive(Clone, Debug)]
pub struct RunRequest {
    /// The brief, verbatim.
    pub brief: String,
    pub workdir: PathBuf,
    pub effort: String,
    /// The spawning persona: label, alias or id.
    pub spawner: String,
    pub provider: Provider,
    pub cover: CoverWish,
}

/// What a launch asks of its cover's size: `percent` of the worker's
/// context window for the whole cover, a best-effort wish, and the window
/// itself when the caller knows it. The default asks nothing: the plain
/// cover of [`PLAIN_MEMORY_CHARS`] memory characters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CoverWish {
    pub percent: Option<f64>,
    /// The window in tokens (`--window`), before any other source.
    pub window: Option<u64>,
}

/// Launch a zooid whose brief is rendered from a goal.
#[derive(Clone, Debug)]
pub struct GoalRequest {
    /// The goal: an id or a unique id prefix.
    pub goal: String,
    /// The spawner's own instructions, appended to the rendered brief.
    pub extra: Option<String>,
    pub workdir: PathBuf,
    pub effort: String,
    pub spawner: String,
    pub provider: Provider,
    pub cover: CoverWish,
}

/// What one launch runs from: the brief text, the memory tail's topic, and
/// for `zooid goal`, the goal revision it was rendered from.
struct Brief {
    text: String,
    about: String,
    goal: Option<(Id, Vec<Id>)>,
}

#[derive(Clone, Debug)]
pub struct Launched {
    pub name: String,
    pub spawn: Id,
    pub run: Id,
    pub persona: Id,
    pub brief: TextHandle,
    pub unit: String,
    pub report: PathBuf,
    pub log: PathBuf,
    pub prompt_chars: usize,
    pub provider: Provider,
    pub provider_reason: String,
    /// The model the agent was told to use, or why it names none.
    pub model: Option<String>,
    pub model_unset: Option<String>,
    /// Why the recollection has no topic, if reported.
    pub untinted: Option<String>,
    /// Why this launch used the lexical cue after semantic scoring failed.
    pub lexical_fallback: Option<String>,
    /// The archived node a local run continues its zooid's history from.
    pub resumes: Option<Id>,
    /// How the cover was sized, when the launch asked for a share
    /// (`--cover`).
    pub cover: Option<CoverRecord>,
    /// Why a newly named zooid's name has no colony prefix (one line).
    pub unprefixed: Option<String>,
    /// The goal note the zooid added at launch (`zooid goal`).
    pub goal_note: Option<Id>,
    /// What went wrong after the unit had started. The zooid runs anyway.
    pub warnings: Vec<String>,
}

/// The files of one run that an agent's command line names.
struct RunFiles {
    /// The zooid's name.
    name: String,
    workdir: PathBuf,
    /// The unit script; run as `mcp` it is the zooid's MCP server.
    script: PathBuf,
    prompt: PathBuf,
    report: PathBuf,
    mcp_config: PathBuf,
    settings: PathBuf,
    codex_home: PathBuf,
    vibe_home: PathBuf,
    vibe_out: PathBuf,
    /// The archived node a local run continues from, for `{resume}`.
    resume: Option<Id>,
    /// A local run's first input without the wake, for `{brief}`.
    brief: PathBuf,
    /// The spawning persona's label, for `{spawner}`.
    spawner: String,
}

/// An agent command line and what the unit gives it.
struct AgentLine {
    argv: Vec<String>,
    /// Exported for the agent only.
    env: Vec<(String, String)>,
    /// Variable and Secrets version, read inside the unit for the agent only.
    secrets: Vec<(String, String)>,
    stdout_is_report: bool,
}

#[derive(Clone, Debug)]
pub struct SettleRequest {
    pub name: String,
    pub run: Id,
    pub exit: Exit,
    /// The run's report file: a hosted run's final reply, or a local run's
    /// closing message. A missing or empty file records none; other bytes
    /// are recorded as they are.
    pub report: Option<PathBuf>,
    /// vibe's `export.json`, for a mistral run: its usage and cost are
    /// recorded with the end. A file that is missing or unreadable is a
    /// settlement failure, recorded on the run.
    pub usage: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Settled {
    pub run: Id,
    pub report: Option<TextHandle>,
    pub message: Option<Id>,
    /// The end was already recorded by an earlier settlement.
    pub already_ended: bool,
    /// The spawner was already messaged by an earlier settlement.
    pub already_messaged: bool,
}

/// One spawn as this observation holds it.
#[derive(Clone, Debug)]
pub struct SpawnView {
    pub id: Id,
    /// The zooid's name. When hosts named it apart, every stored name,
    /// joined: see `names`.
    pub name: String,
    /// Every stored name, sorted. Normally one; several are a conflict that
    /// `zooid list` and `zooid show` surface and nothing resolves.
    pub names: Vec<String>,
    /// The colony the name was taken from, when one was recorded.
    pub origin_colony: Vec<Id>,
    pub brief: TextHandle,
    pub brief_text: Option<String>,
    /// The goal the brief was rendered from, and that revision.
    pub goal: Vec<Id>,
    pub goal_revision: Vec<Id>,
    /// What it declares about where and how it lives.
    pub declared: super::declaration::Declared,
    /// Oldest first.
    pub runs: Vec<RunView>,
    /// What the removed host queue recorded about this zooid, oldest first.
    /// History: nothing answers these.
    pub requests: Vec<RequestView>,
}

/// One run. The fields after `started` are annotations, so each is a list:
/// normally one value, none when it was never recorded, several shown as is.
#[derive(Clone, Debug)]
pub struct RunView {
    pub id: Id,
    pub started: Option<IntervalValue>,
    pub finished: Vec<IntervalValue>,
    pub exit_code: Vec<u64>,
    pub result: Vec<String>,
    pub signal: Vec<String>,
    pub unit: Vec<String>,
    pub workdir: Vec<String>,
    pub effort: Vec<String>,
    pub persona: Vec<Id>,
    pub spawner: Vec<Id>,
    pub prompt: Vec<TextHandle>,
    pub report: Vec<TextHandle>,
    /// The first report's text, when it is here.
    pub report_text: Option<String>,
    pub provider: Vec<String>,
    pub provider_reason: Vec<String>,
    pub model: Vec<String>,
    pub model_unset: Vec<String>,
    pub untinted: Vec<String>,
    pub lexical_fallback: Vec<String>,
    /// The archived node(s) a local run continued its zooid's history from.
    pub resumes: Vec<Id>,
    /// The declaration(s) the run made true.
    pub realises: Vec<Id>,
    pub message: Vec<Id>,
    pub settle_failure: Vec<String>,
    pub goal_note: Vec<Id>,
    /// The past request the run served.
    pub request: Vec<Id>,
    /// Usage the agent reported, recorded at settlement (mistral runs).
    pub input_tokens: Vec<u64>,
    pub cached_input_tokens: Vec<u64>,
    pub output_tokens: Vec<u64>,
    /// The agent's own cost estimate in US dollars, not the provider's bill.
    pub cost_usd: Vec<f64>,
    /// How the cover was sized, in words (`--cover` launches).
    pub cover_sizing: Vec<String>,
    /// The window the provider reported at settlement.
    pub context_window: Vec<u64>,
    /// The first model call's input tokens as the provider reported them.
    pub first_call_input_tokens: Vec<u64>,
    /// The conversation the provider recorded, as the archive locates it
    /// (`session/<id>`); `archive thread` reads it once imported.
    pub source_locator: Vec<String>,
}

impl RunView {
    pub fn ended(&self) -> bool {
        !self.finished.is_empty()
    }
}

/// One outcome recorded for a past request.
#[derive(Clone, Debug)]
pub struct OutcomeView {
    pub id: Id,
    pub kind: Id,
    pub at: Vec<IntervalValue>,
    pub run: Vec<Id>,
    pub reason: Vec<String>,
}

/// One past request as this observation holds it. Fields that are lists may
/// hold none or several values, all shown.
#[derive(Clone, Debug)]
pub struct RequestView {
    pub id: Id,
    pub host: Vec<HostKey>,
    pub host_label: Vec<String>,
    pub provider: Vec<String>,
    pub effort: Vec<String>,
    pub previous: Vec<Id>,
    pub handoff: Vec<String>,
    pub cover_percent: Vec<f64>,
    pub spawner: Vec<Id>,
    pub requested_at: Vec<IntervalValue>,
    /// Earliest first.
    pub outcomes: Vec<OutcomeView>,
    /// Runs that served it, and whether each has a recorded end.
    pub runs: Vec<(Id, bool)>,
}

/// The name a launch gives a zooid.
#[derive(Clone, Debug)]
struct Naming {
    name: String,
    /// The colony the name was taken from, recorded with a new name.
    origin: Option<Id>,
    /// Whether the name is new and has to be stored.
    fresh: bool,
    /// Why a new name has no colony prefix.
    unprefixed: Option<String>,
}

/// The stored names of one spawn, sorted, and the name handles whose text
/// has not arrived. Absence is not knowledge: a reader that needs the name
/// waits for the missing text rather than acting without it.
pub(super) fn stored_names<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    spawn: Id,
) -> (Vec<String>, Vec<TextHandle>) {
    let mut names = Vec::new();
    let mut missing = Vec::new();
    for handle in find!(
        handle: TextHandle,
        pattern!(facts, [{ spawn @ attrs::name: ?handle }])
    ) {
        match read_text(reader, handle) {
            Some(text) => names.push(text),
            None => missing.push(handle),
        }
    }
    names.sort();
    names.dedup();
    (names, missing)
}

/// [`stored_names`] for display: a missing text is shown as its handle.
fn shown_names<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    spawn: Id,
) -> Vec<String> {
    let (mut names, missing) = stored_names(facts, reader, spawn);
    names.extend(missing.into_iter().map(super::handle_text));
    names.sort();
    names
}

/// The least time between two inbox reads by one run's post hook.
pub const INBOX_POST_INTERVAL_SECS: u64 = 60;

/// The longest message body an inbox hook shows; a longer one is cut, and
/// says where the whole of it is.
pub const INBOX_BODY_CHARS: usize = 4000;

/// Which claude hook `zooid inbox` answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxHook {
    /// Stop: the zooid is about to end its run.
    Stop,
    /// PostToolUse: a tool call just finished.
    Post,
}

impl InboxHook {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "stop" => Ok(Self::Stop),
            "post" => Ok(Self::Post),
            other => bail!("hook {other:?} is not stop or post"),
        }
    }
}

/// One hook call: whose inbox, which run, which hook, and what the agent
/// said about it.
#[derive(Clone, Debug)]
pub struct InboxRequest {
    pub name: String,
    pub run: Id,
    pub hook: InboxHook,
    /// The Stop input's `stop_hook_active`: this stop follows a Stop block.
    pub stop_hook_active: bool,
    /// Seconds since the Unix epoch.
    pub now: u64,
}

/// What a hook prints and logs. No output means "carry on": the agent
/// treats an empty stdout with exit 0 as no decision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InboxAnswer {
    /// The hook's JSON, printed on stdout.
    pub output: Option<serde_json::Value>,
    /// Written to stderr, which the unit script appends to the run's log.
    pub warning: Option<String>,
}

/// The answer when the inbox could not be read. A stop says so and blocks
/// the end once, since the zooid may be ending without a message meant to
/// change its task; a post logs it and lets the tool result through.
pub fn inbox_failure(hook: InboxHook, error: &anyhow::Error) -> InboxAnswer {
    let warning = Some(format!(
        "zooid inbox: the inbox could not be read: {error:#}"
    ));
    match hook {
        InboxHook::Stop => InboxAnswer {
            output: Some(serde_json::json!({
                "decision": "block",
                "reason": format!(
                    "Your inbox could not be read before you finish: {error:#}. Messages sent \
                     to you while you worked may be unread. Check with `orient_poll` (peek true) \
                     if you can, and say in your report that the inbox could not be read. This \
                     hook stops you only once: your next reply ends the run and becomes your \
                     report, so give the whole report again."
                ),
            })),
            warning,
        },
        InboxHook::Post => InboxAnswer {
            output: None,
            warning,
        },
    }
}

/// A run's post-hook state: the run it belongs to, when its inbox was last
/// read, and the messages already shown.
#[derive(Clone, Debug, Default, PartialEq)]
struct InboxState {
    run: Option<Id>,
    checked_at: Option<u64>,
    shown: Vec<Id>,
}

impl InboxState {
    fn parse(text: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        let id = |text: &str| Id::from_hex(text).ok_or_else(|| anyhow!("invalid id {text:?}"));
        Ok(Self {
            run: value["run"].as_str().map(id).transpose()?,
            checked_at: value["checked_at"].as_u64(),
            shown: value["shown"]
                .as_array()
                .map(|ids| {
                    ids.iter()
                        .map(|text| {
                            text.as_str()
                                .ok_or_else(|| anyhow!("shown id is not a string"))
                                .and_then(id)
                        })
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default(),
        })
    }

    fn render(&self) -> String {
        let value = serde_json::json!({
            "run": self.run.map(|run| format!("{run:x}")),
            "checked_at": self.checked_at,
            "shown": self.shown.iter().map(|id| format!("{id:x}")).collect::<Vec<_>>(),
        });
        format!("{value}\n")
    }
}

/// Each message as the hooks show it: id, sender and body.
fn inbox_listing(messages: &[crate::message::MessageObservation]) -> String {
    let mut text = String::new();
    for entry in messages {
        let body = match &entry.body {
            crate::message::MessageText::Text(body) => {
                let mut chars = body.chars();
                let cut: String = chars.by_ref().take(INBOX_BODY_CHARS).collect();
                if chars.next().is_some() {
                    format!(
                        "{cut}\n[cut at {INBOX_BODY_CHARS} characters; `orient_poll` shows the whole message]"
                    )
                } else {
                    cut
                }
            }
            crate::message::MessageText::Unavailable(handle) => format!(
                "[its text {} is not here yet; `orient_poll` may show it later]",
                super::handle_text(*handle)
            ),
            crate::message::MessageText::Undecodable(handle) => {
                format!("[its text {} is not UTF-8]", super::handle_text(*handle))
            }
        };
        let _ = write!(
            text,
            "\n- message {:x} from {}:\n{}\n",
            entry.id, entry.from_label, body
        );
    }
    text
}

fn stop_reason(name: &str, unread: &[crate::message::MessageObservation]) -> String {
    format!(
        "Before you finish: {} message(s) sent to you ({name}) are unread. They may change your \
         task or its boundaries.\n{}\nHandle each: answer with `message_send` from {name} if it \
         asks something, and `message_ack` it once handled. This hook stops you only once: your \
         next reply ends the run and becomes your report, so give the whole report again, \
         including what these messages changed.",
        unread.len(),
        inbox_listing(unread)
    )
}

fn post_context(name: &str, new: &[crate::message::MessageObservation]) -> String {
    format!(
        "New message(s) for {name} arrived while you work. They are shown once and are not \
         acknowledged; handle them before your next step if they bear on it, and `message_ack` \
         each once handled.\n{}",
        inbox_listing(new)
    )
}

/// Configured Spawn operations.
#[derive(Clone)]
pub struct Spawn {
    storage: Storage,
    launch: LaunchConfig,
    launcher: Arc<dyn Launcher>,
    renderer: Arc<dyn Renderer>,
}

impl Spawn {
    pub fn new(storage: Storage, launch: LaunchConfig) -> Self {
        Self {
            storage,
            launch,
            launcher: Arc::new(SystemdLauncher),
            renderer: Arc::new(LibraryRenderer::default()),
        }
    }

    /// The storage every operation reads and writes through.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn with_launcher(mut self, launcher: Arc<dyn Launcher>) -> Self {
        self.launcher = launcher;
        self
    }

    pub fn with_renderer(mut self, renderer: Arc<dyn Renderer>) -> Self {
        self.renderer = renderer;
        self
    }

    /// Launch the zooid a brief names. Refuses while its persona is live.
    pub fn run(&self, request: &RunRequest) -> Result<Launched> {
        let brief = Brief {
            about: super::about(&request.brief).to_owned(),
            text: request.brief.clone(),
            goal: None,
        };
        self.launch(
            brief,
            &request.workdir,
            &request.effort,
            &request.spawner,
            request.provider,
            request.cover,
        )
    }

    /// Launch the zooid for a goal's current revision. The brief is
    /// rendered from the goal ([`super::goal_brief`]); the spawn records the
    /// goal and the revision, and the zooid notes its launch on the goal. The
    /// goal's status is the zooid's to move: nothing here touches it.
    pub fn goal(&self, request: &GoalRequest) -> Result<Launched> {
        let reading = Goals::with_storage(self.storage.clone())
            .reading(&request.goal)
            .with_context(|| format!("read goal {}", request.goal))?;
        let rendered = super::goal_brief(&reading, request.extra.as_deref());
        let brief = Brief {
            text: rendered.text,
            about: rendered.about,
            goal: Some((reading.goal, rendered.revision)),
        };
        self.launch(
            brief,
            &request.workdir,
            &request.effort,
            &request.spawner,
            request.provider,
            request.cover,
        )
    }

    /// Every collection route this launch resolves, as the environment
    /// variable and value the unit exports.
    fn pinned_routes(&self) -> Result<Vec<(String, String)>> {
        let mut routes = Vec::new();
        for (scope, _) in crate::collection_names::table() {
            let handle = match self.storage.collection_handle(scope) {
                Ok(handle) => handle,
                // A workspace leaves roles it does not use unconfigured.
                Err(_) if self.storage.collection_routes().is_some() => None,
                Err(error) => return Err(error),
            };
            if let Some(handle) = handle {
                routes.push((
                    crate::collection_names::override_env_name(scope),
                    format!("blake3:{}", hex::encode(handle.raw)),
                ));
            }
        }
        Ok(routes)
    }

    /// The concrete provider and why, before anything is touched.
    fn resolve_provider(&self, provider: Provider) -> Result<(Provider, String)> {
        match provider {
            Provider::Claude | Provider::Codex | Provider::Mistral => {
                Ok((provider, format!("{} was asked for", provider.name())))
            }
            Provider::Local => match &self.launch.local {
                Some(template)
                    if names_runtime(template) && self.launch.local_max_runtime.is_none() =>
                {
                    bail!("{RUNTIME_UNSET}.")
                }
                Some(_) => Ok((provider, "local was asked for".to_owned())),
                None => bail!("{LOCAL_UNCONFIGURED}. Nothing was started."),
            },
            Provider::Auto => {
                let view = crate::quota::operations::Quota::with_storage(self.storage.clone())
                    .show()
                    .context("--provider auto reads the quota faculty")?;
                choose_provider(&view)
            }
        }
    }

    /// The prompt, and how its cover was sized when the launch asked for a
    /// share of the context window. Without one the wake is the plain cover
    /// and nothing is looked up. With one, the target is `percent` of `W`;
    /// fixed text is measured by a wake with no journal budget. The journal
    /// gets the remaining target, initially at least
    /// [`cover::MIN_MEMORY_CHARS`], and a task's Knowledge readings their own
    /// allowance beside it. If the whole prompt is then
    /// estimated above `W` less the headroom, the memory part is rendered
    /// once more aimed at that line, possibly with no memory at all, and the
    /// second render replaces the first. Every figure is an estimate, the
    /// line is an aim and not an observed limit, and nothing is refused.
    fn size_cover(
        &self,
        provider: Provider,
        model: Option<&str>,
        tint: Option<&WakeTint>,
        wish: CoverWish,
        assemble: &dyn Fn(&str) -> String,
    ) -> Result<(String, Option<CoverRecord>)> {
        let wake = |memory_chars: usize| self.renderer.wake(&self.storage, tint, memory_chars);
        let Some(percent) = wish.percent else {
            return Ok((assemble(&wake(PLAIN_MEMORY_CHARS)?), None));
        };
        let window = match self.window(provider, model, wish.window) {
            Ok(window) => window,
            Err(why) => {
                let delivered = wake(PLAIN_MEMORY_CHARS)?;
                let summary = format!(
                    "cover {percent}%: no context window is known for {} {} ({why}), so it was \
                     rendered at the plain size, {PLAIN_MEMORY_CHARS} journal characters; \
                     --window N gives one",
                    provider.name(),
                    model.unwrap_or("(no model named)")
                );
                return Ok((
                    assemble(&delivered),
                    Some(CoverRecord {
                        percent,
                        window: None,
                        window_source: why,
                        calibration: None,
                        target_chars: None,
                        fixed_chars: None,
                        memory_budget: PLAIN_MEMORY_CHARS,
                        cover_chars: delivered.chars().count(),
                        estimate: None,
                        headroom: None,
                        reduction: None,
                        summary,
                    }),
                ));
            }
        };
        let calibration = cover::calibration(provider.name(), model);
        let ratio = calibration.chars_per_token;
        let (overhead, overhead_basis) = cover::overhead(provider.name());
        let target = cover::target_chars(percent, window.tokens, ratio);
        let fixed = wake(0)?.chars().count();
        let mut budget = cover::memory_budget(target, fixed);
        let mut delivered = wake(budget)?;
        let mut prompt = assemble(&delivered);
        let mut estimate = cover::estimate_tokens(prompt.chars().count(), ratio, overhead);
        let aim = cover::aim_tokens(window.tokens);
        let mut reduction = None;
        if estimate > aim {
            let rendered = delivered.chars().count().saturating_sub(fixed);
            let smaller = cover::reduced_budget(rendered, estimate, aim, ratio);
            // What was rendered over the fixed sections is journal and
            // readings together; a task's wake has both.
            let smaller = match tint {
                Some(_) => cover::journal_budget(smaller),
                None => smaller,
            };
            let again = wake(smaller)?;
            let again_prompt = assemble(&again);
            let again_estimate =
                cover::estimate_tokens(again_prompt.chars().count(), ratio, overhead);
            let mut text = format!(
                "the prompt was estimated at {estimate} tokens, above {aim} (W {} less {}% \
                 headroom for the work), so the memory part was rendered once more: budget \
                 {budget} -> {smaller} characters, estimate {again_estimate} tokens",
                window.tokens,
                cover::HEADROOM * 100.0
            );
            if again_estimate > window.tokens {
                text.push_str(&format!(
                    "; still estimated above the whole window of {} tokens{}, launched anyway: \
                     an estimate, not a refusal",
                    window.tokens,
                    if smaller == 0 {
                        " even with no memories"
                    } else {
                        ""
                    }
                ));
            }
            reduction = Some(text);
            budget = smaller;
            delivered = again;
            prompt = again_prompt;
            estimate = again_estimate;
        }
        let cover_chars = delivered.chars().count();
        let readings = match tint {
            Some(_) => format!(
                " and up to {} for Knowledge readings",
                crate::knowledge::recall::readings_allowance(budget)
            ),
            None => String::new(),
        };
        let mut summary = format!(
            "cover {percent}% of a {} token window ({}): target {target} characters at {ratio} \
             characters per token ({}); fixed wake {fixed} characters, journal budget \
             {budget}{readings}, cover {cover_chars} characters{}; whole prompt estimated at {estimate} \
             tokens with {overhead} tokens of harness overhead ({overhead_basis})",
            window.tokens,
            window.source,
            calibration.basis,
            if cover_chars > target {
                ", over its target (fixed sections render whole)"
            } else {
                ""
            }
        );
        if let Some(reduction) = &reduction {
            summary.push_str("; ");
            summary.push_str(reduction);
        }
        Ok((
            prompt,
            Some(CoverRecord {
                percent,
                window: Some(window.tokens),
                window_source: window.source,
                calibration: Some(calibration),
                target_chars: Some(target),
                fixed_chars: Some(fixed),
                memory_budget: budget,
                cover_chars,
                estimate: Some(estimate),
                headroom: Some(cover::HEADROOM),
                reduction,
                summary,
            }),
        ))
    }

    /// The worker model's context window, from the first source that gives
    /// one: `--window`; the provider's own configuration for the model
    /// (codex's catalog, the window spawn writes for mistral); a value the
    /// provider reported at the end of an earlier run of the same provider
    /// and model, the latest. A local run takes `--window` only. `Err` says
    /// why no source gave one; nothing invents a window.
    fn window(
        &self,
        provider: Provider,
        model: Option<&str>,
        flag: Option<u64>,
    ) -> std::result::Result<cover::Window, String> {
        if let Some(tokens) = flag {
            return Ok(cover::Window {
                tokens,
                source: "--window".to_owned(),
            });
        }
        let mut why = Vec::new();
        match provider {
            Provider::Local => {
                return Err("a local run takes its window from --window only".to_owned());
            }
            Provider::Mistral => {
                return Ok(cover::Window {
                    tokens: MISTRAL_CONTEXT_TOKENS,
                    source: "the max_context_length spawn writes into the run's vibe \
                             configuration"
                        .to_owned(),
                });
            }
            Provider::Codex => match (model, &self.launch.codex_models) {
                (Some(model), Some(path)) => match cover::codex_catalog_window(path, model) {
                    Ok(window) => return Ok(window),
                    Err(error) => why.push(error),
                },
                (None, _) => why.push("no codex model is named (SPAWN_CODEX_MODEL)".to_owned()),
                (Some(_), None) => why.push("no codex model catalog is configured".to_owned()),
            },
            _ => {}
        }
        let Some(model) = model else {
            why.push("no model is named to look up a recorded window for".to_owned());
            return Err(why.join("; "));
        };
        match self.recorded_window(provider.name(), model) {
            Ok(Some(window)) => Ok(window),
            Ok(None) => {
                why.push(format!(
                    "no settled {} run of {model} has recorded one",
                    provider.name()
                ));
                Err(why.join("; "))
            }
            Err(error) => {
                why.push(format!("reading recorded windows failed: {error:#}"));
                Err(why.join("; "))
            }
        }
    }

    /// The latest context window a settled run of this provider and model
    /// recorded, with that run and its end. Its age refuses nothing.
    fn recorded_window(&self, provider: &str, model: &str) -> Result<Option<cover::Window>> {
        self.observe(|facts, _| {
            let latest = find!(
                (run: Id, window: Inline<inlineencodings::U256BE>, at: IntervalValue),
                pattern!(facts, [{ ?run @
                    attrs::provider: provider,
                    attrs::model: model,
                    attrs::context_window: ?window,
                    metadata::finished_at: ?at,
                }])
            )
            .filter_map(|(run, window, at)| {
                let window = window.try_from_inline::<u64>().ok()?;
                let (from, _) = at.try_from_inline::<(Epoch, Epoch)>().ok()?;
                Some((from, run, window, at))
            })
            .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            Ok(latest.map(|(_, run, tokens, at)| cover::Window {
                tokens,
                source: format!(
                    "reported by {provider} for {model} at the end of run {run:x}, {}",
                    crate::care::format_time(at)
                ),
            }))
        })
    }

    /// The agent command line for a provider, the environment it runs in,
    /// and whether its stdout is the report. Every concrete provider has its
    /// own arm; `auto` is refused, and `local` without an executor, never
    /// sent to claude.
    fn agent(&self, provider: Provider, effort: &str, files: &RunFiles) -> Result<AgentLine> {
        let text = |path: &Path| path.display().to_string();
        Ok(match provider {
            Provider::Codex => {
                // codex has no `max`; `xhigh` is its highest.
                let effort = if effort == "max" { "xhigh" } else { effort };
                let mut argv = vec![
                    text(&self.launch.codex),
                    "exec".to_owned(),
                    "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                    "--skip-git-repo-check".to_owned(),
                ];
                if let Some(model) = &self.launch.codex_model {
                    argv.extend(["-m".to_owned(), model.clone()]);
                }
                argv.extend([
                    "-c".to_owned(),
                    format!("model_reasoning_effort={effort}"),
                    "-o".to_owned(),
                    text(&files.report),
                    "-".to_owned(),
                ]);
                AgentLine {
                    argv,
                    env: vec![("CODEX_HOME".to_owned(), text(&files.codex_home))],
                    secrets: Vec::new(),
                    stdout_is_report: false,
                }
            }
            Provider::Mistral => AgentLine {
                // The prompt is read from its file and the model's last reply
                // is stdout; vibe leaves usage and cost in export.json under
                // the output directory on every exit, for settlement.
                argv: vec![
                    text(&self.launch.vibe),
                    "--workdir".to_owned(),
                    text(&files.workdir),
                    "--trust".to_owned(),
                    "--auto-approve".to_owned(),
                    "--model".to_owned(),
                    self.launch.mistral_model.clone(),
                    "--max-price".to_owned(),
                    format!("{}", self.launch.mistral_max_price),
                    "--output".to_owned(),
                    "text".to_owned(),
                    "--output-dir".to_owned(),
                    text(&files.vibe_out),
                    "--prompt-file".to_owned(),
                    text(&files.prompt),
                ],
                env: vec![("VIBE_HOME".to_owned(), text(&files.vibe_home))],
                // Known limit: vibe's own bash tool inherits this variable,
                // so a zooid's shell can read the key, as a codex zooid can
                // read its linked auth.json. Settlement and the MCP server
                // never hold it (the script unsets it for every branch).
                secrets: vec![(
                    "MISTRAL_API_KEY".to_owned(),
                    self.launch.mistral_key_secret.clone(),
                )],
                stdout_is_report: true,
            },
            Provider::Claude => AgentLine {
                argv: vec![
                    text(&self.launch.claude),
                    "-p".to_owned(),
                    "--model".to_owned(),
                    self.launch.model.clone(),
                    "--effort".to_owned(),
                    effort.to_owned(),
                    "--dangerously-skip-permissions".to_owned(),
                    "--strict-mcp-config".to_owned(),
                    "--mcp-config".to_owned(),
                    text(&files.mcp_config),
                    "--settings".to_owned(),
                    text(&files.settings),
                    // One result object on stdout: settlement records its
                    // `result` text as the report, as the text format's
                    // stdout was, and the window the CLI reports.
                    "--output-format".to_owned(),
                    "json".to_owned(),
                ],
                env: Vec::new(),
                secrets: Vec::new(),
                stdout_is_report: true,
            },
            Provider::Local => {
                let Some(template) = &self.launch.local else {
                    bail!("{LOCAL_UNCONFIGURED}");
                };
                let runtime = match self.launch.local_max_runtime {
                    Some(seconds) => seconds.to_string(),
                    None if names_runtime(template) => bail!("{RUNTIME_UNSET}"),
                    None => String::new(),
                };
                let values = [
                    ("{prompt}", text(&files.prompt)),
                    // The run's report file: settlement reads a local
                    // run's closing message from the same file it reads a
                    // hosted run's report from.
                    ("{message}", text(&files.report)),
                    ("{name}", files.name.clone()),
                    ("{effort}", effort.to_owned()),
                    ("{script}", text(&files.script)),
                    ("{runtime}", runtime),
                    (
                        "{resume}",
                        files
                            .resume
                            .map(|node| format!("{node:x}"))
                            .unwrap_or_default(),
                    ),
                    ("{brief}", text(&files.brief)),
                    ("{spawner}", files.spawner.clone()),
                ];
                let argv = template
                    .iter()
                    .map(|word| {
                        values
                            .iter()
                            .fold(word.clone(), |word, (key, value)| word.replace(key, value))
                    })
                    .collect();
                // The executor may write the message file; its stdout goes to
                // the unit's log. No file, or an empty one, is no message.
                AgentLine {
                    argv,
                    env: Vec::new(),
                    secrets: Vec::new(),
                    stdout_is_report: false,
                }
            }
            Provider::Auto => bail!(
                "{} is a policy, not an agent: it is resolved before a command line is built",
                provider.name()
            ),
        })
    }

    /// The name for the zooid `spawn`. A zooid that has a stored name keeps
    /// it: a relaunch, a move or a group rename never renames it. A new one
    /// is named after the unique colony its spawner belongs to, or failing
    /// that the unique origin colony of the zooids the spawner ran as (task
    /// zooids are not colony members); otherwise it gets no prefix and the
    /// reason. Two stored names, from hosts that named it apart, refuse: the
    /// launch would have to pick one.
    fn naming(&self, spawn: Id, brief: TextHandle, spawner: Id) -> Result<Naming> {
        let (names, inherited) = self.observe(|facts, reader| {
            let (names, missing) = stored_names(facts, reader, spawn);
            if !missing.is_empty() {
                bail!(
                    "zooid {spawn:x} has a name whose text is not here yet ({}); try again \
                     once it has synced",
                    super::handle_text(missing[0])
                );
            }
            let inherited: std::collections::BTreeSet<Id> = find!(
                origin: Id,
                pattern!(facts, [
                    { _?run @ metadata::tag: KIND_RUN, attrs::of: _?parent, attrs::persona: spawner },
                    { _?parent @ attrs::origin_colony: ?origin }
                ])
            )
            .collect();
            Ok((names, inherited))
        })?;
        match names.as_slice() {
            [one] => {
                return Ok(Naming {
                    name: one.clone(),
                    origin: None,
                    fresh: false,
                    unprefixed: None,
                });
            }
            [] => {}
            several => bail!(
                "zooid {spawn:x} has {} names, from hosts that named it apart: {}. \
                 Nothing picks one; run it by its brief once the names are settled",
                several.len(),
                several.join(", ")
            ),
        }
        let relations = Relations::with_storage(self.storage.clone());
        let membership = relations.colonies_of(spawner)?;
        let unprefixed = |reason: String| Naming {
            name: super::name_for(brief),
            origin: None,
            fresh: true,
            unprefixed: Some(reason),
        };
        let (origin, label) = match membership.colonies.as_slice() {
            [(colony, label)] => (*colony, label.clone()),
            [] => match inherited.len() {
                1 => {
                    let colony = *inherited.iter().next().expect("one origin");
                    (colony, relations.colony_label(colony)?)
                }
                0 => {
                    return Ok(unprefixed(
                        "no colony prefix: the spawner is in no colony group, and no zooid it \
                         ran as has an origin colony"
                            .to_owned(),
                    ));
                }
                n => {
                    return Ok(unprefixed(format!(
                        "no colony prefix: the zooids the spawner ran as come from {n} colonies \
                         ({})",
                        inherited
                            .iter()
                            .map(|id| format!("{id:x}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            },
            several => {
                return Ok(unprefixed(format!(
                    "no colony prefix: the spawner is in {} colony groups ({})",
                    several.len(),
                    several
                        .iter()
                        .map(|(id, label)| match label {
                            Ok(label) => label.clone(),
                            Err(_) => format!("{id:x}"),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        };
        Ok(match label {
            Ok(label) => Naming {
                name: super::prefixed_name(Some(&label), brief),
                origin: Some(origin),
                fresh: true,
                unprefixed: None,
            },
            Err(reason) => Naming {
                origin: Some(origin),
                ..unprefixed(format!("no colony prefix: {reason}"))
            },
        })
    }

    fn launch(
        &self,
        brief: Brief,
        workdir: &Path,
        effort: &str,
        spawner_label: &str,
        provider: Provider,
        cover_wish: CoverWish,
    ) -> Result<Launched> {
        if !super::EFFORTS.contains(&effort) {
            bail!(
                "effort {:?} is not one of {}",
                effort,
                super::EFFORTS.join(", ")
            );
        }
        let workdir = std::fs::canonicalize(workdir)
            .with_context(|| format!("workdir {}", workdir.display()))?;
        if !workdir.is_dir() {
            bail!("workdir {} is not a directory", workdir.display());
        }
        let workdir_text = workdir
            .to_str()
            .ok_or_else(|| anyhow!("workdir {} is not UTF-8", workdir.display()))?
            .to_owned();
        super::require_unit_safe("workdir", &workdir_text)?;
        if let Some(percent) = cover_wish.percent {
            cover::check_percent(percent)?;
        }
        if cover_wish.window == Some(0) {
            bail!("--window 0 is not a context window");
        }
        let (provider, provider_reason) = self.resolve_provider(provider)?;
        let (mut spawn_fragment, spawn, brief_handle) = super::spawn_core(&brief.text)?;
        let relations = Relations::with_storage(self.storage.clone());
        let spawner = match relations.person_state(spawner_label)? {
            Some(state) if !state.retired => state.person,
            Some(_) => bail!("spawner {spawner_label} is retired"),
            None => bail!("no persona matches spawner {spawner_label}"),
        };
        let naming = self.naming(spawn, brief_handle, spawner)?;
        let name = naming.name.clone();
        if naming.fresh {
            spawn_fragment += super::name_fragment(spawn, &name, naming.origin);
        }
        let unit = super::unit_for(&name);
        let launch = &self.launch;
        let files = [
            ("prompt", launch.file(&name, "prompt")),
            ("script", launch.file(&name, "sh")),
            ("report", launch.file(&name, "report")),
            ("log", launch.file(&name, "log")),
            ("brief", launch.file(&name, "brief")),
        ];
        for (what, path) in &files {
            super::require_unit_safe(what, &path.display().to_string())?;
        }
        let [(_, prompt_path), (_, script_path), (_, report_path), (_, log_path), (_, brief_path)] =
            files;
        let mcp_config = launch.file(&name, "mcp.json");
        let settings = launch.file(&name, "settings.json");
        super::require_unit_safe("settings", &settings.display().to_string())?;
        let codex_home = launch.file(&name, "codex");
        super::require_unit_safe("codex home", &codex_home.display().to_string())?;
        let vibe_home = launch.file(&name, "vibe");
        let vibe_out = launch.file(&name, "vibe-out");
        super::require_unit_safe("vibe home", &vibe_home.display().to_string())?;
        super::require_unit_safe("vibe output", &vibe_out.display().to_string())?;
        let usage_path = (provider == Provider::Mistral).then(|| vibe_out.join("export.json"));
        let faculties = launch.faculties_binary()?;
        super::require_unit_safe("faculties binary", &faculties.display().to_string())?;
        let (model, model_unset) = launch.model_for(provider);
        if provider == Provider::Codex {
            if let Some(auth) = &launch.codex_auth {
                if !auth.exists() {
                    bail!(
                        "codex is not logged in here: {} does not exist, so a codex zooid \
                         would be refused by the API. Nothing was started; run `codex login`, \
                         or pass --provider claude",
                        auth.display()
                    );
                }
            }
        }
        let routes = self.pinned_routes()?;
        if let Some(env) = &launch.colony_env {
            let text = std::fs::read_to_string(env).with_context(|| {
                format!(
                    "colony environment {} is unreadable; a zooid without it would write to \
                     private collections",
                    env.display()
                )
            })?;
            let missing = super::missing_routes(&text, &routes);
            if !missing.is_empty() {
                bail!(
                    "this launch resolved no route for {}, which {} names: it would record the \
                     run, and pin the zooid, to signer-private collections nobody else reads. \
                     Run it from a shell that has the colony routes (set -a; . {}; set +a).",
                    missing.join(", "),
                    env.display(),
                    env.display()
                );
            }
        }
        let pile = std::fs::canonicalize(self.storage.path())
            .with_context(|| format!("pile {}", self.storage.path().display()))?;
        let key = crate::storage::signer_path(&pile, self.storage.key_path());
        let key = std::fs::canonicalize(&key).with_context(|| format!("key {}", key.display()))?;

        let persona = match relations.person_state(&name)? {
            Some(state) if !state.retired => {
                let unit_state = self
                    .launcher
                    .state(&unit)
                    .unwrap_or_else(|error| format!("unreadable ({error:#})"));
                bail!(
                    "{name} is still running: its persona {:x} is live (unit {unit} is \
                     {unit_state}). Settlement retires it once the spawner is messaged; if the \
                     unit is gone and its settlement failed, run the settle line of {} again, \
                     or `relations retire {name}`.",
                    state.person,
                    script_path.display()
                );
            }
            Some(state) => {
                relations.unretire(&name)?;
                state.person
            }
            None => {
                relations
                    .add(
                        ProfileInput {
                            label: name.clone(),
                            affinities: vec!["zooid".to_owned()],
                            note: Some(format!(
                                "Task zooid spawned from brief {}.",
                                super::handle_text(brief_handle)
                            )),
                            ..Default::default()
                        },
                        None,
                        &[],
                    )?
                    .person
            }
        };

        let mut started = false;
        let launched = (|| -> Result<Launched> {
            let about = brief.about.as_str();
            // With no semantic score available, both Memory and Knowledge can still
            // use the brief lexically. A failed device allocation can panic.
            let lexical = || WakeTint {
                about: about.to_owned(),
                scores: None,
                ..Default::default()
            };
            let (tint, lexical_fallback) = if about.trim().is_empty() {
                (lexical(), None)
            } else {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.renderer.tint(&self.storage, about)
                })) {
                    Ok(Ok(tint)) => {
                        let reason = (!tint.has_memory_scores()
                            && tint.knowledge_contents.is_none()
                            && !tint.diagnostics.is_empty())
                        .then(|| tint.diagnostics.join(", "));
                        (tint, reason)
                    }
                    Ok(Err(error)) => (lexical(), Some(format!("{error:#}"))),
                    Err(panic) => (
                        lexical(),
                        Some(format!("tint panicked: {}", panic_text(&*panic))),
                    ),
                }
            };
            let preamble = super::preamble(&name, spawner_label, provider == Provider::Local);
            let assemble = |wake: &str| super::assemble_prompt(wake, &preamble, &brief.text);
            // A local executor reads its own wake: its first input is the rest.
            let first_input = (provider == Provider::Local)
                .then(|| super::assemble_brief(&preamble, &brief.text));
            let (prompt, cover) = self.size_cover(
                provider,
                model.as_deref(),
                Some(&tint),
                cover_wish,
                &assemble,
            )?;
            // A local executor that names {resume} continues the zooid's own
            // history. Where it ends is read from the archive; a lookup that
            // refuses fails the launch rather than starting the zooid fresh.
            let resumes = match &self.launch.local {
                Some(template) if provider == Provider::Local && names_resume(template) => {
                    self.continuation(&name)?
                }
                _ => None,
            };
            let (run_fragment, run) = super::run_fragment(&RunFacts {
                spawn,
                started_at: crate::clock::point_now()?,
                persona,
                spawner,
                unit: unit.clone(),
                workdir: workdir_text.clone(),
                effort: effort.to_owned(),
                prompt: prompt.clone(),
                provider: provider.name().to_owned(),
                provider_reason: provider_reason.clone(),
                model: model.clone(),
                model_unset: model_unset.clone(),
                untinted: None,
                resumes,
                cover: cover.clone(),
            });
            let text = |path: &Path| path.display().to_string();
            let mut agent = self.agent(
                provider,
                effort,
                &RunFiles {
                    name: name.clone(),
                    workdir: workdir.clone(),
                    script: script_path.clone(),
                    prompt: prompt_path.clone(),
                    report: report_path.clone(),
                    mcp_config: mcp_config.clone(),
                    settings: settings.clone(),
                    codex_home: codex_home.clone(),
                    vibe_home: vibe_home.clone(),
                    vibe_out: vibe_out.clone(),
                    resume: resumes,
                    brief: brief_path.clone(),
                    spawner: spawner_label.to_owned(),
                },
            )?;
            if let Some(input) = &first_input {
                // Local executors read their own wake. Bind the hosted cue to
                // the exact file and its text, including on a resumed run.
                let handle = super::handle_text(super::text_handle(input));
                let scope = if let Some(workspace) = self.storage.selected_workspace() {
                    Some(serde_json::Value::String(format!("{:x}", workspace.id)))
                } else {
                    self.storage.collection_routes().map(|routes| {
                        let descriptors = routes
                            .iter()
                            .map(|(role, root)| {
                                (
                                    format!("{role:x}"),
                                    serde_json::Value::String(hex::encode(root.raw)),
                                )
                            })
                            .collect();
                        serde_json::Value::Object(descriptors)
                    })
                };
                agent.env.push((
                    "SPAWN_BRIEF_TOPIC".to_owned(),
                    serde_json::to_string(&(&brief_path, handle, about, scope))?,
                ));
            }
            let script = super::unit_script(&ScriptFacts {
                name: &name,
                run,
                pile: &text(&pile),
                key: &text(&key),
                path: &launch.path,
                colony_env: launch.colony_env.as_deref().map(text).as_deref(),
                routes: &routes,
                agent: &agent.argv,
                agent_env: &agent.env,
                agent_secrets: &agent.secrets,
                secrets: &launch.secrets,
                usage: usage_path.as_deref().map(text).as_deref(),
                stdout_is_report: agent.stdout_is_report,
                prompt: &text(&prompt_path),
                report: &text(&report_path),
                log: &text(&log_path),
                spawn: &launch.spawn,
                faculties: &text(&faculties),
            });
            std::fs::create_dir_all(&launch.state_dir)
                .with_context(|| format!("create {}", launch.state_dir.display()))?;
            match provider {
                Provider::Codex => codex_home_for_run(
                    &codex_home,
                    launch.codex_auth.as_deref(),
                    &codex_mcp_config(&script_path),
                )?,
                Provider::Mistral => {
                    vibe_home_for_run(
                        &vibe_home,
                        &vibe_config(&script_path, &launch.mistral_model, vibe_thinking(effort)),
                    )?;
                    // An earlier run's export must never be read as this
                    // run's usage.
                    if let Some(path) = &usage_path {
                        match std::fs::remove_file(path) {
                            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                                return Err(error).context("remove the previous vibe export");
                            }
                            _ => {}
                        }
                    }
                }
                Provider::Claude => {
                    std::fs::write(&mcp_config, claude_mcp_config(&script_path))
                        .with_context(|| format!("write {}", mcp_config.display()))?;
                    std::fs::write(&settings, claude_hook_settings(&script_path))
                        .with_context(|| format!("write {}", settings.display()))?;
                }
                // The executor's own configuration is the host's.
                Provider::Local => {}
                Provider::Auto => bail!("{} reached the launch unresolved", provider.name()),
            }
            std::fs::write(&prompt_path, &prompt)?;
            if let Some(first_input) = &first_input {
                std::fs::write(&brief_path, first_input)?;
            }
            std::fs::write(&script_path, script)?;
            match std::fs::remove_file(&report_path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error).context("remove the previous report");
                }
                _ => {}
            }
            self.launcher.start(&UnitSpec {
                unit: unit.clone(),
                workdir: workdir.clone(),
                script: script_path.clone(),
                log: log_path.clone(),
                memory_max: launch.memory_max.clone(),
                runtime_max: (provider == Provider::Local)
                    .then_some(launch.local_max_runtime)
                    .flatten(),
                interrupt: provider == Provider::Local,
            })?;
            started = true;
            let mut record = spawn_fragment;
            record += run_fragment;
            if let Some(why) = &lexical_fallback {
                record +=
                    entity! { ExclusiveId::force_ref(&run) @ attrs::lexical_fallback: why.clone() };
            }
            if let Some((goal, revision)) = &brief.goal {
                record += super::goal_fragment(spawn, *goal, revision);
            }
            self.publish(record).with_context(|| {
                format!("{unit} is running, but its spawn record was not published")
            })?;
            let mut warnings = Vec::new();
            let mut goal_note = None;
            if let Some((goal, revision)) = &brief.goal {
                let note = format!(
                    "spawned {name} run {run:x} from goal revision {}. `zooid show {name}` reads \
                     it; the goal's status is {name}'s to move.",
                    revision
                        .iter()
                        .map(|id| format!("{id:x}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                match Goals::with_storage(self.storage.clone()).note(
                    &format!("{goal:x}"),
                    &note,
                    NoteOptions {
                        persona: Some(&name),
                        ..Default::default()
                    },
                ) {
                    Ok(added) => {
                        goal_note = Some(added.note);
                        if let Err(error) =
                            self.publish(super::run_annotation(run, None, None, Some(added.note)))
                        {
                            warnings.push(format!(
                                "the goal note {:x} was added, but recording it on the run \
                                 failed: {error:#}",
                                added.note
                            ));
                        }
                    }
                    Err(error) => warnings.push(format!(
                        "{unit} is running, but noting the launch on goal {goal:x} failed: \
                         {error:#}"
                    )),
                }
            }
            Ok(Launched {
                name: name.clone(),
                spawn,
                run,
                persona,
                brief: brief_handle,
                unit: unit.clone(),
                report: report_path.clone(),
                log: log_path.clone(),
                prompt_chars: prompt.chars().count(),
                provider,
                provider_reason: provider_reason.clone(),
                model: model.clone(),
                model_unset: model_unset.clone(),
                untinted: None,
                lexical_fallback,
                resumes,
                cover,
                unprefixed: naming.unprefixed.clone(),
                goal_note,
                warnings,
            })
        })();
        if launched.is_err() && !started {
            // Nothing runs under this persona; leave the brief relaunchable.
            if let Err(error) = relations.retire(&name) {
                return launched.map_err(|launch| {
                    launch.context(format!("retiring {name} afterwards also failed: {error:#}"))
                });
            }
        }
        launched
    }

    /// Record a run's end, tell the spawner, and then retire the zooid's
    /// persona. Safe to run again: what an earlier settlement recorded is not
    /// recorded twice. A step that fails is recorded on the run; while the
    /// spawner has not been messaged the persona stays live, so a repeated
    /// settlement can still message as the zooid.
    pub fn settle(&self, request: &SettleRequest) -> Result<Settled> {
        let run = request.run;
        let (already_ended, earlier_message, local, agent) = self.observe(|facts, reader| {
            Ok((
                find!(at: IntervalValue, pattern!(facts, [{ run @ metadata::finished_at: ?at }]))
                    .next()
                    .is_some(),
                find!(id: Id, pattern!(facts, [{ run @ attrs::message: ?id }])).next(),
                exists!(pattern!(facts, [{ run @ attrs::provider: Provider::Local.name() }])),
                // The agent and model the run record names, for reading what
                // the provider reported about the run.
                (
                    find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::provider: ?handle }]))
                        .next()
                        .and_then(|handle| read_text(reader, handle)),
                    find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::model: ?handle }]))
                        .next()
                        .and_then(|handle| read_text(reader, handle)),
                ),
            ))
        })?;
        let (agent, agent_model) = agent;
        let mut failures = Vec::new();
        // A hosted run's report or a local run's closing message. Either is
        // optional: no file, or an empty one, records none, and how the run
        // ended is its exit alone.
        let mut report_handle = None;
        if !already_ended {
            let mut reported = Reported::default();
            let report = match &request.report {
                Some(path) => match std::fs::read(path) {
                    Ok(bytes) if bytes.is_empty() => None,
                    Ok(bytes) => self.read_report(
                        &bytes,
                        agent.as_deref(),
                        agent_model.as_deref(),
                        &mut reported,
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        return Err(error).with_context(|| format!("read {}", path.display()));
                    }
                },
                None => None,
            };
            let usage = match &request.usage {
                Some(path) => match std::fs::read_to_string(path)
                    .with_context(|| format!("read {}", path.display()))
                    .and_then(|text| super::vibe_usage(&text))
                {
                    Ok(usage) => Some(usage),
                    Err(error) => {
                        let why = format!("record the run's usage: {error:#}");
                        if let Err(record) =
                            self.publish(super::run_annotation(run, None, Some(&why), None))
                        {
                            failures.push(record.context("record the settlement failure"));
                        }
                        failures.push(error.context("record the run's usage"));
                        None
                    }
                },
                None => None,
            };
            let (fragment, handle) = super::settlement_fragment(
                run,
                crate::clock::point_now()?,
                &request.exit,
                report.as_deref(),
                usage.as_ref(),
                &reported,
            );
            report_handle = handle;
            if let Err(error) = self.publish(fragment) {
                failures.push(error.context("record the run's end"));
            }
        }
        // What the pile now says about the run is what the message reports,
        // so a repeated settlement describes the end that was recorded.
        let recorded = self.observe(|facts, reader| {
            let started = find!(
                at: IntervalValue,
                pattern!(facts, [{ run @ metadata::started_at: ?at }])
            )
            .next();
            Ok(started.map(|started| run_view(facts, reader, run, started)))
        });
        let (spawner, end, clean, report) = match &recorded {
            Ok(Some(view)) => (
                view.spawner.first().copied(),
                describe_end(view),
                ended_clean(view, local),
                view.report.first().copied().or(report_handle),
            ),
            Ok(None) => (
                None,
                request.exit.describe(),
                request.exit.clean(local),
                report_handle,
            ),
            Err(error) => {
                failures.push(anyhow!("read the run back: {error:#}"));
                (
                    None,
                    request.exit.describe(),
                    request.exit.clean(local),
                    report_handle,
                )
            }
        };
        let mut message = earlier_message;
        if message.is_none() {
            let sent = match spawner {
                Some(spawner) => {
                    let text = format!(
                        "{} finished, {end}, {}. `zooid show {}` reads it.{}",
                        request.name,
                        lifecycle_text(local, report, clean),
                        request.name,
                        self.unread_note(&request.name)
                    );
                    crate::message::Message::with_storage(self.storage.clone())
                        .send(&crate::message::SendOptions {
                            from: &request.name,
                            to: &format!("{spawner:x}"),
                            text: &text,
                        })
                        .map(|sent| sent.id)
                        .context("message the spawner")
                }
                None => Err(anyhow!("run {run:x} names no spawner to message")),
            };
            match sent {
                Ok(id) => {
                    message = Some(id);
                    if let Err(error) =
                        self.publish(super::run_annotation(run, Some(id), None, None))
                    {
                        failures.push(error.context(format!(
                            "message {id:x} was sent, but recording it on the run failed"
                        )));
                    }
                }
                Err(error) => {
                    let why = format!("{error:#}");
                    if let Err(record) =
                        self.publish(super::run_annotation(run, None, Some(&why), None))
                    {
                        failures.push(record.context("record the settlement failure"));
                    }
                    failures.push(error.context(format!(
                        "{} stays live so that settling again can message",
                        request.name
                    )));
                }
            }
        }
        if message.is_some() {
            if let Err(error) = Relations::with_storage(self.storage.clone()).retire(&request.name)
            {
                let why = format!("retire {}: {error:#}", request.name);
                // Best effort: the failure itself is returned either way.
                let _ = self.publish(super::run_annotation(run, None, Some(&why), None));
                failures.push(error.context(format!("retire {}", request.name)));
            }
        }
        let mut failures = failures.into_iter();
        match failures.next() {
            None => Ok(Settled {
                run,
                report,
                message,
                already_ended,
                already_messaged: earlier_message.is_some(),
            }),
            Some(first) => Err(failures.fold(first, |error, next| {
                error.context(format!("also: {next:#}"))
            })),
        }
    }

    /// A report file's text. A claude run's stdout is its result object:
    /// the report is the `result` text, as the text format printed it, and
    /// the window the CLI reported and the first call's input tokens (from
    /// the session transcript) go into `reported`. Bytes that are not a
    /// result object (a crash, partial output, a run started before the
    /// JSON format) are recorded as they are, as is a result with no text.
    /// An empty `result` is no report, as empty text output always was; the
    /// window and first-call tokens are still kept.
    fn read_report(
        &self,
        bytes: &[u8],
        agent: Option<&str>,
        model: Option<&str>,
        reported: &mut Reported,
    ) -> Option<String> {
        let raw = || String::from_utf8_lossy(bytes).into_owned();
        if agent != Some(Provider::Claude.name()) {
            return Some(raw());
        }
        let Some(result) = cover::claude_result(bytes, model) else {
            return Some(raw());
        };
        reported.context_window = result.context_window;
        reported.first_call_input_tokens = result
            .session_id
            .as_deref()
            .and_then(|session| self.claude_first_call(session));
        // The archive names an imported Claude Code session `session/<id>`.
        reported.source_locator = result
            .session_id
            .as_deref()
            .filter(|session| !session.is_empty() && !session.contains(['/', '.']))
            .map(|session| format!("session/{session}"));
        match result.report {
            Some(text) if text.is_empty() => None,
            Some(text) => Some(text),
            None => Some(raw()),
        }
    }

    /// The first model call's input tokens in claude's transcript of a
    /// session: `<projects>/<any project>/<session>.jsonl`.
    fn claude_first_call(&self, session: &str) -> Option<u64> {
        if session.is_empty() || session.contains(['/', '.']) {
            return None;
        }
        let projects = self.launch.claude_projects.as_ref()?;
        std::fs::read_dir(projects)
            .ok()?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join(format!("{session}.jsonl")))
            .find(|path| path.is_file())
            .and_then(|path| std::fs::File::open(path).ok())
            .and_then(|file| {
                use std::io::BufRead as _;
                cover::claude_first_call(
                    std::io::BufReader::new(file).lines().map_while(Result::ok),
                )
            })
    }

    /// Every spawn, by name.
    pub fn list(&self) -> Result<Vec<SpawnView>> {
        self.observe(|facts, reader| spawns_in(facts, reader))
    }

    /// One spawn by its name.
    pub fn show(&self, name: &str) -> Result<SpawnView> {
        let name_handle = super::text_handle(name.trim());
        self.observe(|facts, reader| {
            let found = find!(
                (spawn: Id, brief: TextHandle),
                pattern!(facts, [{ ?spawn @
                    metadata::tag: KIND_SPAWN,
                    attrs::name: name_handle,
                    attrs::brief: ?brief,
                }])
            )
            .collect::<Vec<_>>();
            match found.as_slice() {
                [] => bail!("no spawn is named {name}"),
                [(spawn, brief)] => spawn_view(facts, reader, *spawn, *brief),
                several => bail!(
                    "{} briefs share the name {name}: {}",
                    several.len(),
                    several
                        .iter()
                        .map(|(spawn, _)| format!("{spawn:x}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        })
    }

    /// The unit's state, as the launcher reads it.
    pub fn unit_state(&self, unit: &str) -> Result<String> {
        self.launcher.state(unit)
    }

    /// Block until the latest run's unit has ended, polling the launcher;
    /// the pile is not held while waiting. Returns the spawn afterwards.
    pub fn wait(&self, name: &str, poll: Duration) -> Result<SpawnView> {
        let view = self.show(name)?;
        let unit = view
            .runs
            .last()
            .and_then(|run| run.unit.first().cloned())
            .ok_or_else(|| anyhow!("{name} has no recorded run to wait for"))?;
        while is_running(&self.launcher.state(&unit)?) {
            std::thread::sleep(poll);
        }
        self.show(name)
    }

    /// What the zooid never read: unread messages addressed to it, named in
    /// its settle message so the spawner sees a missed message instead of
    /// assuming it arrived. The preamble only asks the zooid to poll; this
    /// makes a poll that never happened visible. An inbox that cannot be
    /// read is said to be unread, not reported as empty.
    fn unread_note(&self, name: &str) -> String {
        match self.unread(name) {
            Ok(unread) => {
                let unread: Vec<String> = unread
                    .iter()
                    .map(|entry| format!("{:x} from {}", entry.id, entry.from_label))
                    .collect();
                if unread.is_empty() {
                    String::new()
                } else {
                    format!(
                        " It never read {} message(s) sent to it: {}.",
                        unread.len(),
                        unread.join(", ")
                    )
                }
            }
            Err(error) => format!(" Its inbox could not be read at settle: {error:#}."),
        }
    }

    /// The incoming messages addressed to `name` that it has not
    /// acknowledged: the one query settlement and the inbox hooks share.
    fn unread(&self, name: &str) -> Result<Vec<crate::message::MessageObservation>> {
        let mut options = crate::message::ListOptions::new(name);
        options.unread = true;
        options.limit = 50;
        let list = crate::message::Message::with_storage(self.storage.clone()).list(&options)?;
        Ok(list
            .entries
            .into_iter()
            .filter(|entry| entry.incoming)
            .collect())
    }

    /// Answer one of a running claude zooid's inbox hooks: what its unit
    /// script prints as `inbox stop` or `inbox post`. Showing a message never
    /// acknowledges it; the zooid does that itself.
    ///
    /// Stop blocks the end once while anything is unread, naming every
    /// message, and lets the zooid end when it is already continuing from a
    /// Stop block. Post is throttled per run: it reads the inbox at most once
    /// per [`INBOX_POST_INTERVAL_SECS`], under an exclusive lock on the run's
    /// inbox state file so parallel tool calls take turns, and shows only
    /// messages it has not shown this run. A throttled post opens no pile.
    pub fn inbox(&self, request: &InboxRequest) -> InboxAnswer {
        match request.hook {
            InboxHook::Stop => {
                if request.stop_hook_active {
                    return InboxAnswer::default();
                }
                match self.unread(&request.name) {
                    Ok(unread) if unread.is_empty() => InboxAnswer::default(),
                    Ok(unread) => InboxAnswer {
                        output: Some(serde_json::json!({
                            "decision": "block",
                            "reason": stop_reason(&request.name, &unread),
                        })),
                        warning: None,
                    },
                    Err(error) => inbox_failure(InboxHook::Stop, &error),
                }
            }
            InboxHook::Post => match self.post(request) {
                Ok(answer) => answer,
                Err(error) => inbox_failure(InboxHook::Post, &error),
            },
        }
    }

    fn post(&self, request: &InboxRequest) -> Result<InboxAnswer> {
        let path = self.launch.file(&request.name, "inbox.json");
        std::fs::create_dir_all(&self.launch.state_dir)
            .with_context(|| format!("create {}", self.launch.state_dir.display()))?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open the inbox state {}", path.display()))?;
        // Never unlinked, so every hook of the run locks the same inode.
        file.lock()
            .with_context(|| format!("lock {}", path.display()))?;
        let mut text = String::new();
        std::io::Read::read_to_string(&mut file, &mut text)
            .with_context(|| format!("read {}", path.display()))?;
        let mut warning = None;
        let mut state = if text.trim().is_empty() {
            InboxState::default()
        } else {
            InboxState::parse(&text).unwrap_or_else(|error| {
                warning = Some(format!(
                    "{} was unreadable ({error:#}); starting this run's inbox state afresh",
                    path.display()
                ));
                InboxState::default()
            })
        };
        if state.run != Some(request.run) {
            // An earlier run of the same name: nothing it showed was shown here.
            state = InboxState {
                run: Some(request.run),
                ..Default::default()
            };
        }
        if let Some(checked) = state.checked_at {
            if checked <= request.now && request.now - checked < INBOX_POST_INTERVAL_SECS {
                return Ok(InboxAnswer {
                    output: None,
                    warning,
                });
            }
        }
        // Stamped before reading, so a failing read is throttled too and
        // never costs every tool call a pile open.
        state.checked_at = Some(request.now);
        let write = |file: &mut std::fs::File, state: &InboxState| -> Result<()> {
            use std::io::{Seek as _, Write as _};
            file.set_len(0)?;
            file.seek(std::io::SeekFrom::Start(0))?;
            file.write_all(state.render().as_bytes())?;
            Ok(())
        };
        write(&mut file, &state).with_context(|| format!("write {}", path.display()))?;
        let unread = self.unread(&request.name)?;
        let new: Vec<_> = unread
            .into_iter()
            .filter(|entry| !state.shown.contains(&entry.id))
            .collect();
        if new.is_empty() {
            return Ok(InboxAnswer {
                output: None,
                warning,
            });
        }
        state.shown.extend(new.iter().map(|entry| entry.id));
        write(&mut file, &state).with_context(|| format!("write {}", path.display()))?;
        Ok(InboxAnswer {
            output: Some(serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": post_context(&request.name, &new),
                }
            })),
            warning,
        })
    }

    fn observe<T>(
        &self,
        read: impl FnOnce(&FactArchive, &AcquiringReader<FacultySnapshot>) -> Result<T>,
    ) -> Result<T> {
        observe_scope(
            &self.storage,
            DEFAULT_SCOPE_ID,
            "Spawn",
            |facts, reader, _| read(facts, reader),
        )
    }

    /// Where the zooid `name`'s archived history ends, for a local run that
    /// continues it: read from the archive collection's own facts
    /// ([`super::continuation::zooid_tail`]). `None` when no context names
    /// it; an archive that cannot say refuses.
    pub fn continuation(&self, name: &str) -> Result<Option<Id>> {
        observe_scope(
            &self.storage,
            crate::schemas::blockdag::DEFAULT_SCOPE_ID,
            "Archive",
            |facts, _, _| super::continuation::zooid_tail(facts, name),
        )
        .with_context(|| format!("find where zooid {name}'s archived history ends"))
    }

    pub(super) fn publish(&self, fragment: Fragment) -> Result<()> {
        self.storage.with_store(|pile, signer, runtime| {
            let collection = self.storage.open_collection_write(
                pile,
                DEFAULT_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            crate::collection_names::require_command_write_admission_acquiring(
                pile,
                collection,
                signer,
                "Spawn",
                "zooid list",
                runtime,
            )?;
            pile.commit(collection, signer, fragment)
                .context("commit Spawn fragment")?;
            drop(
                pollster::block_on(crate::storage::ensure_downstream(pile, collection, signer))
                    .context(
                        "Spawn facts were committed, but ensuring their derived views failed",
                    )?,
            );
            Ok(())
        })
    }
}

/// Read one scope's facts in `storage`, with the source collection they are
/// read from.
pub(super) fn observe_scope<T>(
    storage: &Storage,
    scope: Id,
    label: &str,
    read: impl FnOnce(&FactArchive, &AcquiringReader<FacultySnapshot>, CollectionHandle) -> Result<T>,
) -> Result<T> {
    storage.with_store(|pile, signer, runtime| {
        let source = storage.open_collection_read(pile, scope, signer.verifying_key(), runtime)?;
        let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
        let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
        runtime
            .block_on(async {
                crate::storage::tolerate_own_lag(pile.maintain_attached(succinct, signer).await)?;
                crate::storage::tolerate_own_lag(pile.maintain_attached(rank9, signer).await)
            })
            .with_context(|| format!("maintain {label} fact collection"))?;
        let snapshot = AcquiringReader::new(pile.snapshot()?, Arc::clone(runtime));
        let facts = crate::storage::acquire_facts(&snapshot, rank9)
            .with_context(|| format!("read {label} Rank9 collection"))?;
        read(&facts, &snapshot, source.handle())
    })
}

/// A caught panic's message, as `panic!` gave it.
fn panic_text(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("non-text panic payload")
}

pub(super) fn read_text<R: BlobStoreGet + ?Sized>(
    reader: &R,
    handle: TextHandle,
) -> Option<String> {
    reader
        .get::<View<str>, _>(handle)
        .ok()
        .map(|text| text.to_string())
}

/// Every spawn in these facts, by name. [`Spawn::list`] over facts a caller
/// already holds, such as the viewer's routed Spawn collection.
pub(super) fn spawns_in<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
) -> Result<Vec<SpawnView>> {
    let mut spawns = find!(
        (spawn: Id, brief: TextHandle),
        pattern!(facts, [{ ?spawn @ metadata::tag: KIND_SPAWN, attrs::brief: ?brief }])
    )
    .map(|(spawn, brief)| spawn_view(facts, reader, spawn, brief))
    .collect::<Result<Vec<_>>>()?;
    spawns.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(spawns)
}

fn spawn_view<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    spawn: Id,
    brief: TextHandle,
) -> Result<SpawnView> {
    let mut runs = find!(
        (run: Id, started: IntervalValue),
        pattern!(facts, [{ ?run @
            metadata::tag: KIND_RUN,
            attrs::of: spawn,
            metadata::started_at: ?started,
        }])
    )
    .collect::<Vec<_>>();
    // Oldest first; an undecodable time sorts first rather than hiding a run.
    runs.sort_by_key(|(run, started)| {
        let start = started
            .try_from_inline::<(Epoch, Epoch)>()
            .map(|(start, _)| start.to_tai_duration().total_nanoseconds())
            .unwrap_or(i128::MIN);
        (start, *run)
    });
    let runs = runs
        .into_iter()
        .map(|(run, started)| run_view(facts, reader, run, started))
        .collect();
    let names = shown_names(facts, reader, spawn);
    Ok(SpawnView {
        id: spawn,
        name: if names.is_empty() {
            super::name_for(brief)
        } else {
            names.join(" | ")
        },
        names,
        origin_colony: find!(id: Id, pattern!(facts, [{ spawn @ attrs::origin_colony: ?id }]))
            .collect(),
        brief,
        brief_text: read_text(reader, brief),
        goal: find!(id: Id, pattern!(facts, [{ spawn @ attrs::goal: ?id }])).collect(),
        goal_revision: find!(id: Id, pattern!(facts, [{ spawn @ attrs::goal_revision: ?id }]))
            .collect(),
        declared: super::declaration::declared_view(facts, reader, spawn),
        runs,
        requests: requests_of(facts, reader, spawn),
    })
}

/// Every past request for a zooid, oldest first.
fn requests_of<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    zooid: Id,
) -> Vec<RequestView> {
    let mut requests: Vec<RequestView> = find!(
        request: Id,
        pattern!(facts, [{ ?request @ metadata::tag: KIND_REQUEST, attrs::zooid: zooid }])
    )
    .map(|request| request_view(facts, reader, request))
    .collect();
    requests.sort_by_key(|view| (earliest(&view.requested_at), view.id));
    requests
}

/// The earliest start of these times; an undecodable one sorts first.
fn earliest(times: &[IntervalValue]) -> i128 {
    times
        .iter()
        .map(|at| {
            at.try_from_inline::<(Epoch, Epoch)>()
                .map(|(start, _)| start.to_tai_duration().total_nanoseconds())
                .unwrap_or(i128::MIN)
        })
        .min()
        .unwrap_or(i128::MIN)
}

fn run_view<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    run: Id,
    started: IntervalValue,
) -> RunView {
    let texts = |handles: Vec<TextHandle>| -> Vec<String> {
        handles
            .into_iter()
            .map(|handle| read_text(reader, handle).unwrap_or_else(|| super::handle_text(handle)))
            .collect()
    };
    let report: Vec<TextHandle> = find!(
        handle: TextHandle,
        pattern!(facts, [{ run @ attrs::report: ?handle }])
    )
    .collect();
    RunView {
        id: run,
        started: Some(started),
        finished: find!(
            at: IntervalValue,
            pattern!(facts, [{ run @ metadata::finished_at: ?at }])
        )
        .collect(),
        exit_code: find!(
            code: Inline<inlineencodings::U256BE>,
            pattern!(facts, [{ run @ attrs::exit_code: ?code }])
        )
        .filter_map(|code| code.try_from_inline::<u64>().ok())
        .collect(),
        result: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::result: ?handle }]))
                .collect(),
        ),
        signal: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::signal: ?handle }]))
                .collect(),
        ),
        unit: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::unit: ?handle }])).collect(),
        ),
        workdir: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::workdir: ?handle }]))
                .collect(),
        ),
        effort: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::effort: ?handle }]))
                .collect(),
        ),
        persona: find!(id: Id, pattern!(facts, [{ run @ attrs::persona: ?id }])).collect(),
        spawner: find!(id: Id, pattern!(facts, [{ run @ archive_attrs::author: ?id }])).collect(),
        prompt: find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::prompt: ?handle }]))
            .collect(),
        report_text: report.first().and_then(|handle| read_text(reader, *handle)),
        report,
        provider: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::provider: ?handle }]))
                .collect(),
        ),
        provider_reason: texts(
            find!(
                handle: TextHandle,
                pattern!(facts, [{ run @ attrs::provider_reason: ?handle }])
            )
            .collect(),
        ),
        untinted: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::untinted: ?handle }]))
                .collect(),
        ),
        lexical_fallback: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::lexical_fallback: ?handle }])).collect(),
        ),
        resumes: find!(node: Id, pattern!(facts, [{ run @ attrs::resumes: ?node }])).collect(),
        realises: find!(id: Id, pattern!(facts, [{ run @ attrs::realises: ?id }])).collect(),
        model: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::model: ?handle }])).collect(),
        ),
        model_unset: texts(
            find!(
                handle: TextHandle,
                pattern!(facts, [{ run @ attrs::model_unset: ?handle }])
            )
            .collect(),
        ),
        message: find!(id: Id, pattern!(facts, [{ run @ attrs::message: ?id }])).collect(),
        settle_failure: texts(
            find!(
                handle: TextHandle,
                pattern!(facts, [{ run @ attrs::settle_failure: ?handle }])
            )
            .collect(),
        ),
        goal_note: find!(id: Id, pattern!(facts, [{ run @ attrs::goal_note: ?id }])).collect(),
        request: find!(id: Id, pattern!(facts, [{ run @ attrs::request: ?id }])).collect(),
        input_tokens: counts(
            find!(
                count: Inline<inlineencodings::U256BE>,
                pattern!(facts, [{ run @ imagine_attrs::input_tokens: ?count }])
            )
            .collect(),
        ),
        cached_input_tokens: counts(
            find!(
                count: Inline<inlineencodings::U256BE>,
                pattern!(facts, [{ run @ attrs::cached_input_tokens: ?count }])
            )
            .collect(),
        ),
        output_tokens: counts(
            find!(
                count: Inline<inlineencodings::U256BE>,
                pattern!(facts, [{ run @ imagine_attrs::output_tokens: ?count }])
            )
            .collect(),
        ),
        cost_usd: find!(cost: f64, pattern!(facts, [{ run @ attrs::cost_usd: ?cost }])).collect(),
        cover_sizing: texts(
            find!(handle: TextHandle, pattern!(facts, [{ run @ attrs::cover_sizing: ?handle }]))
                .collect(),
        ),
        context_window: counts(
            find!(
                count: Inline<inlineencodings::U256BE>,
                pattern!(facts, [{ run @ attrs::context_window: ?count }])
            )
            .collect(),
        ),
        first_call_input_tokens: counts(
            find!(
                count: Inline<inlineencodings::U256BE>,
                pattern!(facts, [{ run @ attrs::first_call_input_tokens: ?count }])
            )
            .collect(),
        ),
        source_locator: texts(
            find!(
                handle: TextHandle,
                pattern!(facts, [{
                    run @ crate::schemas::blockdag::source_projection::source_locator: ?handle
                }])
            )
            .collect(),
        ),
    }
}

fn counts(values: Vec<Inline<inlineencodings::U256BE>>) -> Vec<u64> {
    values
        .into_iter()
        .filter_map(|value| value.try_from_inline::<u64>().ok())
        .collect()
}

fn request_view<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    request: Id,
) -> RequestView {
    let texts = |handles: Vec<TextHandle>| -> Vec<String> {
        handles
            .into_iter()
            .map(|handle| read_text(reader, handle).unwrap_or_else(|| super::handle_text(handle)))
            .collect()
    };
    let mut outcomes: Vec<OutcomeView> = find!(
        (outcome: Id, kind: Id),
        pattern!(facts, [{ ?outcome @
            metadata::tag: KIND_OUTCOME,
            attrs::request: request,
            attrs::outcome: ?kind,
        }])
    )
    .map(|(outcome, kind)| OutcomeView {
        id: outcome,
        kind,
        at: find!(
            at: IntervalValue,
            pattern!(facts, [{ outcome @ metadata::created_at: ?at }])
        )
        .collect(),
        run: find!(id: Id, pattern!(facts, [{ outcome @ attrs::outcome_run: ?id }])).collect(),
        reason: texts(
            find!(handle: TextHandle, pattern!(facts, [{ outcome @ attrs::reason: ?handle }]))
                .collect(),
        ),
    })
    .collect();
    outcomes.sort_by_key(|outcome| (earliest(&outcome.at), outcome.id));
    let runs = find!(
        run: Id,
        pattern!(facts, [{ ?run @ metadata::tag: KIND_RUN, attrs::request: request }])
    )
    .map(|run| {
        let ended = find!(
            at: IntervalValue,
            pattern!(facts, [{ run @ metadata::finished_at: ?at }])
        )
        .next()
        .is_some();
        (run, ended)
    })
    .collect();
    RequestView {
        id: request,
        host: find!(host: HostKey, pattern!(facts, [{ request @ attrs::host: ?host }])).collect(),
        host_label: texts(
            find!(handle: TextHandle, pattern!(facts, [{ request @ attrs::host_label: ?handle }]))
                .collect(),
        ),
        provider: texts(
            find!(handle: TextHandle, pattern!(facts, [{ request @ attrs::provider: ?handle }]))
                .collect(),
        ),
        effort: texts(
            find!(handle: TextHandle, pattern!(facts, [{ request @ attrs::effort: ?handle }]))
                .collect(),
        ),
        previous: find!(id: Id, pattern!(facts, [{ request @ attrs::previous: ?id }])).collect(),
        handoff: texts(
            find!(handle: TextHandle, pattern!(facts, [{ request @ attrs::handoff: ?handle }]))
                .collect(),
        ),
        cover_percent: find!(
            percent: f64,
            pattern!(facts, [{ request @ attrs::cover_percent: ?percent }])
        )
        .collect(),
        spawner: find!(id: Id, pattern!(facts, [{ request @ archive_attrs::author: ?id }]))
            .collect(),
        requested_at: find!(
            at: IntervalValue,
            pattern!(facts, [{ request @ metadata::created_at: ?at }])
        )
        .collect(),
        outcomes,
        runs,
    }
}

/// The name of an outcome kind.
pub fn outcome_name(kind: Id) -> String {
    match kind {
        kind if kind == OUTCOME_ACCEPTED => "accepted".to_owned(),
        kind if kind == OUTCOME_RUNNING => "running".to_owned(),
        kind if kind == OUTCOME_REFUSED => "refused".to_owned(),
        kind if kind == OUTCOME_TERMINAL => "task-terminal".to_owned(),
        other => format!("{other:x}"),
    }
}

pub(super) fn join<T: std::fmt::Display>(values: &[T]) -> String {
    if values.is_empty() {
        return "-".to_owned();
    }
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn ids(values: &[Id]) -> String {
    join(
        &values
            .iter()
            .map(|id| format!("{id:x}"))
            .collect::<Vec<_>>(),
    )
}

/// How a run ended, in a few words.
pub fn describe_end(run: &RunView) -> String {
    if !run.ended() {
        return "not ended".to_owned();
    }
    let end = if !run.exit_code.is_empty() {
        format!("exit {}", join(&run.exit_code))
    } else if !run.signal.is_empty() {
        format!("signal {}", join(&run.signal))
    } else if !run.result.is_empty() {
        join(&run.result)
    } else {
        "ended, exit unknown".to_owned()
    };
    if run
        .result
        .iter()
        .any(|result| result == super::TIMEOUT_RESULT)
    {
        return format!("{end}, {}", super::RUNTIME_LIMIT_NOTE);
    }
    end
}

/// What the spawner's lifecycle message says the run left: a hosted run's
/// report, or a local run's closing message, or that there is none.
/// Whatever a failed or stopped run left is not its finished word.
fn lifecycle_text(local: bool, report: Option<TextHandle>, clean: bool) -> String {
    let (what, none) = if local {
        ("closing message", "no closing message")
    } else {
        ("report", "report none")
    };
    match report {
        None => none.to_owned(),
        Some(report) if clean => format!("{what} {}", super::handle_text(report)),
        Some(report) => format!(
            "{what} {} (partial: the run did not end cleanly)",
            super::handle_text(report)
        ),
    }
}

/// Whether a recorded run ended as a success: exit 0 alone, and, for a
/// hosted run, not stopped by its runtime limit. A local run lives until it
/// is interrupted, its limit included, so its exit 0 after the limit is its
/// ordinary teardown. A local run's closing message is optional and plays
/// no part.
pub fn ended_clean(run: &RunView, local: bool) -> bool {
    run.exit_code == [0]
        && run.signal.is_empty()
        && (local
            || !run
                .result
                .iter()
                .any(|result| result == super::TIMEOUT_RESULT))
}

/// One line per spawn: name, runs, how the latest ended, the brief's first line.
pub fn render_list(spawns: &[SpawnView]) -> String {
    let mut out = String::new();
    if spawns.is_empty() {
        out.push_str("No spawns.\n");
    }
    for spawn in spawns {
        let latest = spawn
            .runs
            .last()
            .map(|run| {
                let started = run
                    .started
                    .map(crate::care::format_time)
                    .unwrap_or_default();
                format!("{started} {}", describe_end(run))
            })
            .unwrap_or_else(|| "never run".to_owned());
        let first_line = spawn
            .brief_text
            .as_deref()
            .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or("(brief not here)");
        let first_line: String = first_line.trim().chars().take(80).collect();
        let conflict = if spawn.names.len() > 1 {
            format!("  (name conflict: {} names)", spawn.names.len())
        } else {
            String::new()
        };
        writeln!(
            out,
            "{}  runs {}  latest {}  {}{conflict}",
            spawn.name,
            spawn.runs.len(),
            latest,
            first_line
        )
        .unwrap();
    }
    out
}

/// One past request of a zooid, with what was recorded about it.
fn show_request(out: &mut String, request: &RequestView) {
    writeln!(out, "\npast request {:x}", request.id).unwrap();
    let hosts: Vec<String> = request.host.iter().map(|h| super::host_text(*h)).collect();
    let label = if request.host_label.is_empty() {
        String::new()
    } else {
        format!(" ({})", join(&request.host_label))
    };
    writeln!(out, "  host: {}{label}", join(&hosts)).unwrap();
    let asked: Vec<String> = request
        .requested_at
        .iter()
        .map(|at| crate::care::format_time(*at))
        .collect();
    writeln!(out, "  asked: {}", join(&asked)).unwrap();
    writeln!(out, "  spawner: {}", ids(&request.spawner)).unwrap();
    writeln!(out, "  provider: {}", join(&request.provider)).unwrap();
    writeln!(out, "  effort: {}", join(&request.effort)).unwrap();
    if !request.cover_percent.is_empty() {
        writeln!(out, "  cover: {}%", join(&request.cover_percent)).unwrap();
    }
    if !request.previous.is_empty() {
        writeln!(out, "  previous run: {}", ids(&request.previous)).unwrap();
    }
    for handoff in &request.handoff {
        writeln!(out, "  --- handoff").unwrap();
        for line in handoff.lines() {
            writeln!(out, "  {line}").unwrap();
        }
        writeln!(out, "  ---").unwrap();
    }
    for (run, ended) in &request.runs {
        writeln!(out, "  run {run:x}{}", if *ended { " (ended)" } else { "" }).unwrap();
    }
    for outcome in &request.outcomes {
        write!(out, "  {}", outcome_name(outcome.kind)).unwrap();
        if let Some(at) = outcome.at.first() {
            write!(out, " at {}", crate::care::format_time(*at)).unwrap();
        }
        if !outcome.run.is_empty() {
            write!(out, ", run {}", ids(&outcome.run)).unwrap();
        }
        for reason in &outcome.reason {
            write!(out, ": {reason}").unwrap();
        }
        out.push('\n');
    }
    if request.outcomes.is_empty() {
        writeln!(out, "  no outcome recorded").unwrap();
    }
}

/// A spawn with every run and past request; `with_brief` adds the brief
/// itself.
pub fn render_show(spawn: &SpawnView, with_brief: bool) -> String {
    let mut out = String::new();
    writeln!(out, "spawn: {} ({:x})", spawn.name, spawn.id).unwrap();
    if spawn.names.len() > 1 {
        writeln!(
            out,
            "name conflict: hosts named this zooid apart; it answers to each of {}",
            spawn.names.join(", ")
        )
        .unwrap();
    }
    if !spawn.origin_colony.is_empty() {
        writeln!(out, "origin colony: {}", ids(&spawn.origin_colony)).unwrap();
    }
    writeln!(out, "brief: {}", super::handle_text(spawn.brief)).unwrap();
    if !spawn.goal.is_empty() {
        writeln!(out, "goal: {}", ids(&spawn.goal)).unwrap();
        writeln!(out, "goal revision: {}", ids(&spawn.goal_revision)).unwrap();
    }
    spawn.declared.show(&mut out);
    for run in &spawn.runs {
        writeln!(out, "\nrun {:x}", run.id).unwrap();
        if let Some(started) = run.started {
            writeln!(out, "  started: {}", crate::care::format_time(started)).unwrap();
        }
        writeln!(
            out,
            "  finished: {}",
            join(
                &run.finished
                    .iter()
                    .map(|at| crate::care::format_time(*at))
                    .collect::<Vec<_>>()
            )
        )
        .unwrap();
        writeln!(out, "  end: {}", describe_end(run)).unwrap();
        if !run.result.is_empty() {
            writeln!(out, "  systemd result: {}", join(&run.result)).unwrap();
        }
        writeln!(out, "  unit: {}", join(&run.unit)).unwrap();
        writeln!(out, "  workdir: {}", join(&run.workdir)).unwrap();
        writeln!(out, "  effort: {}", join(&run.effort)).unwrap();
        writeln!(out, "  provider: {}", join(&run.provider)).unwrap();
        if !run.provider_reason.is_empty() {
            writeln!(out, "  provider reason: {}", join(&run.provider_reason)).unwrap();
        }
        if !run.model.is_empty() {
            writeln!(out, "  model: {}", join(&run.model)).unwrap();
        }
        for why in &run.model_unset {
            writeln!(out, "  model: none named; {why}").unwrap();
        }
        for why in &run.untinted {
            writeln!(out, "  untinted recollection: {why}").unwrap();
        }
        for why in &run.lexical_fallback {
            writeln!(
                out,
                "  semantic tint unavailable (used the lexical cue): {why}"
            )
            .unwrap();
        }
        if !run.resumes.is_empty() {
            writeln!(out, "  continues archived node: {}", ids(&run.resumes)).unwrap();
        }
        if !run.realises.is_empty() {
            writeln!(out, "  realises declaration: {}", ids(&run.realises)).unwrap();
        }
        for sizing in &run.cover_sizing {
            writeln!(out, "  cover sizing: {sizing}").unwrap();
        }
        if !run.context_window.is_empty() {
            writeln!(
                out,
                "  context window reported: {} tokens",
                join(&run.context_window)
            )
            .unwrap();
        }
        if !run.first_call_input_tokens.is_empty() {
            writeln!(
                out,
                "  first call input: {} tokens (overhead, prompt and cover together)",
                join(&run.first_call_input_tokens)
            )
            .unwrap();
        }
        if !run.source_locator.is_empty() {
            writeln!(out, "  conversation: {}", run.source_locator.join(", ")).unwrap();
        }
        if !run.input_tokens.is_empty() || !run.output_tokens.is_empty() {
            writeln!(
                out,
                "  tokens: {} in ({} cached), {} out",
                join(&run.input_tokens),
                join(&run.cached_input_tokens),
                join(&run.output_tokens)
            )
            .unwrap();
        }
        if !run.cost_usd.is_empty() {
            let costs = run
                .cost_usd
                .iter()
                .map(|cost| format!("{cost:.4}"))
                .collect::<Vec<_>>();
            writeln!(
                out,
                "  cost: ${} (the agent's estimate at configured prices, not the bill)",
                costs.join(", $")
            )
            .unwrap();
        }
        if !run.goal_note.is_empty() {
            writeln!(out, "  goal note: {}", ids(&run.goal_note)).unwrap();
        }
        if !run.request.is_empty() {
            writeln!(out, "  request: {}", ids(&run.request)).unwrap();
        }
        writeln!(out, "  message: {}", ids(&run.message)).unwrap();
        for why in &run.settle_failure {
            writeln!(out, "  settlement failure: {why}").unwrap();
        }
        writeln!(out, "  persona: {}", ids(&run.persona)).unwrap();
        writeln!(out, "  spawner: {}", ids(&run.spawner)).unwrap();
        let handles = |values: &[TextHandle]| {
            join(
                &values
                    .iter()
                    .map(|h| super::handle_text(*h))
                    .collect::<Vec<_>>(),
            )
        };
        writeln!(out, "  prompt: {}", handles(&run.prompt)).unwrap();
        writeln!(out, "  report: {}", handles(&run.report)).unwrap();
        if let Some(report) = &run.report_text {
            writeln!(out, "  --- report").unwrap();
            for line in report.lines() {
                writeln!(out, "  {line}").unwrap();
            }
        }
    }
    if spawn.runs.is_empty() {
        writeln!(out, "\nno runs").unwrap();
    }
    for request in &spawn.requests {
        show_request(&mut out, request);
    }
    if with_brief {
        writeln!(out, "\n--- brief").unwrap();
        out.push_str(spawn.brief_text.as_deref().unwrap_or("(brief not here)"));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::initialize_signer;
    use std::fs::File;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeLauncher {
        started: Mutex<Vec<UnitSpec>>,
        state: Mutex<String>,
        refuse: bool,
    }

    impl Launcher for FakeLauncher {
        fn start(&self, unit: &UnitSpec) -> Result<()> {
            if self.refuse {
                bail!("fake systemd refuses");
            }
            self.started.lock().unwrap().push(unit.clone());
            *self.state.lock().unwrap() = "active".to_owned();
            Ok(())
        }
        fn state(&self, _unit: &str) -> Result<String> {
            Ok(self.state.lock().unwrap().clone())
        }
    }

    #[derive(Clone, Default)]
    struct StubRenderer {
        /// Refuse a tint the way WeMM refuses an over-long query.
        refuse_tint: bool,
        /// Panic in the tint, the way CubeCL does when a device allocation
        /// does not fit.
        panic_tint: bool,
        /// Fail every wake, as an unreadable pile would.
        refuse_wake: bool,
        /// Render the memory part as exactly its budget of characters, the
        /// way a full cover fills it.
        fill: bool,
        /// Beliefs and goals: appended whole to every wake.
        fixed: String,
        /// The memory budget of every wake, in order.
        calls: Arc<Mutex<Vec<usize>>>,
    }

    impl Renderer for StubRenderer {
        fn tint(&self, _storage: &Storage, about: &str) -> Result<WakeTint> {
            if self.refuse_tint {
                bail!("WeMM text preparation: too long");
            }
            if self.panic_tint {
                panic!("CUDA allocation: BufferTooBig");
            }
            Ok(WakeTint {
                about: about.to_owned(),
                scores: None,
                ..Default::default()
            })
        }
        fn wake(
            &self,
            _storage: &Storage,
            tint: Option<&WakeTint>,
            memory_chars: usize,
        ) -> Result<String> {
            self.calls.lock().unwrap().push(memory_chars);
            if self.refuse_wake {
                bail!("read selected Memory collection: unreadable");
            }
            let memory = match tint {
                _ if self.fill => "m".repeat(memory_chars),
                Some(tint) => format!("STUB-WAKE about {}", tint.about),
                None => "STUB-WAKE untinted".to_owned(),
            };
            Ok(format!("{memory}{}", self.fixed))
        }
    }

    struct Fixture {
        directory: tempfile::TempDir,
        storage: Storage,
        launcher: Arc<FakeLauncher>,
        spawn: Spawn,
    }

    fn fixture_with(launcher: FakeLauncher) -> Fixture {
        fixture_full(
            launcher,
            StubRenderer::default(),
            "export TRIBLESPACE_PEERS=none\n",
        )
    }

    fn fixture_full(launcher: FakeLauncher, renderer: StubRenderer, colony_env: &str) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("spawn.pile");
        let key = directory.path().join("spawn.key");
        File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        let storage = Storage::new(pile, Some(key));
        let env = directory.path().join("colony.env");
        std::fs::write(&env, colony_env).unwrap();
        std::fs::create_dir(directory.path().join("lane")).unwrap();
        let codex_auth = directory.path().join("codex-auth.json");
        std::fs::write(&codex_auth, "fake login\n").unwrap();
        let faculties = directory.path().join("bin").join("faculties");
        std::fs::create_dir(directory.path().join("bin")).unwrap();
        std::fs::write(&faculties, "#!/bin/sh\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&faculties, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let launch = LaunchConfig {
            state_dir: directory.path().join("state"),
            claude: PathBuf::from("/fake/claude"),
            model: "claude-opus-5-5".to_owned(),
            codex: PathBuf::from("/fake/codex"),
            codex_model: None,
            codex_auth: Some(codex_auth),
            vibe: PathBuf::from("/fake/vibe"),
            mistral_model: "mistral-large-4".to_owned(),
            mistral_key_secret: "c2c7b31676a61315928103941b04d487".to_owned(),
            mistral_max_price: 2.0,
            secrets: "secrets".to_owned(),
            colony_env: Some(env),
            path: format!("/usr/bin:/bin:{}", directory.path().join("bin").display()),
            memory_max: "24G".to_owned(),
            spawn: "zooid".to_owned(),
            faculties: PathBuf::from("faculties"),
            local: None,
            local_max_runtime: None,
            codex_models: Some(directory.path().join("codex-models.json")),
            claude_projects: Some(directory.path().join("claude-projects")),
        };
        let launcher = Arc::new(launcher);
        let spawn = Spawn::new(storage.clone(), launch)
            .with_launcher(launcher.clone())
            .with_renderer(Arc::new(renderer));
        Relations::with_storage(storage.clone())
            .add(
                ProfileInput {
                    label: "agent-cc".to_owned(),
                    affinities: vec!["zooid".to_owned()],
                    ..Default::default()
                },
                None,
                &[],
            )
            .unwrap();
        Fixture {
            directory,
            storage,
            launcher,
            spawn,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(FakeLauncher::default())
    }

    impl Fixture {
        fn request(&self, brief: &str) -> RunRequest {
            RunRequest {
                brief: brief.to_owned(),
                workdir: self.directory.path().join("lane"),
                effort: "high".to_owned(),
                spawner: "agent-cc".to_owned(),
                provider: Provider::Claude,
                cover: CoverWish::default(),
            }
        }
        fn settle(&self, name: &str, launched: &Launched) -> Result<Settled> {
            self.spawn.settle(&SettleRequest {
                name: name.to_owned(),
                run: launched.run,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: Some(launched.report.clone()),
                usage: None,
            })
        }
        fn inbox(&self) -> Vec<crate::message::MessageText> {
            crate::message::Message::with_storage(self.storage.clone())
                .list(&crate::message::ListOptions::new("agent-cc"))
                .unwrap()
                .entries
                .into_iter()
                .filter(|entry| entry.incoming)
                .map(|entry| entry.body)
                .collect()
        }
        /// The settlement message a zooid sent the spawner.
        fn told(&self, name: &str) -> String {
            self.inbox()
                .into_iter()
                .find_map(|body| match body {
                    crate::message::MessageText::Text(text)
                        if text.starts_with(&format!("{name} finished")) =>
                    {
                        Some(text)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} told the spawner nothing"))
        }
        fn script(&self, index: usize) -> String {
            std::fs::read_to_string(&self.launcher.started.lock().unwrap()[index].script).unwrap()
        }
        /// The same fixture with its launch configuration changed.
        fn relaunch(&self, change: impl FnOnce(&mut LaunchConfig)) -> Spawn {
            let mut launch = self.spawn.launch.clone();
            change(&mut launch);
            Spawn::new(self.storage.clone(), launch)
                .with_launcher(self.launcher.clone())
                .with_renderer(Arc::new(StubRenderer::default()))
        }
        fn persona(&self, label: &str) -> Option<crate::relations::PersonState> {
            Relations::with_storage(self.storage.clone())
                .person_state(label)
                .unwrap()
        }
    }

    fn other_host() -> HostKey {
        ed25519_dalek::SigningKey::from_bytes(&[42; 32])
            .verifying_key()
            .to_inline()
    }

    /// One request as the removed queue wrote it: what it asked for is its
    /// core, the label and the asker annotate it. Returns the facts and the
    /// request.
    fn past_request(
        zooid: Id,
        host: HostKey,
        provider: &str,
        previous: Option<Id>,
        handoff: Option<&str>,
        asked: f64,
        asker: Id,
    ) -> (Fragment, Id) {
        let mut facts = Fragment::empty();
        let provider = facts.put::<blobencodings::UTF8String, _>(provider.to_owned());
        let effort = facts.put::<blobencodings::UTF8String, _>("medium".to_owned());
        let handoff =
            handoff.map(|text| facts.put::<blobencodings::UTF8String, _>(text.to_owned()));
        let core = entity! { _ @
            metadata::tag: KIND_REQUEST,
            attrs::zooid: zooid,
            attrs::host: host,
            attrs::provider: provider,
            attrs::effort: effort,
            attrs::previous?: previous,
            attrs::handoff?: handoff,
            metadata::created_at: crate::clock::point(Epoch::from_unix_seconds(asked)).unwrap(),
        };
        let id = core.root().unwrap();
        facts += core;
        let label = facts.put::<blobencodings::UTF8String, _>("testhost".to_owned());
        facts += entity! { ExclusiveId::force_ref(&id) @
            archive_attrs::author: asker,
            attrs::host_label: label,
        };
        (facts, id)
    }

    /// One outcome as the removed keeper recorded it.
    fn past_outcome(
        request: Id,
        kind: Id,
        at: f64,
        run: Option<Id>,
        reason: Option<&str>,
    ) -> Fragment {
        let mut facts = Fragment::empty();
        let core = entity! { _ @
            metadata::tag: KIND_OUTCOME,
            attrs::request: request,
            attrs::outcome: kind,
        };
        let id = core.root().unwrap();
        facts += core;
        let unit = facts.put::<blobencodings::UTF8String, _>(format!("spawn-req-{request:x}"));
        let reason = reason.map(|text| facts.put::<blobencodings::UTF8String, _>(text.to_owned()));
        facts += entity! { ExclusiveId::force_ref(&id) @
            metadata::created_at: crate::clock::point(Epoch::from_unix_seconds(at)).unwrap(),
            attrs::outcome_run?: run,
            attrs::unit: unit,
            attrs::reason?: reason,
        };
        facts
    }

    /// The zooid's record, as a request for a new brief carried it.
    fn past_zooid(brief: &str, goal: Option<(Id, Id)>) -> (Fragment, Id, String) {
        let (mut facts, zooid, _, name) = super::super::spawn_fragment(brief).unwrap();
        if let Some((goal, revision)) = goal {
            facts += super::super::goal_fragment(zooid, goal, &[revision]);
        }
        (facts, zooid, name)
    }

    fn byte_id(byte: u8) -> Id {
        Id::new([byte; 16]).unwrap()
    }

    /// The id the queue's own writer gave these facts, taken from the last
    /// build that had it.
    #[test]
    fn the_past_request_fixture_has_the_identity_the_queue_wrote() {
        let (_, id) = past_request(
            byte_id(1),
            Inline::new([2; 32]),
            "claude",
            Some(byte_id(3)),
            Some("h"),
            10.0,
            byte_id(4),
        );
        assert_eq!(format!("{id:x}"), "b9ade3de436243e83c480ce9e4bea691");
    }

    #[test]
    fn an_ended_request_stays_readable_with_its_run_and_its_outcomes() {
        let f = fixture();
        let asker = f.persona("agent-cc").unwrap().person;
        let (mut facts, zooid, name) =
            past_zooid("Ended long ago.", Some((byte_id(7), byte_id(8))));
        let (request, id) = past_request(
            zooid,
            other_host(),
            "codex",
            None,
            None,
            1_800_000_000.0,
            asker,
        );
        facts += request;
        let run = entity! { _ @
            metadata::tag: KIND_RUN,
            attrs::of: zooid,
            metadata::started_at: crate::clock::point(Epoch::from_unix_seconds(1_800_000_060.0)).unwrap(),
        };
        let run_id = run.root().unwrap();
        facts += run;
        facts += entity! { ExclusiveId::force_ref(&run_id) @
            attrs::request: id,
            metadata::finished_at: crate::clock::point(Epoch::from_unix_seconds(1_800_000_600.0)).unwrap(),
        };
        facts += past_outcome(id, OUTCOME_ACCEPTED, 1_800_000_050.0, None, None);
        facts += past_outcome(id, OUTCOME_RUNNING, 1_800_000_061.0, Some(run_id), None);
        facts += past_outcome(id, OUTCOME_TERMINAL, 1_800_000_601.0, Some(run_id), None);
        f.spawn.publish(facts).unwrap();

        let shown = render_show(&f.spawn.show(&name).unwrap(), false);
        for expected in [
            format!("goal: {:x}", byte_id(7)),
            format!("past request {id:x}"),
            format!("{} (testhost)", super::super::host_text(other_host())),
            format!("run {run_id:x} (ended)"),
            "accepted at 2027-01-15T08:00:50Z".to_owned(),
            format!("running at 2027-01-15T08:01:01Z, run {run_id:x}"),
            format!("task-terminal at 2027-01-15T08:10:01Z, run {run_id:x}"),
        ] {
            assert!(
                shown.contains(&expected),
                "missing {expected:?} in\n{shown}"
            );
        }
    }

    #[test]
    fn a_refused_request_stays_readable_with_its_reason_and_its_handoff() {
        let f = fixture();
        let asker = f.persona("agent-cc").unwrap().person;
        let (mut facts, zooid, name) = past_zooid("Refused long ago.", None);
        let (request, id) = past_request(
            zooid,
            other_host(),
            "claude",
            Some(byte_id(9)),
            Some("Open: the review thread."),
            1_800_000_000.0,
            asker,
        );
        facts += request;
        facts += past_outcome(
            id,
            OUTCOME_REFUSED,
            1_800_000_050.0,
            None,
            Some("a handoff needs the disclosure check"),
        );
        f.spawn.publish(facts).unwrap();

        let shown = render_show(&f.spawn.show(&name).unwrap(), false);
        for expected in [
            format!("past request {id:x}"),
            format!("{} (testhost)", super::super::host_text(other_host())),
            format!("previous run: {:x}", byte_id(9)),
            "Open: the review thread.".to_owned(),
            "refused at 2027-01-15T08:00:50Z: a handoff needs the disclosure check".to_owned(),
            "no runs".to_owned(),
        ] {
            assert!(
                shown.contains(&expected),
                "missing {expected:?} in\n{shown}"
            );
        }
    }

    #[test]
    fn a_request_nobody_answered_is_history_and_not_work_waiting() {
        let f = fixture();
        let asker = f.persona("agent-cc").unwrap().person;
        let (mut facts, zooid, name) = past_zooid("Never started.", Some((byte_id(7), byte_id(8))));
        let (request, id) = past_request(
            zooid,
            other_host(),
            "claude",
            None,
            None,
            1_800_000_000.0,
            asker,
        );
        facts += request;
        f.spawn.publish(facts).unwrap();

        let shown = render_show(&f.spawn.show(&name).unwrap(), false);
        for expected in [
            format!("goal: {:x}", byte_id(7)),
            format!("past request {id:x}"),
            format!("{} (testhost)", super::super::host_text(other_host())),
            "no outcome recorded".to_owned(),
            "no runs".to_owned(),
        ] {
            assert!(
                shown.contains(&expected),
                "missing {expected:?} in\n{shown}"
            );
        }
        assert!(
            render_list(&f.spawn.list().unwrap()).contains(&format!("{name}  runs 0")),
            "the zooid is still listed"
        );
        let colony = super::super::render_colony(
            &f.spawn
                .colony(&super::super::ColonyOptions::default())
                .unwrap(),
        );
        assert!(
            !colony.contains(&format!("{id:x}")) && !colony.contains("request"),
            "an old request is not work waiting:\n{colony}"
        );
    }

    /// A fake local executor in the fixture's directory: it reads the prompt
    /// on stdin, leaves the words it was given beside the report, and then
    /// does what `body` says with the report path in `$1`.
    fn local_executor(f: &Fixture, file: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = f.directory.path().join(file);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nwc -c > \"$1.stdin\"\necho \"$@\" > \"$1.args\"\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Run a launched zooid's unit script as systemd would, without systemd,
    /// and settle it with the exit it had.
    fn run_unit_and_settle(f: &Fixture, spawn: &Spawn, launched: &Launched) -> RunView {
        let status = std::process::Command::new("/bin/bash")
            .arg(launched.report.with_extension("sh"))
            .current_dir(f.directory.path().join("lane"))
            .env_clear()
            .env("HOME", f.directory.path())
            .status()
            .unwrap();
        let code = status.code().unwrap().to_string();
        let result = if status.success() {
            "success"
        } else {
            "exit-code"
        };
        spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit: Exit::from_systemd("exited", &code, result),
                report: Some(launched.report.clone()),
                usage: None,
            })
            .unwrap();
        spawn.show(&launched.name).unwrap().runs.remove(0)
    }

    fn local_request(f: &Fixture, brief: &str) -> RunRequest {
        let mut request = f.request(brief);
        request.provider = Provider::Local;
        request
    }

    /// A local run launched with a trivial executor whose template names the
    /// message file, settled directly with `exit`. `message` is what the
    /// message file holds at settlement; `None` leaves no file at all.
    fn settle_local(
        f: &Fixture,
        brief: &str,
        exit: Exit,
        message: Option<&[u8]>,
    ) -> (Spawn, Launched, Settled) {
        let executor = local_executor(f, "local-settle", "true");
        let spawn = f.relaunch(|launch| {
            launch.local =
                Some(parse_local_executor(&format!("{} {{message}}", executor.display())).unwrap())
        });
        let launched = spawn.run(&local_request(f, brief)).unwrap();
        if let Some(bytes) = message {
            std::fs::write(&launched.report, bytes).unwrap();
        }
        let settled = spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit,
                report: Some(launched.report.clone()),
                usage: None,
            })
            .unwrap();
        (spawn, launched, settled)
    }

    /// The newest run of a launched zooid.
    fn newest_run(spawn: &Spawn, launched: &Launched) -> RunView {
        spawn.show(&launched.name).unwrap().runs.pop().unwrap()
    }

    #[test]
    fn a_local_run_that_leaves_no_message_file_ends_by_its_exit_with_no_message() {
        let f = fixture();
        let exit = Exit::from_systemd("exited", "0", "success");
        let (spawn, launched, settled) = settle_local(&f, "No message.", exit, None);
        assert!(!launched.report.exists());
        assert_eq!(settled.report, None);
        let run = newest_run(&spawn, &launched);
        assert_eq!(run.exit_code, [0]);
        assert_eq!(run.result, ["success"], "only systemd's own result");
        assert!(run.report.is_empty());
        assert!(run.report_text.is_none());
        assert!(run.settle_failure.is_empty(), "{:?}", run.settle_failure);
        assert!(ended_clean(&run, true));
        assert_eq!(describe_end(&run), "exit 0");
        let told = f.told(&launched.name);
        assert!(
            told.contains("finished, exit 0, no closing message."),
            "{told}"
        );
        assert!(!told.contains("report"), "{told}");
        assert!(f.persona(&launched.name).unwrap().retired);
    }

    #[test]
    fn an_empty_message_file_is_no_message_and_no_failure() {
        let f = fixture();
        let exit = Exit::from_systemd("exited", "0", "success");
        let (spawn, launched, settled) = settle_local(&f, "Empty message.", exit, Some(&b""[..]));
        assert!(launched.report.exists());
        assert_eq!(settled.report, None);
        let run = newest_run(&spawn, &launched);
        assert!(run.report.is_empty());
        assert_eq!(run.result, ["success"]);
        assert!(ended_clean(&run, true));
        assert_eq!(describe_end(&run), "exit 0");
        let told = f.told(&launched.name);
        assert!(told.contains("no closing message"), "{told}");
    }

    #[test]
    fn a_closing_message_is_recorded_byte_for_byte_and_shown_to_the_spawner() {
        let f = fixture();
        // Leading and trailing whitespace, blank lines and non-ASCII text
        // all survive: nothing is trimmed or normalized.
        let words = "  done.\n\nNotes are on the goal. \u{00fc}ber \u{2713}\n\n";
        let exit = Exit::from_systemd("exited", "0", "success");
        let (spawn, launched, settled) =
            settle_local(&f, "With a message.", exit, Some(words.as_bytes()));
        let run = newest_run(&spawn, &launched);
        assert_eq!(run.report_text.as_deref(), Some(words));
        assert_eq!(run.report.len(), 1);
        assert_eq!(settled.report, Some(run.report[0]));
        assert_eq!(run.result, ["success"]);
        assert!(ended_clean(&run, true));
        let told = f.told(&launched.name);
        assert!(
            told.contains(&format!(
                "finished, exit 0, closing message {}.",
                super::super::handle_text(run.report[0])
            )),
            "{told}"
        );
        assert!(!told.contains("partial"), "{told}");
    }

    #[test]
    fn a_local_run_that_exits_badly_fails_by_its_exit_and_keeps_its_partial_words() {
        let f = fixture();
        let exit = Exit::from_systemd("exited", "3", "exit-code");
        let (spawn, launched, _) = settle_local(&f, "Fails.", exit, Some(&b"half\n"[..]));
        let run = newest_run(&spawn, &launched);
        assert_eq!(run.exit_code, [3]);
        assert!(!ended_clean(&run, true));
        assert_eq!(describe_end(&run), "exit 3");
        assert_eq!(run.report_text.as_deref(), Some("half\n"));
        assert_eq!(run.result, ["exit-code"]);
        let told = f.told(&launched.name);
        assert!(
            told.contains("finished, exit 3, closing message blake3:"),
            "{told}"
        );
        assert!(
            told.contains("(partial: the run did not end cleanly)"),
            "{told}"
        );
    }

    #[test]
    fn a_local_run_that_ends_cleanly_on_its_runtime_limit_is_noted_not_failed() {
        let f = fixture();
        let exit = Exit::from_systemd("exited", "0", super::super::TIMEOUT_RESULT);
        let (spawn, launched, _) = settle_local(&f, "Timed out.", exit, Some(&b"almost\n"[..]));
        let run = newest_run(&spawn, &launched);
        assert_eq!(run.result, [super::super::TIMEOUT_RESULT]);
        // The limit interrupts her (SIGINT) and exit 0 is Drive's teardown.
        assert!(
            ended_clean(&run, true),
            "exit 0 after the limit's interrupt is her ordinary end"
        );
        assert!(
            !ended_clean(&run, false),
            "a hosted run on its limit did not finish"
        );
        assert_eq!(
            describe_end(&run),
            format!("exit 0, {}", super::super::RUNTIME_LIMIT_NOTE)
        );
        assert_eq!(run.report_text.as_deref(), Some("almost\n"));
        let told = f.told(&launched.name);
        assert!(told.contains(super::super::RUNTIME_LIMIT_NOTE), "{told}");
        assert!(
            !told.contains("(partial: the run did not end cleanly)"),
            "{told}"
        );
    }

    #[test]
    fn settling_a_local_run_again_changes_nothing_it_recorded() {
        let f = fixture();
        let exit = Exit::from_systemd("exited", "0", "success");
        let (spawn, launched, first) = settle_local(&f, "Twice.", exit, Some(&b"done"[..]));
        assert!(!first.already_ended && !first.already_messaged);
        let before = newest_run(&spawn, &launched);
        // The second settlement reads the earlier one back: what the file
        // says now plays no part, and neither does a different exit.
        std::fs::write(&launched.report, "something else").unwrap();
        let again = spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit: Exit::from_systemd("exited", "1", "exit-code"),
                report: Some(launched.report.clone()),
                usage: None,
            })
            .unwrap();
        assert!(again.already_ended && again.already_messaged);
        assert_eq!(again.message, first.message);
        assert_eq!(again.report, first.report);
        let after = newest_run(&spawn, &launched);
        assert_eq!(after.finished, before.finished);
        assert_eq!(after.exit_code, [0]);
        assert_eq!(after.result, ["success"]);
        assert_eq!(after.report, before.report);
        assert_eq!(after.report_text.as_deref(), Some("done"));
        assert_eq!(after.message, before.message);
        assert!(ended_clean(&after, true));
        assert_eq!(f.inbox().len(), 1);
    }

    #[test]
    fn a_hosted_run_that_exits_0_without_a_report_keeps_its_meaning() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Hosted, no report.")).unwrap();
        f.spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: Some(launched.report.clone()),
                usage: None,
            })
            .unwrap();
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert!(run.report.is_empty());
        assert!(ended_clean(run, true));
        assert_eq!(describe_end(run), "exit 0");
    }

    #[test]
    fn a_local_executor_gets_the_brief_without_the_wake_and_who_sent_it() {
        let f = fixture();
        let executor = local_executor(
            &f,
            "local-brief",
            r#"printf %s "$SPAWN_BRIEF_TOPIC" > "$1.topic""#,
        );
        let spawn = f.relaunch(|launch| {
            launch.local = Some(
                parse_local_executor(&format!(
                    "{} {{message}} --brief {{brief}} --spawner {{spawner}} --name {{name}}",
                    executor.display()
                ))
                .unwrap(),
            )
        });
        // Preserve shell metacharacters and end on a UTF-8 boundary.
        let task = format!("Check '$HOME' and $(echo x) 🐈\n{}", "é".repeat(160));
        let launched = spawn.run(&local_request(&f, &task)).unwrap();
        run_unit_and_settle(&f, &spawn, &launched);
        let brief = launched.report.with_extension("brief");
        let args = std::fs::read_to_string(launched.report.with_extension("report.args")).unwrap();
        assert_eq!(
            args.trim(),
            format!(
                "{} --brief {} --spawner agent-cc --name {}",
                launched.report.display(),
                brief.display(),
                launched.name
            )
        );
        // Who she is and the brief, exactly; the wake only in the prompt.
        let text = std::fs::read_to_string(&brief).unwrap();
        assert_eq!(
            text,
            super::super::assemble_brief(
                &super::super::preamble(&launched.name, "agent-cc", true),
                &task
            )
        );
        let (assigned, handle, topic, workspace): (PathBuf, String, String, Option<String>) =
            serde_json::from_slice(
                &std::fs::read(launched.report.with_extension("report.topic")).unwrap(),
            )
            .unwrap();
        assert_eq!(assigned, brief);
        assert_eq!(workspace, None);
        assert_eq!(
            handle,
            super::super::handle_text(super::super::text_handle(&text))
        );
        assert_eq!(topic, super::super::about(&task));
        assert!(topic.len() < task.len());
        let prompt = std::fs::read_to_string(launched.report.with_extension("prompt")).unwrap();
        assert!(prompt.starts_with(&format!("# Wake\n\nSTUB-WAKE about {topic}\n")));
        assert!(
            prompt.starts_with("# Wake\n\n") && prompt.ends_with(&text),
            "{prompt}"
        );
        assert!(!text.contains("# Wake"));
        // And a local unit is stopped by interrupting it, a hosted one is not.
        spawn.run(&f.request("Hosted.")).unwrap();
        let started = f.launcher.started.lock().unwrap().clone();
        let stop = |spec: &UnitSpec| {
            let args = spec.systemd_run_args();
            ["KillSignal=SIGINT", "KillMode=mixed"]
                .map(|property| args.iter().any(|arg| arg == property))
        };
        assert_eq!(stop(&started[0]), [true, true]);
        assert_eq!(stop(&started[1]), [false, false]);
        // And only a local unit delegates a cgroup for its shell sandbox,
        // each property after its own -p and before the command.
        let delegation = |spec: &UnitSpec| {
            let args = spec.systemd_run_args();
            let command = args.iter().position(|arg| arg == "--").unwrap();
            ["Delegate=cpu memory pids", "DelegateSubgroup=supervisor"].map(|property| {
                args.iter()
                    .position(|arg| arg == property)
                    .is_some_and(|at| at < command && args[at - 1] == "-p")
            })
        };
        assert_eq!(delegation(&started[0]), [true, true]);
        assert_eq!(delegation(&started[1]), [false, false]);
    }

    #[test]
    fn a_local_zooid_runs_the_hosts_executor_and_its_message_file_is_the_closing_message() {
        let f = fixture();
        let executor = local_executor(&f, "local-ok", "printf 'local words\\n' > \"$1\"");
        let spawn = f.relaunch(|launch| {
            launch.local = Some(
                parse_local_executor(&format!(
                    "{} {{message}} --name={{name}} --effort {{effort}} --prompt {{prompt}} \
                     --mcp {{script}}",
                    executor.display()
                ))
                .unwrap(),
            )
        });
        let launched = spawn.run(&local_request(&f, "Local work.")).unwrap();
        assert_eq!(launched.provider, Provider::Local);
        assert_eq!(launched.model, None);
        assert!(launched
            .model_unset
            .as_deref()
            .unwrap()
            .contains("SPAWN_LOCAL_EXECUTOR"));
        let script = f.script(0);
        assert!(!script.contains("claude"), "{script}");
        // The message is the executor's file, never its stdout.
        assert!(
            !script.contains(&format!("> {}", launched.report.display())),
            "{script}"
        );

        let run = run_unit_and_settle(&f, &spawn, &launched);
        assert_eq!(run.exit_code, [0]);
        assert_eq!(run.report_text.as_deref(), Some("local words\n"));
        let args = std::fs::read_to_string(launched.report.with_extension("report.args")).unwrap();
        let sh = launched.report.with_extension("sh");
        let prompt = launched.report.with_extension("prompt");
        assert_eq!(
            args.trim(),
            format!(
                "{} --name={} --effort high --prompt {} --mcp {}",
                launched.report.display(),
                launched.name,
                prompt.display(),
                sh.display()
            )
        );
        // The prompt arrived on stdin too, whole.
        let stdin =
            std::fs::read_to_string(launched.report.with_extension("report.stdin")).unwrap();
        assert_eq!(
            stdin.trim().parse::<u64>().unwrap(),
            std::fs::metadata(&prompt).unwrap().len()
        );
        let told = f.told(&launched.name);
        assert!(
            told.contains("finished, exit 0, closing message blake3:"),
            "{told}"
        );
        assert!(!told.contains("partial"), "{told}");
    }

    #[test]
    fn a_local_executor_that_fails_is_recorded_as_failed_and_nothing_else_runs() {
        let f = fixture();
        let executor = local_executor(&f, "local-fail", "echo 'out of memory' > \"$1\"; exit 3");
        let spawn = f.relaunch(|launch| {
            launch.local =
                Some(parse_local_executor(&format!("{} {{message}}", executor.display())).unwrap())
        });
        let launched = spawn
            .run(&local_request(&f, "Failing local work."))
            .unwrap();
        let run = run_unit_and_settle(&f, &spawn, &launched);
        assert_eq!(run.exit_code, [3]);
        assert_eq!(describe_end(&run), "exit 3");
        assert_eq!(run.report_text.as_deref(), Some("out of memory\n"));
        // One unit, the local one; no hosted retry.
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
        assert!(!f.script(0).contains("/fake/"));
        assert_eq!(spawn.show(&launched.name).unwrap().runs.len(), 1);
    }

    #[test]
    fn a_local_executor_that_writes_no_message_settles_clean_with_none() {
        let f = fixture();
        let executor = local_executor(&f, "local-silent", "echo 'stdout is not the message'");
        let spawn = f.relaunch(|launch| {
            launch.local =
                Some(parse_local_executor(&format!("{} {{message}}", executor.display())).unwrap())
        });
        let launched = spawn.run(&local_request(&f, "Silent local work.")).unwrap();
        let run = run_unit_and_settle(&f, &spawn, &launched);
        assert_eq!(run.exit_code, [0]);
        assert!(run.report.is_empty());
        assert!(run.report_text.is_none());
        assert!(ended_clean(&run, true));
        let told = f.told(&launched.name);
        assert!(told.contains("no closing message"), "{told}");
    }

    #[test]
    fn a_local_template_gets_the_runtime_limit_it_names() {
        let f = fixture();
        let executor = local_executor(&f, "local-runtime", "printf 'ok' > \"$1\"");
        let spawn = f.relaunch(|launch| {
            launch.local = Some(
                parse_local_executor(&format!(
                    "{} {{message}} --runtime {{runtime}} --deadline-of={{runtime}}s",
                    executor.display()
                ))
                .unwrap(),
            );
            launch.local_max_runtime = Some(90);
        });
        let launched = spawn.run(&local_request(&f, "Runtime named.")).unwrap();
        assert_eq!(f.launcher.started.lock().unwrap()[0].runtime_max, Some(90));
        let run = run_unit_and_settle(&f, &spawn, &launched);
        assert_eq!(run.exit_code, [0]);
        let args = std::fs::read_to_string(launched.report.with_extension("report.args")).unwrap();
        assert_eq!(
            args.trim(),
            format!(
                "{} --runtime 90 --deadline-of=90s",
                launched.report.display()
            )
        );
    }

    /// Publish archive facts into the fixture's archive collection, as a
    /// native Drive writes them.
    fn publish_archive(f: &Fixture, facts: Fragment) {
        f.storage
            .with_store(|pile, signer, runtime| {
                let collection = f.storage.open_collection_write(
                    pile,
                    crate::schemas::blockdag::DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                    runtime,
                )?;
                pile.commit(collection, signer, facts)?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_local_template_naming_resume_continues_the_zooids_archived_history() {
        use crate::spawn::continuation::tests::{ctx, Archive};
        let f = fixture();
        let spawn = f.relaunch(|launch| {
            launch.local =
                Some(parse_local_executor("/opt/drive/zooid {message} --resume={resume}").unwrap())
        });
        // Someone else's history is in the archive; this zooid has none yet.
        let mut archive = Archive::default();
        let other = ctx();
        archive.named(other, "zooid-someone-else");
        let foreign = archive.chain(None, other, 3);
        publish_archive(&f, archive.facts.clone());
        let first = spawn.run(&local_request(&f, "Lives twice.")).unwrap();
        assert_eq!(first.resumes, None);
        let script = f.script(0);
        assert!(
            script.split_whitespace().any(|word| word == "--resume="),
            "{script}"
        );
        assert!(!script.contains(&format!("{foreign:x}")), "{script}");
        assert!(newest_run(&spawn, &first).resumes.is_empty());
        spawn
            .settle(&SettleRequest {
                name: first.name.clone(),
                run: first.run,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: Some(first.report.clone()),
                usage: None,
            })
            .unwrap();

        // Its first life is archived, then it runs again.
        let mut archive = Archive::default();
        let lived = ctx();
        archive.named(lived, &first.name);
        let tail = archive.chain(None, lived, 4);
        publish_archive(&f, archive.facts);
        let again = spawn.run(&local_request(&f, "Lives twice.")).unwrap();
        assert_eq!(again.name, first.name);
        let script = f.script(1);
        let word = format!("--resume={tail:x}");
        assert!(script.split_whitespace().any(|w| w == word), "{script}");
        let view = spawn.show(&first.name).unwrap();
        let resumed: Vec<_> = view
            .runs
            .iter()
            .flat_map(|run| run.resumes.clone())
            .collect();
        assert_eq!(resumed, [tail]);
    }

    #[test]
    fn a_local_launch_whose_archive_cannot_say_where_the_zooid_ends_starts_nothing() {
        use crate::spawn::continuation::tests::{ctx, Archive};
        let f = fixture();
        let spawn = f.relaunch(|launch| {
            launch.local = Some(parse_local_executor("/opt/drive/zooid --resume={resume}").unwrap())
        });
        let first = spawn.run(&local_request(&f, "Diverged.")).unwrap();
        spawn
            .settle(&SettleRequest {
                name: first.name.clone(),
                run: first.run,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: Some(first.report.clone()),
                usage: None,
            })
            .unwrap();
        // Two contexts of the zooid whose tails neither continue the other.
        let mut archive = Archive::default();
        let (one, two) = (ctx(), ctx());
        archive.named(one, &first.name);
        archive.named(two, &first.name);
        archive.chain(None, one, 2);
        archive.chain(None, two, 2);
        publish_archive(&f, archive.facts);
        let error = format!(
            "{:#}",
            spawn.run(&local_request(&f, "Diverged.")).unwrap_err()
        );
        assert!(
            error.contains("diverges") && error.contains(&first.name),
            "{error}"
        );
        assert_eq!(
            f.launcher.started.lock().unwrap().len(),
            1,
            "nothing else started"
        );
    }

    #[test]
    fn a_template_naming_runtime_without_a_limit_starts_nothing() {
        let f = fixture();
        let spawn = f.relaunch(|launch| {
            launch.local = Some(parse_local_executor("/opt/local {message} {runtime}").unwrap());
            launch.local_max_runtime = None;
        });
        let error = spawn
            .run(&local_request(&f, "Runtime named, none set."))
            .unwrap_err()
            .to_string();
        assert!(error.contains("SPAWN_LOCAL_MAX_RUNTIME"), "{error}");
        assert!(f.launcher.started.lock().unwrap().is_empty());
    }

    #[test]
    fn only_a_local_unit_gets_the_runtime_limit() {
        let f = fixture();
        let spawn = f.relaunch(|launch| {
            launch.local = Some(parse_local_executor("/opt/local {message}").unwrap());
            launch.local_max_runtime = Some(7);
        });
        spawn.run(&f.request("Hosted, unlimited.")).unwrap();
        spawn.run(&local_request(&f, "Local, limited.")).unwrap();
        let started = f.launcher.started.lock().unwrap().clone();
        assert_eq!(started[0].runtime_max, None);
        assert!(!started[0]
            .systemd_run_args()
            .iter()
            .any(|arg| arg.starts_with("RuntimeMaxSec")));
        assert_eq!(started[1].runtime_max, Some(7));
        let args = started[1].systemd_run_args();
        let at = args
            .iter()
            .position(|arg| arg == "RuntimeMaxSec=7")
            .unwrap();
        assert_eq!(args[at - 1], "-p");
        assert!(at < args.iter().position(|arg| arg == "--").unwrap());
    }

    /// The runtime limit through real systemd: the limit interrupts a local
    /// unit's main process with SIGINT. An executor that sleeps past a
    /// two-second limit dies of it and settles as a failure; one that tears
    /// down on the interrupt, as Drive does, writes its message and exits 0,
    /// and settles as a normal end noted as the limit. Skipped where no
    /// systemd user manager answers.
    #[test]
    fn a_local_unit_is_interrupted_at_its_runtime_limit_and_a_teardown_ends_cleanly() {
        let reachable = std::process::Command::new("systemctl")
            .args(["--user", "show-environment"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !reachable {
            eprintln!("skipped: no systemd user manager here");
            return;
        }
        let f = fixture();
        // settlement's command, found on the unit's PATH: it leaves its
        // arguments for the test to settle with.
        let settle = f.directory.path().join("bin").join("zooid");
        std::fs::write(
            &settle,
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/../settle-$2.args\"\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&settle, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let sleeper = local_executor(&f, "local-sleep", "exec sleep 60");
        let catcher = local_executor(
            &f,
            "local-catch",
            "trap 'echo partial > \"$1\"; exit 0' INT\nsleep 60 &\nwait",
        );
        let real = |executor: &Path| {
            let mut launch = f.spawn.launch.clone();
            launch.local =
                Some(parse_local_executor(&format!("{} {{message}}", executor.display())).unwrap());
            launch.local_max_runtime = Some(2);
            Spawn::new(f.storage.clone(), launch).with_renderer(Arc::new(StubRenderer::default()))
        };
        // The fixture's directory makes the brief, and so the unit, unique.
        let tag = f.directory.path().display().to_string();
        let (sleeping, catching) = (real(&sleeper), real(&catcher));
        let slept = sleeping
            .run(&local_request(&f, &format!("Sleep past the limit {tag}.")))
            .unwrap();
        let caught = catching
            .run(&local_request(&f, &format!("Catch the limit {tag}.")))
            .unwrap();
        for (spawn, launched, report) in [
            (&sleeping, &slept, None),
            (&catching, &caught, Some("partial\n")),
        ] {
            let args = f
                .directory
                .path()
                .join(format!("settle-{}.args", launched.name));
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !args.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{} never settled",
                    launched.unit
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            std::thread::sleep(Duration::from_millis(200));
            let args = std::fs::read_to_string(&args).unwrap();
            let words: Vec<&str> = args.trim_end_matches('\n').split(' ').collect();
            let after = |flag: &str| {
                let at = words.iter().position(|word| *word == flag).unwrap();
                words[at + 1]
            };
            let exit = Exit::from_systemd(after("--code"), after("--status"), after("--result"));
            assert_eq!(exit.result.as_deref(), Some("timeout"), "{args}");
            assert_eq!(exit.clean(true), report.is_some(), "{args}");
            if report.is_none() {
                assert_eq!(exit.signal.as_deref(), Some("INT"), "{args}");
            }
            spawn
                .settle(&SettleRequest {
                    name: launched.name.clone(),
                    run: launched.run,
                    exit,
                    report: Some(launched.report.clone()),
                    usage: None,
                })
                .unwrap();
            let run = &spawn.show(&launched.name).unwrap().runs[0];
            assert_eq!(ended_clean(run, true), report.is_some());
            assert!(
                describe_end(run).ends_with("stopped at its runtime limit"),
                "{}",
                describe_end(run)
            );
            assert_eq!(run.report_text.as_deref(), report);
            let told = f.told(&launched.name);
            assert!(told.contains("stopped at its runtime limit"), "{told}");
            match report {
                Some(_) => assert!(
                    told.contains("closing message")
                        && !told.contains("(partial: the run did not end cleanly)"),
                    "{told}"
                ),
                None => assert!(told.contains("no closing message"), "{told}"),
            }
        }
    }

    #[test]
    fn local_executor_templates_are_read_from_the_environment_and_checked() {
        let home = Path::new("/h");
        let read = |value: &'static str| {
            let mut config = LaunchConfig::under_home(home);
            config
                .override_from(move |name: &str| {
                    (name == "SPAWN_LOCAL_EXECUTOR").then(|| value.to_owned())
                })
                .map(|()| config.local)
        };
        assert_eq!(read("   ").unwrap(), None);
        assert_eq!(
            read(" /x/run  --out={message}\t{prompt} ").unwrap(),
            Some(vec![
                "/x/run".to_owned(),
                "--out={message}".to_owned(),
                "{prompt}".to_owned()
            ])
        );
        // A closing message is optional, so naming no message file is fine.
        assert_eq!(
            read("/x/run {prompt}").unwrap(),
            Some(vec!["/x/run".to_owned(), "{prompt}".to_owned()])
        );
        for bad in [
            // The old report file is no placeholder any more.
            "/x/run {report}",
            "/x/run {message} {model}",
            "/x/run {message",
        ] {
            assert!(read(bad).is_err(), "{bad}");
        }
        assert_eq!(LaunchConfig::under_home(home).local, None);
        let limit = |value: &'static str| {
            let mut config = LaunchConfig::under_home(home);
            config
                .override_from(move |name: &str| {
                    (name == "SPAWN_LOCAL_MAX_RUNTIME").then(|| value.to_owned())
                })
                .map(|()| config.local_max_runtime)
        };
        assert_eq!(limit(" ").unwrap(), None);
        assert_eq!(limit(" 90 ").unwrap(), Some(90));
        for bad in ["0", "-5", "1.5", "an hour"] {
            assert!(limit(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn run_records_the_spawn_and_refuses_a_live_duplicate() {
        let f = fixture();
        let brief = "Build the spawn faculty.\nThen report.";
        let launched = f.spawn.run(&f.request(brief)).unwrap();
        let (_, spawn_id, handle, name) = super::super::spawn_fragment(brief).unwrap();
        assert_eq!(launched.name, name);
        assert_eq!(launched.spawn, spawn_id);
        assert_eq!(launched.brief, handle);
        assert_eq!(launched.unit, format!("spawn-{name}"));

        let started = f.launcher.started.lock().unwrap().clone();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].unit, launched.unit);
        assert_eq!(started[0].memory_max, "24G");
        let args = started[0].systemd_run_args();
        assert!(args
            .iter()
            .any(|arg| arg.starts_with("ExecStopPost=/bin/bash ")));
        assert!(args.contains(&"--collect".to_owned()));
        let script = std::fs::read_to_string(&started[0].script).unwrap();
        assert!(script.contains(&format!("--run {:x}", launched.run)));

        let persona = f.persona(&name).unwrap();
        assert!(!persona.retired);
        assert_eq!(persona.person, launched.persona);

        let view = f.spawn.show(&name).unwrap();
        assert_eq!(view.id, spawn_id);
        assert_eq!(view.brief_text.as_deref(), Some(brief));
        assert_eq!(view.runs.len(), 1);
        let run = &view.runs[0];
        assert_eq!(run.id, launched.run);
        assert!(!run.ended());
        assert_eq!(run.unit, [launched.unit.clone()]);
        assert_eq!(run.effort, ["high"]);
        assert_eq!(run.persona, [launched.persona]);
        let spawner = f.persona("agent-cc").unwrap().person;
        assert_eq!(run.spawner, [spawner]);
        assert_eq!(f.spawn.list().unwrap().len(), 1);

        let error = f.spawn.run(&f.request(brief)).unwrap_err();
        assert!(format!("{error:#}").contains("still running"), "{error:#}");
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
        assert_eq!(f.spawn.show(&name).unwrap().runs.len(), 1);
        // A different brief is a different zooid and starts beside it.
        let other = f.spawn.run(&f.request("Another task.")).unwrap();
        assert_ne!(other.name, name);
        assert_eq!(f.spawn.list().unwrap().len(), 2);
    }

    #[test]
    fn prompt_is_wake_tail_preamble_brief_in_that_order() {
        let f = fixture();
        let brief = "Brief body, verbatim.";
        let launched = f.spawn.run(&f.request(brief)).unwrap();
        let script_dir = &f.launcher.started.lock().unwrap()[0].script;
        let prompt = std::fs::read_to_string(script_dir.with_extension("prompt")).unwrap();
        let at = |needle: &str| prompt.find(needle).unwrap();
        // The stub wake quotes the brief, so the brief itself is its last occurrence.
        let brief_at = prompt.rfind(brief).unwrap();
        assert!(at("STUB-WAKE about Brief body") < at(&format!("You are {}", launched.name)));
        assert_eq!(
            prompt.matches("STUB-WAKE").count(),
            1,
            "no second memory pass"
        );
        assert!(!prompt.contains("# Recollection for this task"));
        assert!(at("spawned by agent-cc") < brief_at);
        assert!(prompt.trim_end().ends_with(brief));
        assert!(prompt.contains("Do not arm a colony watcher"));
        assert!(launched.untinted.is_none() && launched.lexical_fallback.is_none());
        let ordinary = f.spawn.show(&launched.name).unwrap();
        assert!(ordinary.runs[0].lexical_fallback.is_empty());
        // A historical run can still say that it had no cue at all.
        f.spawn.publish(entity! { ExclusiveId::force_ref(&launched.run) @ attrs::untinted: "earlier scorer error".to_owned() }).unwrap();
        let historical = f.spawn.show(&launched.name).unwrap();
        assert!(
            render_show(&historical, false).contains("untinted recollection: earlier scorer error")
        );
        assert!(historical.runs[0].lexical_fallback.is_empty());
        assert_eq!(launched.prompt_chars, prompt.chars().count());
        let recorded = f.spawn.show(&launched.name).unwrap().runs[0].prompt[0];
        assert_eq!(recorded, super::super::text_handle(&prompt));
    }

    fn covered(f: &Fixture, brief: &str, percent: f64, window: Option<u64>) -> RunRequest {
        let mut request = f.request(brief);
        request.cover = CoverWish {
            percent: Some(percent),
            window,
        };
        request
    }

    fn prompt_of(f: &Fixture, index: usize) -> String {
        let script = f.launcher.started.lock().unwrap()[index].script.clone();
        std::fs::read_to_string(script.with_extension("prompt")).unwrap()
    }

    /// A stub whose memory part is exactly its budget, beside `fixed`
    /// characters of beliefs and goals.
    fn filled(fixed: usize) -> StubRenderer {
        StubRenderer {
            fill: true,
            fixed: "b".repeat(fixed),
            ..Default::default()
        }
    }

    fn fixture_rendering(renderer: StubRenderer) -> Fixture {
        fixture_full(
            FakeLauncher::default(),
            renderer,
            "export TRIBLESPACE_PEERS=none\n",
        )
    }

    #[test]
    fn without_cover_the_prompt_is_the_plain_one_and_nothing_is_sized() {
        let renderer = StubRenderer::default();
        let calls = renderer.calls.clone();
        let f = fixture_rendering(renderer);
        let brief = "Plain brief.";
        let launched = f.spawn.run(&f.request(brief)).unwrap();
        let expected = super::super::assemble_prompt(
            &format!("STUB-WAKE about {brief}"),
            &super::super::preamble(&launched.name, "agent-cc", false),
            brief,
        );
        assert_eq!(prompt_of(&f, 0), expected);
        assert_eq!(*calls.lock().unwrap(), [PLAIN_MEMORY_CHARS]);
        assert!(launched.cover.is_none());
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert!(run.cover_sizing.is_empty());
    }

    #[test]
    fn a_percent_picks_the_memory_budget_from_the_window_the_ratio_and_the_beliefs() {
        let renderer = filled(270_000);
        let calls = renderer.calls.clone();
        let f = fixture_rendering(renderer);
        let launched = f
            .spawn
            .run(&covered(&f, "Sized brief.", 20.0, Some(1_000_000)))
            .unwrap();
        let cover = launched.cover.clone().unwrap();
        assert_eq!(cover.window, Some(1_000_000));
        assert_eq!(cover.window_source, "--window");
        // 20% of 1M tokens at 2.423 characters per token.
        assert_eq!(cover.target_chars, Some(484_600));
        assert_eq!(cover.fixed_chars, Some(270_000));
        assert_eq!(cover.memory_budget, 214_600);
        // Beliefs and goals measured with no memory budget, then one render.
        assert_eq!(*calls.lock().unwrap(), [0, 214_600]);
        assert_eq!(cover.cover_chars, 484_600);
        assert!(cover.reduction.is_none());
        assert_eq!(cover.headroom, Some(cover::HEADROOM));
        assert_eq!(cover.calibration.as_ref().unwrap().n, Some(138));
        let prompt = prompt_of(&f, 0);
        assert!(prompt.contains(&"b".repeat(270_000)));
        assert_eq!(
            cover.estimate,
            Some(cover::estimate_tokens(
                prompt.chars().count(),
                2.423,
                cover::CLAUDE_OVERHEAD_TOKENS
            ))
        );
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.cover_sizing, [cover.summary.clone()]);
        assert!(
            cover
                .summary
                .contains("journal budget 214600 and up to 40000 for Knowledge readings"),
            "{}",
            cover.summary
        );
    }

    #[test]
    fn beliefs_render_whole_and_still_leave_the_minimum_recollection() {
        let renderer = filled(600_000);
        let calls = renderer.calls.clone();
        let f = fixture_rendering(renderer);
        let launched = f
            .spawn
            .run(&covered(&f, "Heavy beliefs.", 20.0, Some(1_000_000)))
            .unwrap();
        let cover = launched.cover.unwrap();
        assert_eq!(cover.memory_budget, cover::MIN_MEMORY_CHARS);
        assert_eq!(*calls.lock().unwrap(), [0, cover::MIN_MEMORY_CHARS]);
        assert_eq!(cover.cover_chars, 600_000 + cover::MIN_MEMORY_CHARS);
        assert!(
            cover.summary.contains("over its target"),
            "{}",
            cover.summary
        );
        assert!(prompt_of(&f, 0).contains(&"b".repeat(600_000)));
    }

    #[test]
    fn an_estimate_over_the_window_renders_the_memory_part_once_more_and_replaces_it() {
        let renderer = filled(10_000);
        let calls = renderer.calls.clone();
        let f = fixture_rendering(renderer);
        let launched = f
            .spawn
            .run(&covered(&f, "Over the window.", 100.0, Some(100_000)))
            .unwrap();
        let cover = launched.cover.unwrap();
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[..2], [0, 232_300]);
        assert!(calls[2] < 232_300 && calls[2] > 0, "{calls:?}");
        assert_eq!(cover.memory_budget, calls[2]);
        assert!(cover.estimate.unwrap() <= cover::aim_tokens(100_000));
        let reduction = cover.reduction.unwrap();
        assert!(reduction.contains("rendered once more"), "{reduction}");
        assert!(!reduction.contains("launched anyway"), "{reduction}");
        // One cover reaches the zooid: the second render's.
        let prompt = prompt_of(&f, 0);
        assert!(prompt.contains(&format!("{}b", "m".repeat(calls[2]))));
        assert!(!prompt.contains(&"m".repeat(calls[2] + 1)));
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
    }

    #[test]
    fn even_an_empty_memory_part_estimated_over_the_window_launches_and_says_so() {
        let renderer = filled(400_000);
        let calls = renderer.calls.clone();
        let f = fixture_rendering(renderer);
        let launched = f
            .spawn
            .run(&covered(
                &f,
                "Beliefs past the window.",
                20.0,
                Some(100_000),
            ))
            .unwrap();
        let cover = launched.cover.unwrap();
        assert_eq!(*calls.lock().unwrap(), [0, cover::MIN_MEMORY_CHARS, 0]);
        assert_eq!(cover.memory_budget, 0);
        let reduction = cover.reduction.unwrap();
        assert!(reduction.contains("even with no memories"), "{reduction}");
        assert!(reduction.contains("launched anyway"), "{reduction}");
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
    }

    #[test]
    fn with_no_window_known_the_cover_is_the_plain_size_and_says_why() {
        let renderer = StubRenderer::default();
        let calls = renderer.calls.clone();
        let f = fixture_rendering(renderer);
        let launched = f.spawn.run(&covered(&f, "No window.", 20.0, None)).unwrap();
        assert_eq!(*calls.lock().unwrap(), [PLAIN_MEMORY_CHARS]);
        let cover = launched.cover.unwrap();
        assert_eq!(cover.window, None);
        assert_eq!(cover.estimate, None);
        assert_eq!(cover.memory_budget, PLAIN_MEMORY_CHARS);
        assert!(
            cover
                .window_source
                .contains("no settled claude run of claude-opus-5-5"),
            "{}",
            cover.window_source
        );
        assert!(cover.summary.contains("plain size"), "{}", cover.summary);
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.cover_sizing, [cover.summary]);
        // codex with no model named has no catalog entry to read.
        let mut codex = covered(&f, "No codex window.", 20.0, None);
        codex.provider = Provider::Codex;
        let cover = f.relaunch(|_| {}).run(&codex).unwrap().cover.unwrap();
        assert!(
            cover.window_source.contains("no codex model is named"),
            "{}",
            cover.window_source
        );
        // A local run takes --window only; mistral's is spawn's own setting.
        assert!(f
            .spawn
            .window(Provider::Local, None, None)
            .unwrap_err()
            .contains("--window only"));
        assert_eq!(
            f.spawn
                .window(Provider::Mistral, Some("mistral-large-4"), None)
                .unwrap()
                .tokens,
            cover::MISTRAL_CONTEXT_TOKENS
        );
        assert_eq!(
            f.spawn
                .window(Provider::Local, None, Some(4096))
                .unwrap()
                .tokens,
            4096
        );
    }

    #[test]
    fn a_bad_cover_percent_or_window_is_refused_before_anything_starts() {
        let f = fixture();
        for (percent, window) in [(0.0, None), (150.0, None), (20.0, Some(0))] {
            let error = f
                .spawn
                .run(&covered(&f, "Refused size.", percent, window))
                .unwrap_err();
            assert!(!format!("{error:#}").is_empty());
        }
        assert!(f.launcher.started.lock().unwrap().is_empty());
        assert!(f
            .persona(&super::super::name_for(super::super::text_handle(
                "Refused size."
            )))
            .is_none());
    }

    #[test]
    fn a_codex_window_comes_from_its_catalog_when_a_model_is_named() {
        let f = fixture();
        std::fs::write(
            f.directory.path().join("codex-models.json"),
            r#"{"fetched_at":"2026-10-02T16:49:47Z","models":[
                {"slug":"gpt-5.5","context_window":272000,"effective_context_window_percent":95}]}"#,
        )
        .unwrap();
        let spawn = f.relaunch(|launch| launch.codex_model = Some("gpt-5.5".to_owned()));
        let mut request = covered(&f, "Codex sized.", 20.0, None);
        request.provider = Provider::Codex;
        let cover = spawn.run(&request).unwrap().cover.unwrap();
        assert_eq!(cover.window, Some(258_400));
        assert!(cover.window_source.contains("codex model catalog"));
        let calibration = cover.calibration.unwrap();
        assert_eq!(calibration.n, None);
        assert_eq!(calibration.chars_per_token, cover::DEFAULT_CHARS_PER_TOKEN);
    }

    #[test]
    fn a_claude_result_records_its_window_and_first_call_and_a_later_cover_uses_it() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("First claude run.")).unwrap();
        let session = "e0bf4238-7a93-4604-be3f-a4ddb456d86b";
        std::fs::write(
            &launched.report,
            format!(
                r#"{{"type":"result","subtype":"success","is_error":false,"result":"the report\n","session_id":"{session}","modelUsage":{{"claude-opus-5-5":{{"inputTokens":2,"contextWindow":1000000}}}}}}"#
            ),
        )
        .unwrap();
        let project = f.directory.path().join("claude-projects/-lane");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(format!("{session}.jsonl")),
            concat!(
                r#"{"type":"user","message":{"content":"x"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":192066,"cache_read_input_tokens":11266}}}"#,
                "\n"
            ),
        )
        .unwrap();
        f.settle(&launched.name, &launched).unwrap();
        let run = f.spawn.show(&launched.name).unwrap().runs[0].clone();
        assert_eq!(run.report_text.as_deref(), Some("the report\n"));
        assert_eq!(run.context_window, [1_000_000]);
        assert_eq!(run.first_call_input_tokens, [203_334]);
        assert_eq!(run.source_locator, [format!("session/{session}")]);
        let later = f
            .spawn
            .run(&covered(&f, "Second claude run.", 20.0, None))
            .unwrap();
        let cover = later.cover.unwrap();
        assert_eq!(cover.window, Some(1_000_000));
        assert!(
            cover.window_source.contains(&format!("{:x}", launched.run)),
            "{}",
            cover.window_source
        );
    }

    #[test]
    fn an_empty_claude_result_is_no_report_but_keeps_its_window() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Empty claude result.")).unwrap();
        std::fs::write(
            &launched.report,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"","modelUsage":{"claude-opus-5-5":{"inputTokens":2,"contextWindow":1000000}}}"#,
        )
        .unwrap();
        f.settle(&launched.name, &launched).unwrap();
        let run = f.spawn.show(&launched.name).unwrap().runs[0].clone();
        assert_eq!(run.report_text, None);
        assert_eq!(run.context_window, [1_000_000]);
    }

    #[test]
    fn a_claude_report_that_is_not_a_result_object_is_recorded_as_it_is() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Plain report.")).unwrap();
        std::fs::write(&launched.report, "plain words\n").unwrap();
        f.settle(&launched.name, &launched).unwrap();
        let run = f.spawn.show(&launched.name).unwrap().runs[0].clone();
        assert_eq!(run.report_text.as_deref(), Some("plain words\n"));
        assert!(run.context_window.is_empty() && run.first_call_input_tokens.is_empty());
    }

    #[test]
    fn settle_records_the_end_messages_the_spawner_and_retires_the_persona() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Settle me.")).unwrap();
        std::fs::write(&launched.report, "All done.\n").unwrap();
        let settled = f
            .spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: Some(launched.report.clone()),
                usage: None,
            })
            .unwrap();
        assert_eq!(
            settled.report,
            Some(super::super::text_handle("All done.\n"))
        );
        assert!(settled.message.is_some());

        let view = f.spawn.show(&launched.name).unwrap();
        let run = &view.runs[0];
        assert!(run.ended());
        assert_eq!(run.exit_code, [0]);
        assert_eq!(run.result, ["success"]);
        assert_eq!(run.report_text.as_deref(), Some("All done.\n"));
        assert!(f.persona(&launched.name).unwrap().retired);

        let inbox = crate::message::Message::with_storage(f.storage.clone())
            .list(&crate::message::ListOptions::new("agent-cc"))
            .unwrap();
        let note = inbox
            .entries
            .iter()
            .find(|entry| entry.incoming)
            .expect("the spawner has the zooid's message");
        assert_eq!(note.from, launched.persona);
        match &note.body {
            crate::message::MessageText::Text(text) => {
                assert!(
                    text.starts_with(&format!(
                        "{} finished, exit 0, report blake3:",
                        launched.name
                    )),
                    "{text}"
                );
                assert!(!text.contains("never read"), "{text}");
            }
            other => panic!("unexpected body {other:?}"),
        }

        // Retired, the same brief relaunches as the same zooid, with a new run.
        let again = f.spawn.run(&f.request("Settle me.")).unwrap();
        assert_eq!(again.name, launched.name);
        assert_eq!(again.persona, launched.persona);
        assert_ne!(again.run, launched.run);
        assert!(!f.persona(&launched.name).unwrap().retired);
        let runs = f.spawn.show(&launched.name).unwrap().runs;
        assert_eq!(runs.len(), 2);
        assert!(runs[0].ended() && !runs[1].ended());
    }

    #[test]
    fn settle_after_a_kill_records_the_signal_and_no_report() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Killed task.")).unwrap();
        f.spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit: Exit::from_systemd("killed", "KILL", "signal"),
                report: Some(launched.report.clone()),
                usage: None,
            })
            .unwrap();
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert!(run.exit_code.is_empty());
        assert_eq!(run.signal, ["KILL"]);
        assert!(run.report.is_empty());
        assert_eq!(describe_end(run), "signal KILL");
    }

    #[test]
    fn a_launch_that_never_started_leaves_no_run_and_no_live_persona() {
        let f = fixture_with(FakeLauncher {
            refuse: true,
            ..Default::default()
        });
        let error = f.spawn.run(&f.request("Never starts.")).unwrap_err();
        assert!(format!("{error:#}").contains("fake systemd refuses"));
        let (_, _, _, name) = super::super::spawn_fragment("Never starts.").unwrap();
        assert!(f.persona(&name).unwrap().retired);
        assert!(f.spawn.list().unwrap().is_empty());
    }

    #[test]
    fn run_refuses_bad_inputs_before_touching_relations() {
        let f = fixture();
        let mut request = f.request("x");
        request.effort = "extreme".to_owned();
        assert!(f.spawn.run(&request).is_err());
        let mut request = f.request("x");
        request.spawner = "nobody".to_owned();
        assert!(format!("{:#}", f.spawn.run(&request).unwrap_err()).contains("spawner"));
        let mut request = f.request("x");
        request.workdir = f.directory.path().join("missing");
        assert!(f.spawn.run(&request).is_err());
        assert!(f.launcher.started.lock().unwrap().is_empty());
    }

    #[test]
    fn wait_returns_once_the_unit_is_inactive() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Wait for me.")).unwrap();
        *f.launcher.state.lock().unwrap() = "inactive".to_owned();
        let view = f
            .spawn
            .wait(&launched.name, Duration::from_millis(1))
            .unwrap();
        assert_eq!(view.runs.len(), 1);
        assert!(is_running("deactivating"));
        assert!(!is_running("inactive"));
        assert!(!is_running("failed"));
    }

    #[test]
    fn settle_names_the_messages_a_zooid_never_read() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Read your mail.")).unwrap();
        std::fs::write(&launched.report, "Done.\n").unwrap();
        let messages = crate::message::Message::with_storage(f.storage.clone());
        let sent = messages
            .send(&crate::message::SendOptions {
                from: "agent-cc",
                to: &launched.name,
                text: "One more question for you.",
            })
            .unwrap();
        f.spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: Some(launched.report.clone()),
                usage: None,
            })
            .unwrap();
        let inbox = messages
            .list(&crate::message::ListOptions::new("agent-cc"))
            .unwrap();
        let note = inbox
            .entries
            .iter()
            .find(|entry| entry.incoming && entry.from == launched.persona)
            .expect("the spawner has the zooid's message");
        match &note.body {
            crate::message::MessageText::Text(text) => {
                assert!(
                    text.contains(&format!(
                        "It never read 1 message(s) sent to it: {:x} from agent-cc.",
                        sent.id
                    )),
                    "{text}"
                );
            }
            other => panic!("unexpected body {other:?}"),
        }
    }

    #[test]
    fn a_repeated_settlement_neither_records_the_end_twice_nor_messages_twice() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Settle twice.")).unwrap();
        std::fs::write(&launched.report, "Report.\n").unwrap();
        let first = f.settle(&launched.name, &launched).unwrap();
        assert!(!first.already_ended && !first.already_messaged);
        let again = f.settle(&launched.name, &launched).unwrap();
        assert!(again.already_ended && again.already_messaged);
        assert_eq!(again.message, first.message);
        assert_eq!(again.report, first.report);
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.finished.len(), 1);
        assert_eq!(run.exit_code, [0]);
        assert_eq!(run.message, [first.message.unwrap()]);
        assert_eq!(f.inbox().len(), 1);
        assert!(f.persona(&launched.name).unwrap().retired);
    }

    #[test]
    fn a_settlement_that_cannot_message_keeps_the_persona_live_for_a_repaired_one() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Message fails first.")).unwrap();
        std::fs::write(&launched.report, "Report.\n").unwrap();
        // A sender that does not exist cannot message; that is the failure.
        let error = f.settle("nobody-here", &launched).unwrap_err();
        assert!(
            format!("{error:#}").contains("message the spawner"),
            "{error:#}"
        );
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert!(run.ended());
        assert!(run.message.is_empty());
        assert_eq!(run.settle_failure.len(), 1, "{:?}", run.settle_failure);
        assert!(!f.persona(&launched.name).unwrap().retired);
        assert!(f.inbox().is_empty());
        // The relaunch refusal says how to finish it.
        let refusal = f.spawn.run(&f.request("Message fails first.")).unwrap_err();
        assert!(
            format!("{refusal:#}").contains("settle line"),
            "{refusal:#}"
        );

        let repaired = f.settle(&launched.name, &launched).unwrap();
        assert!(repaired.already_ended && !repaired.already_messaged);
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.finished.len(), 1, "the end is not recorded twice");
        assert_eq!(run.message.len(), 1);
        assert_eq!(f.inbox().len(), 1);
        assert!(f.persona(&launched.name).unwrap().retired);
    }

    #[test]
    fn a_wake_that_fails_fails_the_launch_even_with_a_tint() {
        let f = fixture_full(
            FakeLauncher::default(),
            StubRenderer {
                refuse_wake: true,
                ..Default::default()
            },
            "export TRIBLESPACE_PEERS=none\n",
        );
        let refusal = f.spawn.run(&f.request("Wake refused.")).unwrap_err();
        assert!(format!("{refusal:#}").contains("unreadable"), "{refusal:#}");
        assert!(f.launcher.started.lock().unwrap().is_empty());
    }

    /// The real renderer: the tint reaches the cover's own exact-span
    /// choice, so of two memories over the same span the one about the topic
    /// is the one the wake recalls.
    #[test]
    fn a_tint_chooses_between_memories_of_the_same_span() {
        let f = fixture_with(FakeLauncher::default());
        let memory = crate::memory::Memory::with_storage(f.storage.clone());
        let span = (
            hifitime::Epoch::from_gregorian_utc_hms(2026, 1, 1, 18, 0, 0),
            hifitime::Epoch::from_gregorian_utc_hms(2026, 1, 1, 20, 0, 0),
        );
        let amber = "Under the amber lamp JP played the piano.";
        let walk = "A long walk along the Weser in the rain.";
        memory.create(amber, Some(span), &[]).unwrap();
        memory.create(walk, Some(span), &[]).unwrap();
        let renderer = LibraryRenderer::default();
        let wake = |about: &str| {
            let tint = WakeTint {
                about: about.to_owned(),
                scores: None,
                ..Default::default()
            };
            renderer
                .wake(&f.storage, Some(&tint), PLAIN_MEMORY_CHARS)
                .unwrap()
        };
        let tinted = wake("amber lamp piano");
        assert!(tinted.contains(amber) && !tinted.contains(walk), "{tinted}");
        let tinted = wake("walk Weser rain");
        assert!(tinted.contains(walk) && !tinted.contains(amber), "{tinted}");
        // With no memory budget the wake is beliefs and goals alone: how a
        // --cover launch measures them.
        let fixed = renderer.wake(&f.storage, None, 0).unwrap();
        assert!(fixed.contains("Beliefs (cover):"), "{fixed}");
        assert!(!fixed.contains(amber) && !fixed.contains(walk), "{fixed}");
    }

    #[test]
    fn a_tint_recalls_current_knowledge_readings_beside_the_journal() {
        let f = fixture();
        let memory = crate::memory::Memory::with_storage(f.storage.clone());
        for day in 1..=16 {
            let start = hifitime::Epoch::from_gregorian_utc_hms(2026, 1, day, 8, 0, 0);
            let end = hifitime::Epoch::from_gregorian_utc_hms(2026, 1, day, 9, 0, 0);
            memory
                .create(
                    &format!(
                        "Journal day {day:02}. The first train passed the house before sunrise."
                    ),
                    Some((start, end)),
                    &[],
                )
                .unwrap();
        }
        let knowledge = crate::knowledge::Knowledge::with_storage(f.storage.clone());
        let curated = "Watch the warp and the orchard.";
        let wake = |cue: &str, chars| {
            LibraryRenderer
                .wake(
                    &f.storage,
                    Some(&WakeTint {
                        about: cue.into(),
                        scores: None,
                        ..Default::default()
                    }),
                    chars,
                )
                .unwrap()
        };
        let budget = 1100;
        let before = wake("warp", budget);
        assert!(!before.contains(crate::knowledge::recall::HEADING));
        knowledge
            .create("Standing note", curated, &["cover".into()], false)
            .unwrap();
        let old = knowledge
            .create("Tile sizes", "The stale warp schedule.", &[], false)
            .unwrap();
        let fresh = knowledge
            .edit(
                &format!("{old:x}"),
                Some("The warp schedule uses one tile per block."),
                None,
                &[],
                false,
            )
            .unwrap();
        let discarded = knowledge
            .create("Discarded tile", "A retired warp experiment.", &[], false)
            .unwrap();
        knowledge.archive(&format!("{discarded:x}")).unwrap();
        let orchard = knowledge
            .create(
                "Pruning",
                "Prune an orchard after the leaves fall.",
                &[],
                false,
            )
            .unwrap();
        for (cue, id, absent) in [
            ("warp", fresh, "Prune an orchard"),
            ("orchard", orchard, "one tile per block"),
        ] {
            let text = wake(cue, budget);
            let exact = knowledge.show(&format!("{id:x}"), true).unwrap();
            assert!(text.contains(&exact), "{text}");
            assert!(
                !text.contains(absent)
                    && !text.contains("stale warp")
                    && !text.contains("retired warp")
            );
            assert_eq!(text.matches(curated).count(), 1);
            // Whatever a brief reads, its journal is the same, byte for byte,
            // and within the whole budget; the readings have their own.
            let flexible = text.split_once("\nGoals:").unwrap().0;
            let (journal, readings) = flexible
                .split_once(crate::knowledge::recall::HEADING)
                .unwrap();
            assert_eq!(journal, before.split_once("\nGoals:").unwrap().0);
            assert!(journal.contains("Journal day"));
            assert!(journal.chars().count() <= budget);
            let allowance = crate::knowledge::recall::readings_allowance(budget);
            assert!(readings.chars().count() <= allowance);
            assert!(text.contains(&format!("of up to {allowance} characters, beside")));
            assert_eq!(text, wake(cue, budget));
            let mut opts = crate::orient::WakePairsOptions::default();
            opts.chars = budget;
            opts.about = Some(cue.into());
            let mut lines = String::new();
            crate::orient::Orient::with_storage(f.storage.clone())
                .wake_pairs(
                    &opts,
                    &mut Out::new(&mut |part| {
                        if let Part::Text { text } = part {
                            lines.push_str(&text);
                        }
                        Ok(())
                    }),
                )
                .unwrap();
            assert!(lines
                .lines()
                .map(|s| serde_json::from_str::<serde_json::Value>(s).unwrap())
                .any(
                    |pair| pair["command"] == format!("knowledge show --exact {id:x}")
                        && pair["output"] == exact
                ));
        }
        // Only the current orchard body has strong semantic evidence. The
        // higher scoring stale and archived bodies must never become readings.
        let tint = WakeTint {
            about: "Fruit tree care".into(),
            knowledge_contents: Some(Arc::new(
                [
                    ("The stale warp schedule.", 0.99),
                    ("A retired warp experiment.", 0.98),
                    ("Prune an orchard after the leaves fall.", 0.92),
                ]
                .into_iter()
                .map(|(body, score)| (super::super::text_handle(body), score))
                .collect(),
            )),
            ..Default::default()
        };
        let semantic = LibraryRenderer
            .wake(&f.storage, Some(&tint), budget)
            .unwrap();
        assert!(semantic.contains(&knowledge.show(&format!("{orchard:x}"), true).unwrap()));
        assert!(semantic.contains("Knowledge topic scorer: WeMM"));
        assert!(!semantic.contains("stale warp schedule"));
        assert!(!semantic.contains("retired warp experiment"));
        let coverage_gap = WakeTint {
            about: "warp".into(),
            knowledge_contents: Some(Arc::new(vec![(
                super::super::text_handle("Prune an orchard after the leaves fall."),
                0.1,
            )])),
            ..Default::default()
        };
        let supplemented = LibraryRenderer
            .wake(&f.storage, Some(&coverage_gap), budget)
            .unwrap();
        assert!(supplemented.contains(&knowledge.show(&format!("{fresh:x}"), true).unwrap()));
        assert!(supplemented.contains("lexical matches without semantic rows"));
        assert!(!wake("warp", 80).contains(crate::knowledge::recall::HEADING));
        for cue in ["", "unmatchedkeyword"] {
            let broad = wake(cue, budget);
            assert!(broad.contains(curated) && !broad.contains("one tile per block"));
        }
        let personal = LibraryRenderer.wake(&f.storage, None, 0).unwrap();
        assert!(personal.contains(curated) && personal.contains("Beliefs (cover):"));
        let zero = wake("warp", 0);
        assert!(!zero.contains(curated) && zero.contains("Goals:"));
        assert!(!zero.contains(crate::knowledge::recall::HEADING) && !zero.contains("Journal day"));
        // A reading too long for the allowance is named, never clipped.
        let long = knowledge
            .create("Warp manual", &"warp ".repeat(400), &[], false)
            .unwrap();
        let text = wake("warp", budget);
        assert!(!text.contains("Warp manual"));
        assert!(text.contains("didn't fit the") && text.contains(&format!("{long:x}")));
    }

    #[test]
    fn a_tint_that_fails_keeps_the_lexical_cue() {
        let f = fixture_full(
            FakeLauncher::default(),
            StubRenderer {
                refuse_tint: true,
                ..Default::default()
            },
            "export TRIBLESPACE_PEERS=none\n",
        );
        let launched = f.spawn.run(&f.request("Tint refused.")).unwrap();
        let why = launched.lexical_fallback.clone().unwrap();
        assert!(why.contains("WeMM"), "{why}");
        let prompt = std::fs::read_to_string(
            f.launcher.started.lock().unwrap()[0]
                .script
                .with_extension("prompt"),
        )
        .unwrap();
        assert!(prompt.contains("STUB-WAKE about Tint refused."));
        assert_eq!(prompt.matches("STUB-WAKE").count(), 1);
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert!(run.untinted.is_empty());
        assert_eq!(run.lexical_fallback, [why]);
    }

    #[test]
    fn a_tint_that_panics_keeps_the_lexical_cue() {
        let f = fixture_full(
            FakeLauncher::default(),
            StubRenderer {
                panic_tint: true,
                ..Default::default()
            },
            "export TRIBLESPACE_PEERS=none\n",
        );
        let launched = f.spawn.run(&f.request("Tint panics.")).unwrap();
        let why = launched.lexical_fallback.clone().unwrap();
        assert!(why.contains("tint panicked"), "{why}");
        assert!(why.contains("BufferTooBig"), "{why}");
        let prompt = std::fs::read_to_string(
            f.launcher.started.lock().unwrap()[0]
                .script
                .with_extension("prompt"),
        )
        .unwrap();
        assert!(prompt.contains("STUB-WAKE about Tint panics."));
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert!(run.untinted.is_empty());
        assert_eq!(run.lexical_fallback, [why]);
    }

    #[test]
    fn a_launch_refuses_when_the_colony_file_routes_what_it_did_not_resolve() {
        let f = fixture_full(
            FakeLauncher::default(),
            StubRenderer::default(),
            &format!(
                "export TRIBLESPACE_COLLECTION_SPAWN=blake3:{}\n",
                "0".repeat(64)
            ),
        );
        let error = f.spawn.run(&f.request("Unrouted.")).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("TRIBLESPACE_COLLECTION_SPAWN"), "{text}");
        assert!(text.contains("signer-private"), "{text}");
        assert!(f.launcher.started.lock().unwrap().is_empty());
        assert!(f.spawn.list().unwrap().is_empty());
    }

    #[test]
    fn the_unit_script_pins_no_route_the_launch_did_not_resolve() {
        // The fixture has no routes: the script exports none, so its zooid
        // and settlement open the same private collections the launch did.
        let f = fixture();
        f.spawn.run(&f.request("No routes.")).unwrap();
        let script = f.script(0);
        assert!(
            !script.contains("export TRIBLESPACE_COLLECTION_"),
            "{script}"
        );
        assert!(script.contains("--strict-mcp-config"));
        assert!(script.contains("--output-format json < "));
    }

    #[test]
    fn a_claude_zooid_gets_its_own_faculties_server_and_nothing_ambient() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("MCP task.")).unwrap();
        let script = f.script(0);
        let unit = f.launcher.started.lock().unwrap()[0].clone();
        let config = f
            .directory
            .path()
            .join("state")
            .join(format!("{}.mcp.json", launched.name));
        // The per-run config, under strict mode, so nothing ambient loads.
        assert!(
            script.contains(&format!(
                "--strict-mcp-config --mcp-config {} ",
                config.display()
            )),
            "{script}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({"mcpServers": {"faculties": {
                "type": "stdio",
                "command": "/bin/bash",
                "args": [unit.script.display().to_string(), "mcp"],
            }}})
        );
        // The server is the script itself, so it gets the CLI's environment:
        // the colony file, the pinned routes, PILE, the key and the persona.
        // Its stderr goes to the run's log, so a failed start is visible;
        // its stdout is untouched, because it is the protocol channel.
        let faculties = f.directory.path().join("bin").join("faculties");
        let mcp = script
            .find(&format!(
                "if [ \"${{1:-}}\" = mcp ]; then exec {} mcp 2>> {}; fi\n",
                faculties.display(),
                unit.log.display()
            ))
            .unwrap_or_else(|| panic!("{script}"));
        assert!(script.find(&format!("PERSONA={}", launched.name)).unwrap() < mcp);
        assert!(script.find(". ").unwrap() < mcp);
        assert!(!f
            .directory
            .path()
            .join("state")
            .join("empty-mcp.json")
            .exists());
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.model, ["claude-opus-5-5"]);
        assert!(run.model_unset.is_empty());
        assert_eq!(launched.model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn a_claude_zooid_gets_inbox_hooks_through_its_own_settings_file() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Hooked task.")).unwrap();
        let script = f.script(0);
        let unit = f.launcher.started.lock().unwrap()[0].clone();
        let settings = f
            .directory
            .path()
            .join("state")
            .join(format!("{}.settings.json", launched.name));
        assert!(
            script.contains(&format!(
                "--mcp-config {} --settings {} --output-format json",
                f.directory
                    .path()
                    .join("state")
                    .join(format!("{}.mcp.json", launched.name))
                    .display(),
                settings.display()
            )),
            "{script}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let hook = |kind: &str, timeout: u64| {
            serde_json::json!([{"hooks": [{
                "type": "command",
                "command": format!("/bin/bash {} inbox {kind}", unit.script.display()),
                "timeout": timeout,
            }]}])
        };
        assert_eq!(
            parsed,
            serde_json::json!({"hooks": {
                "Stop": hook("stop", INBOX_STOP_TIMEOUT_SECS),
                "PostToolUse": hook("post", INBOX_POST_TIMEOUT_SECS),
            }})
        );
        // The script answers them through `zooid inbox`, for this run, in the
        // CLI's environment and before the agent's own exports.
        let inbox = script
            .find(&format!(
                "if [ \"${{1:-}}\" = inbox ]; then exec zooid inbox {} --run {:x} --hook \"${{2:-}}\" 2>> {}; fi\n",
                launched.name,
                launched.run,
                unit.log.display()
            ))
            .unwrap_or_else(|| panic!("{script}"));
        assert!(script.find(&format!("PERSONA={}", launched.name)).unwrap() < inbox);
        assert!(inbox < script.find("exec /fake/claude").unwrap());
    }

    fn hook(
        f: &Fixture,
        launched: &Launched,
        hook: InboxHook,
        active: bool,
        now: u64,
    ) -> InboxAnswer {
        f.spawn.inbox(&InboxRequest {
            name: launched.name.clone(),
            run: launched.run,
            hook,
            stop_hook_active: active,
            now,
        })
    }

    fn context(answer: &InboxAnswer) -> String {
        let output = answer.output.as_ref().expect("an answer");
        assert_eq!(output["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn blocked(answer: &InboxAnswer) -> String {
        let output = answer.output.as_ref().expect("an answer");
        assert_eq!(output["decision"], "block");
        assert_eq!(output.as_object().unwrap().len(), 2, "{output}");
        output["reason"].as_str().unwrap().to_owned()
    }

    #[test]
    fn the_inbox_hooks_show_unread_messages_without_acknowledging_them() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Mail while working.")).unwrap();
        let t = 1_800_000_000;
        // Nothing unread: both hooks print nothing.
        assert_eq!(
            hook(&f, &launched, InboxHook::Stop, false, t),
            InboxAnswer::default()
        );
        assert_eq!(
            hook(&f, &launched, InboxHook::Post, false, t),
            InboxAnswer::default()
        );

        let messages = crate::message::Message::with_storage(f.storage.clone());
        let send = |text: &str| {
            messages
                .send(&crate::message::SendOptions {
                    from: "agent-cc",
                    to: &launched.name,
                    text,
                })
                .unwrap()
                .id
        };
        let first = send("Stay source-only: no hardware.");
        // Within a minute of the last read, post opens nothing and says nothing.
        assert_eq!(
            hook(&f, &launched, InboxHook::Post, false, t + 59),
            InboxAnswer::default()
        );
        // A minute on, it shows the new message once.
        let shown = context(&hook(&f, &launched, InboxHook::Post, false, t + 60));
        assert!(
            shown.contains(&format!(
                "message {first:x} from agent-cc:\nStay source-only: no hardware."
            )),
            "{shown}"
        );
        assert!(shown.contains("message_ack"), "{shown}");
        // Already shown: a later read finds nothing new to show.
        assert_eq!(
            hook(&f, &launched, InboxHook::Post, false, t + 200),
            InboxAnswer::default()
        );
        // A newer message is shown alone.
        let second = send("And one more thing.");
        let shown = context(&hook(&f, &launched, InboxHook::Post, false, t + 300));
        assert!(
            shown.contains(&format!("{second:x}")) && !shown.contains(&format!("{first:x}")),
            "{shown}"
        );

        // Showing acknowledged nothing: stop blocks once, naming both.
        let reason = blocked(&hook(&f, &launched, InboxHook::Stop, false, t + 301));
        for (id, text) in [
            (first, "Stay source-only: no hardware."),
            (second, "And one more thing."),
        ] {
            assert!(
                reason.contains(&format!("message {id:x} from agent-cc:\n{text}")),
                "{reason}"
            );
        }
        assert!(
            reason.contains("2 message(s)") && reason.contains("give the whole report again"),
            "{reason}"
        );
        // Continuing from that block, the end is allowed.
        assert_eq!(
            hook(&f, &launched, InboxHook::Stop, true, t + 302),
            InboxAnswer::default()
        );

        // Acknowledged by the zooid, nothing blocks.
        messages.ack(&format!("{first:x}"), &launched.name).unwrap();
        messages
            .ack(&format!("{second:x}"), &launched.name)
            .unwrap();
        assert_eq!(
            hook(&f, &launched, InboxHook::Stop, false, t + 303),
            InboxAnswer::default()
        );
    }

    #[test]
    fn a_new_run_of_the_same_zooid_starts_its_inbox_state_afresh() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Run twice.")).unwrap();
        let messages = crate::message::Message::with_storage(f.storage.clone());
        let id = messages
            .send(&crate::message::SendOptions {
                from: "agent-cc",
                to: &launched.name,
                text: "Read me.",
            })
            .unwrap()
            .id;
        let t = 1_800_000_000;
        assert!(
            context(&hook(&f, &launched, InboxHook::Post, false, t)).contains(&format!("{id:x}"))
        );
        // A later run under the same name is neither throttled by nor
        // silenced by the earlier run's state.
        let mut later = launched.clone();
        later.run = Id::new([0x42; 16]).unwrap();
        assert!(
            context(&hook(&f, &later, InboxHook::Post, false, t + 1)).contains(&format!("{id:x}"))
        );
        // A clock that went back is not throttled forever.
        assert_eq!(
            hook(&f, &later, InboxHook::Post, false, t - 1000),
            InboxAnswer::default()
        );
        // An unreadable state file is logged and replaced.
        let state = f
            .directory
            .path()
            .join("state")
            .join(format!("{}.inbox.json", launched.name));
        std::fs::write(&state, "not json").unwrap();
        let answer = hook(&f, &later, InboxHook::Post, false, t + 2);
        assert!(context(&answer).contains(&format!("{id:x}")));
        assert!(answer.warning.unwrap().contains("unreadable"));
    }

    #[test]
    fn an_unreadable_inbox_blocks_a_stop_once_and_is_only_logged_by_a_post() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Broken inbox.")).unwrap();
        let broken = Spawn::new(
            Storage::new(f.directory.path().join("no-such.pile"), None),
            f.spawn.launch.clone(),
        );
        let request = |hook, active| InboxRequest {
            name: launched.name.clone(),
            run: launched.run,
            hook,
            stop_hook_active: active,
            now: 1_800_000_000,
        };
        let stop = broken.inbox(&request(InboxHook::Stop, false));
        let reason = blocked(&stop);
        assert!(
            reason.contains("could not be read") && reason.contains("No such file"),
            "{reason}"
        );
        assert!(stop.warning.is_some());
        assert_eq!(
            broken.inbox(&request(InboxHook::Stop, true)),
            InboxAnswer::default()
        );
        let post = broken.inbox(&request(InboxHook::Post, false));
        assert_eq!(post.output, None);
        assert!(post.warning.unwrap().contains("could not be read"));
    }

    #[test]
    fn a_long_message_is_cut_and_says_where_the_rest_is() {
        let f = fixture();
        let launched = f.spawn.run(&f.request("Long mail.")).unwrap();
        crate::message::Message::with_storage(f.storage.clone())
            .send(&crate::message::SendOptions {
                from: "agent-cc",
                to: &launched.name,
                text: &"x".repeat(INBOX_BODY_CHARS + 10),
            })
            .unwrap();
        let reason = blocked(&hook(&f, &launched, InboxHook::Stop, false, 1));
        assert!(reason.contains(&format!(
            "{}\n[cut at {INBOX_BODY_CHARS} characters",
            "x".repeat(INBOX_BODY_CHARS)
        )));
        assert!(!reason.contains(&"x".repeat(INBOX_BODY_CHARS + 1)));
    }

    #[test]
    fn the_launching_build_comes_first_on_the_zooid_path() {
        let home = Path::new("/h");
        let mut config = LaunchConfig::under_home(home);
        let installed = config.path.clone();
        config.own_build_first(Path::new("/g/releases/x/bin"));
        assert_eq!(config.path, format!("/g/releases/x/bin:{installed}"));
        // Launching again from the same build does not stack it twice.
        config.own_build_first(Path::new("/g/releases/x/bin"));
        assert_eq!(config.path, format!("/g/releases/x/bin:{installed}"));
        // And `faculties` is then found in that build, not on the host.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("faculties");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        config.own_build_first(dir.path());
        assert_eq!(config.faculties_binary().unwrap(), bin);
    }

    #[test]
    fn a_launch_without_a_faculties_binary_starts_nothing() {
        let f = fixture();
        let spawn = f.relaunch(|launch| launch.faculties = PathBuf::from("no-such-faculties"));
        let error = format!("{:#}", spawn.run(&f.request("No server.")).unwrap_err());
        assert!(error.contains("no executable no-such-faculties"), "{error}");
        let spawn = f.relaunch(|launch| launch.faculties = PathBuf::from("/nowhere/faculties"));
        let error = format!("{:#}", spawn.run(&f.request("No server.")).unwrap_err());
        assert!(error.contains("not an executable file"), "{error}");
        assert!(f.launcher.started.lock().unwrap().is_empty());
        assert!(f
            .persona(&super::super::name_for(super::super::text_handle(
                "No server."
            )))
            .is_none());
    }

    #[test]
    fn codex_writes_its_own_report_and_local_is_refused() {
        let f = fixture();
        let mut request = f.request("Codex task.");
        request.provider = Provider::Codex;
        request.effort = "max".to_owned();
        let launched = f.spawn.run(&request).unwrap();
        assert_eq!(launched.provider, Provider::Codex);
        let script = f.script(0);
        assert!(
            script.contains(&format!(
                "exec /fake/codex exec --dangerously-bypass-approvals-and-sandbox \
             --skip-git-repo-check -c model_reasoning_effort=xhigh -o {} - < ",
                launched.report.display()
            )),
            "{script}"
        );
        assert!(!script.contains(&format!("> {}", launched.report.display())));
        // The inbox hooks are claude's; codex gets neither the flag nor a file.
        assert!(!script.contains("--settings"), "{script}");
        assert!(!f
            .directory
            .path()
            .join("state")
            .join(format!("{}.settings.json", launched.name))
            .exists());
        // Its own CODEX_HOME: the login linked in, nothing ambient copied.
        let home = f
            .directory
            .path()
            .join("state")
            .join(format!("{}.codex", launched.name));
        assert!(
            script.contains(&format!("export CODEX_HOME={}\n", home.display())),
            "{script}"
        );
        assert_eq!(
            std::fs::read_link(home.join("auth.json")).unwrap(),
            f.directory.path().join("codex-auth.json")
        );
        let mut entries: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(entries, ["auth.json", "config.toml"]);
        // Its config is the faculties server and nothing else.
        let unit = f.launcher.started.lock().unwrap()[0].clone();
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            codex_mcp_config(&unit.script)
        );
        assert!(script.contains("= mcp ]; then exec "), "{script}");
        // codex writes no claude MCP config.
        assert!(!f
            .directory
            .path()
            .join("state")
            .join(format!("{}.mcp.json", launched.name))
            .exists());
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.provider, ["codex"]);
        // No codex model is configured: none is passed, and the run says so.
        assert!(!script.contains(" -m "), "{script}");
        assert!(run.model.is_empty());
        assert_eq!(run.model_unset.len(), 1);
        assert!(run.model_unset[0].contains("SPAWN_CODEX_MODEL"));
        // The provider is not part of the name.
        assert_eq!(
            launched.name,
            super::super::name_for(super::super::text_handle("Codex task."))
        );

        // Without a login nothing starts, and the brief stays relaunchable.
        std::fs::remove_file(f.directory.path().join("codex-auth.json")).unwrap();
        let mut request = f.request("Codex task without a login.");
        request.provider = Provider::Codex;
        let error = format!("{:#}", f.spawn.run(&request).unwrap_err());
        assert!(error.contains("codex is not logged in"), "{error}");
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
        assert!(f
            .persona(&super::super::name_for(super::super::text_handle(
                "Codex task without a login."
            )))
            .is_none());

        // A configured codex model is passed and recorded.
        let spawn = f.relaunch(|launch| launch.codex_model = Some("gpt-test".to_owned()));
        let mut request = f.request("Codex task with a model.");
        request.provider = Provider::Codex;
        std::fs::write(f.directory.path().join("codex-auth.json"), "fake login\n").unwrap();
        let launched = spawn.run(&request).unwrap();
        let script = f.script(1);
        assert!(
            script.contains("--skip-git-repo-check -m gpt-test -c model_reasoning_effort="),
            "{script}"
        );
        let run = &spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.model, ["gpt-test"]);
        assert!(run.model_unset.is_empty());

        let mut request = f.request("Local task.");
        request.provider = Provider::Local;
        let error = f.spawn.run(&request).unwrap_err();
        assert!(format!("{error:#}").contains("SPAWN_LOCAL_EXECUTOR"));
        assert_eq!(f.launcher.started.lock().unwrap().len(), 2);
        assert!(f
            .persona(&super::super::name_for(super::super::text_handle(
                "Local task."
            )))
            .is_none());
    }

    #[test]
    fn codex_model_and_login_come_from_the_environment_when_set() {
        let home = Path::new("/h");
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        let mut config = LaunchConfig::under_home(home);
        config.override_from(env(&[])).unwrap();
        assert_eq!(config.codex_model, None);
        assert_eq!(config.codex_auth, Some(home.join(".codex/auth.json")));
        let mut config = LaunchConfig::under_home(home);
        config
            .override_from(env(&[
                ("SPAWN_CODEX_MODEL", " gpt-5.2-codex "),
                ("SPAWN_CODEX_AUTH", " /other/auth.json "),
            ]))
            .unwrap();
        assert_eq!(config.codex_model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(config.codex_auth, Some(PathBuf::from("/other/auth.json")));
        // Blank is unset: the default login stays.
        let mut config = LaunchConfig::under_home(home);
        config
            .override_from(env(&[("SPAWN_CODEX_AUTH", "  ")]))
            .unwrap();
        assert_eq!(config.codex_auth, Some(home.join(".codex/auth.json")));
    }

    #[test]
    fn mistral_settings_come_from_the_environment_and_a_bad_price_is_refused() {
        let home = Path::new("/h");
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        let mut config = LaunchConfig::under_home(home);
        config.override_from(env(&[])).unwrap();
        assert_eq!(config.vibe, PathBuf::from("vibe"));
        assert_eq!(config.mistral_model, "mistral-large-4");
        assert_eq!(
            config.mistral_key_secret,
            "c2c7b31676a61315928103941b04d487"
        );
        assert_eq!(config.mistral_max_price, 2.0);
        config
            .override_from(env(&[
                ("SPAWN_VIBE", " /v/bin/vibe "),
                (
                    "SPAWN_MISTRAL_KEY_SECRET",
                    " 00112233445566778899aabbccddeeff ",
                ),
                ("SPAWN_MISTRAL_MAX_PRICE", " 0.5 "),
            ]))
            .unwrap();
        assert_eq!(config.vibe, PathBuf::from("/v/bin/vibe"));
        assert_eq!(
            config.mistral_key_secret,
            "00112233445566778899aabbccddeeff"
        );
        assert_eq!(config.mistral_max_price, 0.5);
        for bad in ["0", "-1", "lots", "inf", "NaN"] {
            let mut config = LaunchConfig::under_home(home);
            let pairs: &'static [(&'static str, &'static str)] = match bad {
                "0" => &[("SPAWN_MISTRAL_MAX_PRICE", "0")],
                "-1" => &[("SPAWN_MISTRAL_MAX_PRICE", "-1")],
                "lots" => &[("SPAWN_MISTRAL_MAX_PRICE", "lots")],
                "inf" => &[("SPAWN_MISTRAL_MAX_PRICE", "inf")],
                _ => &[("SPAWN_MISTRAL_MAX_PRICE", "NaN")],
            };
            assert!(config.override_from(env(pairs)).is_err(), "{bad}");
        }
    }

    #[test]
    fn mistral_parses_and_auto_never_chooses_it() {
        assert_eq!(Provider::parse(" Mistral ").unwrap(), Provider::Mistral);
        assert_eq!(Provider::Mistral.name(), "mistral");
        for name in Provider::NAMES {
            assert_eq!(Provider::parse(name).unwrap().name(), name);
        }
        use crate::schemas::quota::PROVIDER_CLAUDE;
        let hour = 3_600 * 1_000_000_000_i128;
        let now = 100 * hour;
        let view = |groups| crate::quota::operations::QuotaView {
            now,
            examined: 0,
            groups,
            status_examined: 0,
            statuses: vec![],
            status_error: None,
        };
        // A provider with all its quota left that is not claude or codex is
        // never a candidate: auto picks the nearly spent claude.
        let other = Id::new([9; 16]).unwrap();
        let (chosen, why) = choose_provider(&view(vec![
            group(PROVIDER_CLAUDE, 99.0, now - hour, now + hour),
            group(other, 0.0, now - hour, now + hour),
        ]))
        .unwrap();
        assert_eq!(chosen, Provider::Claude);
        assert!(!why.contains("mistral"), "{why}");
        // With nothing known, auto refuses rather than falling to mistral.
        let error = format!("{:#}", choose_provider(&view(vec![])).unwrap_err());
        assert!(!error.contains("mistral"), "{error}");
    }

    #[test]
    fn a_policy_provider_never_reaches_an_agent_command_line() {
        let f = fixture();
        let files = RunFiles {
            name: "n".to_owned(),
            workdir: PathBuf::from("/w"),
            script: PathBuf::from("/s/n.sh"),
            prompt: PathBuf::from("/s/n.prompt"),
            report: PathBuf::from("/s/n.report"),
            mcp_config: PathBuf::from("/s/n.mcp.json"),
            settings: PathBuf::from("/s/n.settings.json"),
            codex_home: PathBuf::from("/s/n.codex"),
            vibe_home: PathBuf::from("/s/n.vibe"),
            vibe_out: PathBuf::from("/s/n.vibe-out"),
            resume: None,
            brief: PathBuf::from("/s/n.brief"),
            spawner: "s".to_owned(),
        };
        for provider in [Provider::Auto, Provider::Local] {
            assert!(f.spawn.agent(provider, "high", &files).is_err());
        }
        let line = f.spawn.agent(Provider::Mistral, "high", &files).unwrap();
        assert_eq!(line.argv[0], "/fake/vibe");
        assert!(!line.argv.iter().any(|word| word.contains("claude")));
        // The key is named by its Secrets version only: no argument and no
        // exported variable carries a key.
        assert!(!line
            .argv
            .iter()
            .any(|word| word.contains("MISTRAL_API_KEY")));
        assert_eq!(line.env, [("VIBE_HOME".to_owned(), "/s/n.vibe".to_owned())]);
        assert_eq!(
            line.secrets,
            [(
                "MISTRAL_API_KEY".to_owned(),
                "c2c7b31676a61315928103941b04d487".to_owned()
            )]
        );
        assert!(line.stdout_is_report);
    }

    #[test]
    fn a_mistral_zooid_runs_vibe_with_its_own_home_and_the_key_read_in_the_unit() {
        let f = fixture();
        let mut request = f.request("Mistral task.");
        request.provider = Provider::Mistral;
        let state = f.directory.path().join("state");
        let name = super::super::name_for(super::super::text_handle("Mistral task."));
        // A previous run's export must not be read as this run's usage.
        let out = state.join(format!("{name}.vibe-out"));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("export.json"), "stale").unwrap();
        let launched = f.spawn.run(&request).unwrap();
        assert_eq!(launched.provider, Provider::Mistral);
        assert_eq!(launched.provider_reason, "mistral was asked for");
        assert!(!out.join("export.json").exists());
        let script = f.script(0);
        let home = state.join(format!("{name}.vibe"));
        let prompt = state.join(format!("{name}.prompt"));
        let workdir = std::fs::canonicalize(f.directory.path().join("lane")).unwrap();
        assert!(
            script.contains(&format!(
                "exec /fake/vibe --workdir {} --trust --auto-approve --model mistral-large-4 \
                 --max-price 2 --output text --output-dir {} --prompt-file {} < {} > {}\n",
                workdir.display(),
                out.display(),
                prompt.display(),
                prompt.display(),
                launched.report.display()
            )),
            "{script}"
        );
        // The key: unset for every branch, read from Secrets for the agent
        // only, after settlement and the MCP server have branched off.
        let unset = script
            .find("unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT MISTRAL_API_KEY ${!TRIBLESPACE_@}\n")
            .unwrap();
        let read = script
            .find("MISTRAL_API_KEY=$(secrets get --secret c2c7b31676a61315928103941b04d487) && ")
            .unwrap();
        let settle = script.find("= settle ]").unwrap();
        let mcp = script.find("= mcp ]").unwrap();
        let exec = script.find("exec /fake/vibe").unwrap();
        assert!(
            unset < settle && settle < mcp && mcp < read && read < exec,
            "{script}"
        );
        assert!(
            script.contains(
                " || { echo \"zooid: reading MISTRAL_API_KEY from Secrets failed\" >&2; \
                 exit 77; }\nexport MISTRAL_API_KEY\n"
            ),
            "{script}"
        );
        assert_eq!(script.matches("MISTRAL_API_KEY=").count(), 1, "{script}");
        assert!(script.contains(&format!("export VIBE_HOME={}\n", home.display())));
        // Settlement reads vibe's export for the usage.
        assert!(
            script.contains(&format!(" --usage {}\n", out.join("export.json").display())),
            "{script}"
        );
        // The vibe home holds only the configuration spawn wrote.
        let unit = f.launcher.started.lock().unwrap()[0].clone();
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert_eq!(config, vibe_config(&unit.script, "mistral-large-4", "high"));
        assert!(!state.join(format!("{name}.mcp.json")).exists());
        assert!(!state.join(format!("{name}.codex")).exists());
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.provider, ["mistral"]);
        assert_eq!(run.model, ["mistral-large-4"]);

        // Settlement records vibe's usage and cost with the end.
        std::fs::write(
            out.join("export.json"),
            r#"{"outcome":"finished","usage":{"input_tokens":520386,"output_tokens":1243,
               "cached_input_tokens":518144,"total_tokens":521629},"cost_usd":0.0904}"#,
        )
        .unwrap();
        std::fs::write(&launched.report, "done\n").unwrap();
        f.spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: Some(launched.report.clone()),
                usage: Some(out.join("export.json")),
            })
            .unwrap();
        let view = f.spawn.show(&launched.name).unwrap();
        let run = &view.runs[0];
        assert_eq!(run.input_tokens, [520386]);
        assert_eq!(run.cached_input_tokens, [518144]);
        assert_eq!(run.output_tokens, [1243]);
        assert_eq!(run.cost_usd, [0.0904]);
        assert!(run.settle_failure.is_empty());
        let shown = render_show(&view, false);
        assert!(
            shown.contains("tokens: 520386 in (518144 cached), 1243 out"),
            "{shown}"
        );
        assert!(shown.contains("cost: $0.0904"), "{shown}");
    }

    #[test]
    fn a_mistral_run_without_an_export_says_so_at_settlement() {
        let f = fixture();
        let mut request = f.request("Mistral task that never started vibe.");
        request.provider = Provider::Mistral;
        let launched = f.spawn.run(&request).unwrap();
        let missing = f.directory.path().join("state").join("absent.json");
        let error = f
            .spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit: Exit::from_systemd("exited", "77", "exit-code"),
                report: None,
                usage: Some(missing),
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("usage"), "{error:#}");
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        // The end and the message are recorded anyway; the gap is named.
        assert_eq!(run.exit_code, [77]);
        assert_eq!(run.message.len(), 1);
        assert!(run.input_tokens.is_empty() && run.cost_usd.is_empty());
        assert!(
            run.settle_failure.iter().any(|why| why.contains("usage")),
            "{:?}",
            run.settle_failure
        );
    }

    #[test]
    fn vibe_thinking_is_never_off_and_its_config_is_large_4_at_list_price() {
        assert_eq!(vibe_thinking("low"), "low");
        assert_eq!(vibe_thinking("medium"), "medium");
        for effort in ["high", "xhigh", "max"] {
            assert_eq!(vibe_thinking(effort), "high");
        }
        let config = vibe_config(Path::new("/s/n.sh"), "mistral-large-4", "high");
        for line in [
            "active_model = \"mistral-large-4\"\n",
            "enable_telemetry = false\n",
            "enable_update_checks = false\n",
            "enable_auto_update = false\n",
            "enable_subagents = false\n",
            "api_key_env_var = \"MISTRAL_API_KEY\"\n",
            "alias = \"mistral-large-4\"\n",
            "input_price = 0.7\n",
            "output_price = 2.1\n",
            "thinking = \"high\"\n",
            "transport = \"stdio\"\n",
            "command = \"/bin/bash\"\n",
            "args = [\"/s/n.sh\", \"mcp\"]\n",
        ] {
            assert!(config.contains(line), "{line:?} in\n{config}");
        }
        // No cached price: vibe then bills cached input at the input price.
        assert!(!config.contains("cached_input_price"));
        assert_eq!(config.matches("[[mcp_servers]]").count(), 1);
        assert!(!config.contains("thinking = \"off\""));
    }

    #[test]
    fn a_codex_home_is_reused_and_its_login_link_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("n.codex");
        let (old, new) = (directory.path().join("old"), directory.path().join("new"));
        codex_home_for_run(&home, Some(&old), "first\n").unwrap();
        std::fs::write(home.join("history.jsonl"), "earlier run\n").unwrap();
        codex_home_for_run(&home, Some(&new), "second\n").unwrap();
        assert_eq!(std::fs::read_link(home.join("auth.json")).unwrap(), new);
        assert!(home.join("history.jsonl").exists());
        // The config is written fresh each run, never kept from an earlier one.
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            "second\n"
        );
        let bare = directory.path().join("bare.codex");
        codex_home_for_run(&bare, None, "only\n").unwrap();
        let entries: Vec<_> = std::fs::read_dir(&bare)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["config.toml"]);
    }

    fn group(
        provider: Id,
        used: f64,
        observed: i128,
        resets_at: i128,
    ) -> crate::quota::operations::QuotaGroup {
        crate::quota::operations::QuotaGroup {
            event: Id::new([provider.raw()[0]; 16]).unwrap(),
            provider,
            provider_name: None,
            account: "a".to_owned(),
            window_secs: 18_000,
            bucket: None,
            observed,
            used_percent: vec![used],
            resets_at: vec![resets_at],
            host: vec![],
        }
    }

    #[test]
    fn auto_picks_the_most_headroom_and_treats_stale_readings_as_unknown() {
        use crate::schemas::quota::{PROVIDER_CLAUDE, PROVIDER_CODEX};
        let hour = 3_600 * 1_000_000_000_i128;
        let now = 100 * hour;
        let view = |groups| crate::quota::operations::QuotaView {
            now,
            examined: 0,
            groups,
            status_examined: 0,
            statuses: vec![],
            status_error: None,
        };
        let (chosen, why) = choose_provider(&view(vec![
            group(PROVIDER_CLAUDE, 80.0, now - hour, now + hour),
            group(PROVIDER_CLAUDE, 10.0, now - hour, now + hour),
            group(PROVIDER_CODEX, 30.0, now - hour, now + hour),
        ]))
        .unwrap();
        // claude's tightest window has 20% left, codex's 70%.
        assert_eq!(chosen, Provider::Codex);
        assert!(
            why.contains("codex 70.0% left") && why.contains("claude 20.0% left"),
            "{why}"
        );

        // A codex reading older than the stale age is unknown, never spare.
        let (chosen, why) = choose_provider(&view(vec![
            group(PROVIDER_CLAUDE, 90.0, now - hour, now + hour),
            group(PROVIDER_CODEX, 0.0, now - 7 * hour, now + hour),
        ]))
        .unwrap();
        assert_eq!(chosen, Provider::Claude);
        assert!(why.contains("codex unknown"), "{why}");

        // A window that already reset says nothing about now; nothing known refuses.
        let error = choose_provider(&view(vec![group(
            PROVIDER_CLAUDE,
            5.0,
            now - hour,
            now - 1,
        )]))
        .unwrap_err();
        assert!(format!("{error:#}").contains("no provider with known headroom"));
        assert!(choose_provider(&view(vec![])).is_err());
    }

    #[test]
    fn spawn_goal_renders_the_brief_records_the_revision_and_notes_the_launch() {
        let f = fixture();
        let goals = Goals::with_storage(f.storage.clone());
        let parent = goals
            .add("The larger aim", crate::goals::AddOptions::default())
            .unwrap();
        let parent_hex = format!("{:x}", parent.goal);
        let goal = goals
            .add(
                "Build the widget",
                crate::goals::AddOptions {
                    status: "doing",
                    parent: Some(&parent_hex),
                    note: Some("Decided: use the small design."),
                    ..Default::default()
                },
            )
            .unwrap();
        let goal_hex = format!("{:x}", goal.goal);
        let request = GoalRequest {
            goal: goal_hex.clone(),
            extra: Some("Work in the lane.".to_owned()),
            workdir: f.directory.path().join("lane"),
            effort: "high".to_owned(),
            spawner: "agent-cc".to_owned(),
            provider: Provider::Claude,
            cover: CoverWish::default(),
        };
        let launched = f.spawn.goal(&request).unwrap();
        let view = f.spawn.show(&launched.name).unwrap();
        let brief = view.brief_text.clone().unwrap();
        assert!(brief.starts_with("# Goal: Build the widget\n"), "{brief}");
        assert!(brief.contains("The larger aim"));
        assert!(brief.contains("Decided: use the small design."));
        assert!(brief.contains("check off") || brief.contains("move it to `done`"));
        assert!(brief.trim_end().ends_with("Work in the lane."));
        assert_eq!(view.goal, [goal.goal]);
        let mut revision = view.goal_revision.clone();
        revision.sort();
        let mut expected = vec![goal.status_event, goal.note.unwrap()];
        expected.sort();
        assert_eq!(revision, expected);
        // The tint is the title and its purpose, not the brief's opening.
        let prompt = std::fs::read_to_string(
            f.launcher.started.lock().unwrap()[0]
                .script
                .with_extension("prompt"),
        )
        .unwrap();
        assert!(prompt.contains("STUB-WAKE about Build the widget. Serves: The larger aim"));

        // The zooid noted its launch on the goal, as itself.
        let note = launched.goal_note.expect("a launch note");
        assert!(launched.warnings.is_empty(), "{:?}", launched.warnings);
        assert_eq!(view.runs[0].goal_note, [note]);
        let reading = goals.reading(&goal_hex).unwrap();
        let launch_note = reading.notes.iter().find(|n| n.id == note).unwrap();
        assert!(launch_note.text.starts_with(&format!(
            "spawned {} run {:x} from goal revision",
            launched.name, launched.run
        )));
        assert_eq!(launch_note.by, [launched.persona]);

        // Settling does not move the goal.
        f.settle(&launched.name, &launched).unwrap();
        let after = goals.reading(&goal_hex).unwrap();
        assert_eq!(after.status, Some((goal.status_event, "doing".to_owned())));

        // The launch note is a new revision: the same goal renders a new zooid.
        let again = f.spawn.goal(&request).unwrap();
        assert_ne!(again.name, launched.name);
    }

    #[test]
    fn the_unit_pins_every_route_the_launch_resolved_and_settles_through_them() {
        let f = fixture();
        let routes: crate::storage::CollectionRoutes = f
            .storage
            .with_store(|store, signer, _| {
                crate::collection_names::table()
                    .into_iter()
                    .map(|(scope, name)| {
                        let policy =
                            crate::collection_names::private_policy(signer.verifying_key());
                        Ok((scope, store.collection(name, policy)?.handle()))
                    })
                    .collect()
            })
            .unwrap();
        let routed = f.storage.clone().with_collections(routes.clone());
        let spawn = Spawn::new(routed, f.spawn.launch.clone())
            .with_launcher(f.launcher.clone())
            .with_renderer(Arc::new(StubRenderer::default()));
        let launched = spawn.run(&f.request("Routed task.")).unwrap();
        let script = f.script(0);
        for (scope, handle) in &routes {
            let line = format!(
                "export {}=blake3:{}\n",
                crate::collection_names::override_env_name(*scope),
                hex::encode(handle.raw)
            );
            assert!(script.contains(&line), "missing {line}");
        }
        spawn
            .settle(&SettleRequest {
                name: launched.name.clone(),
                run: launched.run,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: None,
                usage: None,
            })
            .unwrap();
        assert!(spawn.show(&launched.name).unwrap().runs[0].message.len() == 1);
    }

    // Colony prefixes (goal 9b274eb2). A colony is a Relations group marked
    // with KIND_COLONY; a new zooid's name starts with its label.

    fn colony(f: &Fixture, name: &str, members: &[&str]) -> Id {
        let relations = Relations::with_storage(f.storage.clone());
        let group = relations.group_create(name).unwrap().group;
        for member in members {
            relations.group_add(name, member).unwrap();
        }
        assert!(matches!(
            relations.group_colony(name).unwrap(),
            crate::relations::ColonyMark::Marked(id) if id == group
        ));
        group
    }

    fn retire(f: &Fixture, name: &str) {
        Relations::with_storage(f.storage.clone())
            .retire(name)
            .unwrap();
    }

    fn proquints_of(brief: &str) -> String {
        super::super::name_for(super::super::text_handle(brief))
    }

    #[test]
    fn a_spawner_in_one_colony_names_its_zooid_after_it() {
        let f = fixture();
        let group = colony(&f, "aurora", &["agent-cc"]);
        let launched = f.spawn.run(&f.request("Colony task.")).unwrap();
        assert_eq!(
            launched.name,
            format!("aurora-{}", proquints_of("Colony task."))
        );
        assert_eq!(launched.unprefixed, None);
        assert_eq!(
            f.launcher.started.lock().unwrap()[0].unit,
            format!("spawn-aurora-{}", proquints_of("Colony task."))
        );
        // The persona and the stored name are the prefixed name.
        assert!(f.persona(&launched.name).is_some());
        let view = f.spawn.show(&launched.name).unwrap();
        assert_eq!(view.names, [launched.name.clone()]);
        assert_eq!(view.origin_colony, [group]);
        assert!(render_show(&view, false).contains(&format!("origin colony: {group:x}")));
        // The prefix grants nothing: the zooid is not enrolled in the group.
        let zooid = f.persona(&launched.name).unwrap().person;
        let membership = Relations::with_storage(f.storage.clone())
            .colonies_of(zooid)
            .unwrap();
        assert!(membership.colonies.is_empty());
        // An unmarked group the spawner is in does not count.
        Relations::with_storage(f.storage.clone())
            .group_create("crew")
            .unwrap();
        Relations::with_storage(f.storage.clone())
            .group_add("crew", "agent-cc")
            .unwrap();
        let other = f.spawn.run(&f.request("Another colony task.")).unwrap();
        assert!(other.name.starts_with("aurora-"), "{}", other.name);
    }

    #[test]
    fn no_colony_or_two_colonies_give_no_prefix_and_say_why() {
        let f = fixture();
        let none = f.spawn.run(&f.request("Lonely task.")).unwrap();
        assert_eq!(none.name, proquints_of("Lonely task."));
        let why = none.unprefixed.expect("the missing prefix is explained");
        assert!(why.contains("no colony group"), "{why}");
        assert!(f.spawn.show(&none.name).unwrap().origin_colony.is_empty());

        colony(&f, "aurora", &["agent-cc"]);
        colony(&f, "juno", &["agent-cc"]);
        let two = f.spawn.run(&f.request("Torn task.")).unwrap();
        assert_eq!(two.name, proquints_of("Torn task."));
        let why = two.unprefixed.expect("the missing prefix is explained");
        assert!(why.contains("2 colony groups"), "{why}");
        assert!(why.contains("aurora") && why.contains("juno"), "{why}");
        assert!(f.spawn.show(&two.name).unwrap().origin_colony.is_empty());
    }

    #[test]
    fn a_colony_name_unfit_for_a_zooid_name_gives_no_prefix() {
        let f = fixture();
        colony(&f, "the colony", &["agent-cc"]);
        let launched = f.spawn.run(&f.request("Spaced task.")).unwrap();
        assert_eq!(launched.name, proquints_of("Spaced task."));
        assert!(launched.unprefixed.unwrap().contains("not fit"));
    }

    #[test]
    fn a_relaunch_reuses_the_stored_name() {
        let f = fixture();
        colony(&f, "aurora", &["agent-cc"]);
        let first = f.spawn.run(&f.request("Twice task.")).unwrap();
        assert!(first.name.starts_with("aurora-"));
        retire(&f, &first.name);
        // The colony is renamed and the spawner leaves it: the name stays.
        let relations = Relations::with_storage(f.storage.clone());
        relations.group_rename("aurora", "sol").unwrap();
        relations.group_remove("sol", "agent-cc").unwrap();
        let again = f.spawn.run(&f.request("Twice task.")).unwrap();
        assert_eq!(again.name, first.name);
        assert_eq!(again.persona, first.persona);
        assert_eq!(again.unprefixed, None);
        let view = f.spawn.show(&first.name).unwrap();
        assert_eq!(view.names, [first.name.clone()]);
        assert_eq!(view.runs.len(), 2);
    }

    #[test]
    fn an_old_unprefixed_zooid_keeps_its_name_on_relaunch() {
        let f = fixture();
        // Named before any colony existed, as every zooid before today was.
        let old = f.spawn.run(&f.request("Old task.")).unwrap();
        assert_eq!(old.name, proquints_of("Old task."));
        retire(&f, &old.name);
        colony(&f, "aurora", &["agent-cc"]);
        let again = f.spawn.run(&f.request("Old task.")).unwrap();
        assert_eq!(again.name, old.name);
        assert_eq!(again.unprefixed, None);
        let view = f.spawn.show(&old.name).unwrap();
        assert_eq!(view.names, [old.name.clone()]);
        assert!(view.origin_colony.is_empty());
    }

    #[test]
    fn a_child_of_a_task_zooid_inherits_the_origin_colony() {
        let f = fixture();
        let group = colony(&f, "aurora", &["agent-cc"]);
        let parent = f.spawn.run(&f.request("Parent task.")).unwrap();
        assert!(parent.name.starts_with("aurora-"));
        // The parent is not a colony member; its child is named from the
        // parent's recorded origin colony, not from its name.
        let mut child_request = f.request("Child task.");
        child_request.spawner = parent.name.clone();
        let child = f.spawn.run(&child_request).unwrap();
        assert_eq!(
            child.name,
            format!("aurora-{}", proquints_of("Child task."))
        );
        assert_eq!(child.unprefixed, None);
        assert_eq!(f.spawn.show(&child.name).unwrap().origin_colony, [group]);
        // A grandchild inherits it through the child.
        let mut grandchild_request = f.request("Grandchild task.");
        grandchild_request.spawner = child.name.clone();
        let grandchild = f.spawn.run(&grandchild_request).unwrap();
        assert!(
            grandchild.name.starts_with("aurora-"),
            "{}",
            grandchild.name
        );
        // A zooid with no recorded origin passes none on.
        let mut orphan_spawner = f.request("Unnamed parent.");
        orphan_spawner.spawner = "agent-cc".to_owned();
        Relations::with_storage(f.storage.clone())
            .group_remove("aurora", "agent-cc")
            .unwrap();
        let unprefixed = f.spawn.run(&orphan_spawner).unwrap();
        assert!(unprefixed.unprefixed.is_some());
        let mut orphan_child = f.request("Orphan child.");
        orphan_child.spawner = unprefixed.name.clone();
        let orphan = f.spawn.run(&orphan_child).unwrap();
        assert_eq!(orphan.name, proquints_of("Orphan child."));
        assert!(orphan.unprefixed.unwrap().contains("no zooid it ran as"));
    }

    #[test]
    fn two_names_from_a_merge_are_both_shown_and_none_is_picked() {
        let f = fixture();
        colony(&f, "aurora", &["agent-cc"]);
        let here = f.spawn.run(&f.request("Merged task.")).unwrap();
        retire(&f, &here.name);
        // A disconnected host named the same brief under its own colony.
        let elsewhere = format!("juno-{}", proquints_of("Merged task."));
        f.spawn
            .publish(super::super::name_fragment(here.spawn, &elsewhere, None))
            .unwrap();
        let mut both = vec![here.name.clone(), elsewhere.clone()];
        both.sort();
        for name in [&here.name, &elsewhere] {
            let view = f.spawn.show(name).unwrap();
            assert_eq!(view.id, here.spawn);
            assert_eq!(view.names, both);
            assert_eq!(view.name, both.join(" | "));
            let shown = render_show(&view, false);
            assert!(shown.contains("name conflict"), "{shown}");
            assert!(
                shown.contains(&here.name) && shown.contains(&elsewhere),
                "{shown}"
            );
        }
        let listed = render_list(&f.spawn.list().unwrap());
        assert!(listed.contains("name conflict: 2 names"), "{listed}");
        // A relaunch refuses to pick one.
        let started = f.launcher.started.lock().unwrap().len();
        let error = f.spawn.run(&f.request("Merged task.")).unwrap_err();
        assert!(format!("{error:#}").contains("2 names"), "{error:#}");
        assert_eq!(f.launcher.started.lock().unwrap().len(), started);
    }
}
