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
use triblespace::core::collection::CollectionStoreExt;
use triblespace::core::metadata;
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::*;

use super::{Exit, HostKey, IntervalValue, RequestFacts, RunFacts, ScriptFacts, TextHandle};
use crate::compass::{Compass, NoteOptions};
use crate::memory_cover::CoverOpts;
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

/// Renders the two pile-derived parts of the opening context.
pub trait Renderer: Send + Sync {
    /// The shared self: beliefs, a memory cover and goals.
    fn wake(&self, storage: &Storage) -> Result<String>;
    /// Recollection chosen for a topic, or the untinted tail without one.
    fn tail(&self, storage: &Storage, about: Option<&str>) -> Result<String>;
}

/// The wake and tail through the Orient and Memory libraries, on the
/// caller's storage, so one pile open serves the whole launch.
#[derive(Clone, Copy, Debug)]
pub struct LibraryRenderer {
    pub wake_chars: usize,
    pub tail_chars: usize,
}

impl Default for LibraryRenderer {
    fn default() -> Self {
        Self {
            wake_chars: 200_000,
            tail_chars: 40_000,
        }
    }
}

impl Renderer for LibraryRenderer {
    fn wake(&self, storage: &Storage) -> Result<String> {
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
                chars: self.wake_chars,
                ..Default::default()
            },
            &mut Out::new(&mut collect),
        )?;
        Ok(text)
    }

    fn tail(&self, storage: &Storage, about: Option<&str>) -> Result<String> {
        let memory = crate::memory::Memory::with_storage(storage.clone());
        let mut options = CoverOpts::plain(self.tail_chars);
        options.about = about.map(str::to_owned);
        Ok(memory.context(&options)?.text)
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
}

/// The placeholders a local executor template may name, each replaced inside
/// a word: the prompt file (also the executor's stdin), the report file it
/// must write, the zooid's name, the effort, and the unit script, which run
/// as `{script} mcp` is the zooid's own faculties MCP server.
pub const LOCAL_PLACEHOLDERS: [&str; 5] =
    ["{prompt}", "{report}", "{name}", "{effort}", "{script}"];

/// What to do when a host has no local executor.
const LOCAL_UNCONFIGURED: &str = "no local executor is configured on this host. Set \
     SPAWN_LOCAL_EXECUTOR to its argv template, words separated by whitespace, with {prompt}, \
     {report}, {name}, {effort} and {script} replaced at launch; the executor reads the prompt \
     on stdin and writes its report to {report}. Nothing falls back to a hosted model";

/// A local executor template from `SPAWN_LOCAL_EXECUTOR`: whitespace
/// separates words, and there is no quoting (a wrapper script holds anything
/// more). Every `{...}` must be one of [`LOCAL_PLACEHOLDERS`], and the report
/// must be named, since a local run's report is only ever read from its file.
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
    if !words.iter().any(|word| word.contains("{report}")) {
        bail!("SPAWN_LOCAL_EXECUTOR never names {{report}}, so a local run could not report");
    }
    Ok(words)
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
            spawn: "spawn".to_owned(),
            faculties: PathBuf::from("faculties"),
            local: None,
            local_max_runtime: None,
        }
    }

    /// [`Self::under_home`] for `$HOME`, read when a frontend calls this,
    /// with the overrides [`Self::override_from`] reads from the environment.
    pub fn from_home_env() -> Result<Self> {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        let mut config = Self::under_home(Path::new(&home));
        config.override_from(|name| std::env::var(name).ok())?;
        Ok(config)
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

/// How long the hook gives `spawn inbox` before the agent stops waiting:
/// one cold pile read takes seconds, and a post hook may first wait for
/// another's turn on the run's inbox state.
pub const INBOX_STOP_TIMEOUT_SECS: u64 = 120;
pub const INBOX_POST_TIMEOUT_SECS: u64 = 60;

/// The hooks every claude zooid gets, as a `--settings` file: its unit
/// script run as `inbox stop` on Stop and `inbox post` after every tool call
/// (no matcher, so every tool). Both run inside the unit, in the CLI's
/// environment, and read the zooid's unread messages with `spawn inbox`.
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
        "# Written by `spawn` for this run. Nothing else is configured here.\n\
         [mcp_servers.faculties]\n\
         command = \"/bin/bash\"\n\
         args = [\"{}\", \"mcp\"]\n\
         startup_timeout_sec = 120\n\
         tool_timeout_sec = 3600\n",
        script.display()
    )
}

/// Why a keeper refuses a request naming mistral.
pub const MISTRAL_KEEP_REFUSAL: &str =
    "mistral spends money; only a direct spawn run may choose it until JP agrees to unattended spend";

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
        "# Written by `spawn` for this run. Nothing else is configured here.\n\
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
         max_context_length = 1048576\n\
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
}

/// Launch a zooid whose brief is rendered from a Compass goal.
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
}

/// What one launch runs from: the brief text, the memory tail's topic, and
/// for `spawn goal`, the goal revision it was rendered from.
struct Brief {
    text: String,
    about: String,
    goal: Option<(Id, Vec<Id>)>,
}

/// What a keeper adds to an ordinary launch: the unit named by the request,
/// the request the run serves, and the handoff delivered after the brief.
#[derive(Clone, Debug, Default)]
struct LaunchExtras {
    unit: Option<String>,
    request: Option<Id>,
    handoff: Option<String>,
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
    /// Why the recollection is untinted, when its topic query failed.
    pub untinted: Option<String>,
    /// The goal note the zooid added at launch (`spawn goal`).
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
    /// The request the run serves, when a keeper started it. Settlement
    /// links the run to it, so a keeper that stopped before publishing the
    /// run record still finds that the request ran.
    pub request: Option<Id>,
    pub exit: Exit,
    /// Claude's stdout. A missing or empty file records no report.
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
    pub name: String,
    pub brief: TextHandle,
    pub brief_text: Option<String>,
    /// The Compass goal the brief was rendered from, and that revision.
    pub goal: Vec<Id>,
    pub goal_revision: Vec<Id>,
    /// Oldest first.
    pub runs: Vec<RunView>,
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
    pub message: Vec<Id>,
    pub settle_failure: Vec<String>,
    pub goal_note: Vec<Id>,
    /// The request the run serves, when a keeper started it.
    pub request: Vec<Id>,
    /// Usage the agent reported, recorded at settlement (mistral runs).
    pub input_tokens: Vec<u64>,
    pub cached_input_tokens: Vec<u64>,
    pub output_tokens: Vec<u64>,
    /// The agent's own cost estimate in US dollars, not the provider's bill.
    pub cost_usd: Vec<f64>,
}

impl RunView {
    pub fn ended(&self) -> bool {
        !self.finished.is_empty()
    }
}

/// Which zooid a request asks for.
#[derive(Clone, Debug)]
pub enum ZooidRef {
    /// An existing spawn, by name.
    Name(String),
    /// A new brief: the request carries the spawn record with it.
    Brief(String),
}

/// `spawn request`: ask one host's keeper to start a run there. It names no
/// workdir, command, tool or budget; the host resolves those itself.
#[derive(Clone, Debug)]
pub struct RequestInput {
    pub zooid: ZooidRef,
    /// The target host's node key. `None` is this host: the signer's key.
    pub host: Option<HostKey>,
    /// A readable name beside the key.
    pub host_label: Option<String>,
    pub provider: Provider,
    pub effort: String,
    /// The asking persona, messaged when the run ends.
    pub spawner: String,
    /// The run this one continues, when it is a move.
    pub previous: Option<Id>,
    /// Delivered once, after the brief, in the run's prompt.
    pub handoff: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Requested {
    pub request: Id,
    pub zooid: Id,
    pub name: String,
    pub host: HostKey,
    pub host_label: Option<String>,
    pub unit: String,
}

/// One outcome a keeper recorded for a request.
#[derive(Clone, Debug)]
pub struct OutcomeView {
    pub id: Id,
    pub kind: Id,
    pub at: Vec<IntervalValue>,
    pub run: Vec<Id>,
    pub unit: Vec<String>,
    pub reason: Vec<String>,
}

/// One request as this observation holds it. Fields that are lists may
/// hold none or several values; the keeper decides what it can act on.
#[derive(Clone, Debug)]
pub struct RequestView {
    pub id: Id,
    pub zooid: Vec<Id>,
    pub host: Vec<HostKey>,
    pub host_label: Vec<String>,
    pub provider: Vec<String>,
    pub effort: Vec<String>,
    pub previous: Vec<Id>,
    pub handoff: Vec<TextHandle>,
    /// The first handoff's text, when its blob is here.
    pub handoff_text: Option<String>,
    pub spawner: Vec<Id>,
    pub requested_at: Vec<IntervalValue>,
    /// The zooid's brief handle and text (when here), and its goal revision.
    pub brief: Option<TextHandle>,
    pub brief_text: Option<String>,
    pub goal: Option<(Id, Vec<Id>)>,
    pub outcomes: Vec<OutcomeView>,
    /// Runs serving this request, and whether each has a recorded end.
    pub runs: Vec<(Id, bool)>,
}

impl RequestView {
    fn has(&self, kind: Id) -> bool {
        self.outcomes.iter().any(|outcome| outcome.kind == kind)
    }

    /// The zooid's name, when its brief is known.
    pub fn name(&self) -> Option<String> {
        self.brief.map(super::name_for)
    }
}

/// What `spawn keep` resolves on its own host, never from a request.
#[derive(Clone, Debug)]
pub struct KeepConfig {
    /// The directory every request's run starts in.
    pub workdir: PathBuf,
    /// This host's keep lock ([`keep_lock_path`]): every pass holds an
    /// exclusive `flock` on it from before it reads the requests until its
    /// last launch or outcome is written.
    pub lock: PathBuf,
}

/// Where this host's keepers take turns: `$XDG_STATE_HOME/spawn/keep.lock`,
/// else `$HOME/.local/state/spawn/keep.lock`. One fixed rule with no
/// fallback beyond that, so every keeper on the host (a `--once` by hand, a
/// loop, a timer) finds the same file. A relative or empty variable names no
/// fixed file (each working directory would get its own), so it counts as
/// unset.
pub fn keep_lock_path() -> Result<PathBuf> {
    let absolute = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    if let Some(state) = absolute("XDG_STATE_HOME") {
        return Ok(state.join("spawn/keep.lock"));
    }
    absolute("HOME")
        .map(|home| home.join(".local/state/spawn/keep.lock"))
        .ok_or_else(|| anyhow!("neither XDG_STATE_HOME nor HOME is an absolute path"))
}

/// One keeper pass's turn on this host: an exclusive `flock` on a file that
/// is never removed, released by the kernel when the value is dropped or
/// its process dies, however it dies. No pid, staleness rule or clock
/// decides anything. Unlinking a locked path is what would let two keepers
/// each lock a different inode of "the same" lock, so nothing unlinks it.
///
/// Without it two passes on one host can both read "no accepted, no run, no
/// unit" and both start the request; and a run that started, exited and was
/// collected leaves no unit name to refuse the second start (compass
/// fd6af1f2 notes f36d6992, 9ca71066).
struct KeepLock {
    _held: std::fs::File,
}

impl KeepLock {
    /// Take the lock, waiting for a pass that holds it to finish.
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open the keep lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => return Ok(Self { _held: file }),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("lock {}", path.display()))
            }
        }
        eprintln!(
            "spawn keep: waiting for another keeper pass on this host ({} is held)",
            path.display()
        );
        file.lock()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(Self { _held: file })
    }
}

/// What one pass did about one request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeepStep {
    /// Started its run.
    Launched { run: Id, unit: String, name: String },
    /// Recorded a refusal, with its reason.
    Refused(String),
    /// Its unit is running; the outcome is recorded.
    Running,
    /// Its run ended and settlement recorded the end.
    Ended,
    /// Nothing can be done yet; the reason says what it waits for.
    Waiting(String),
    /// Accepted earlier, yet neither a run nor a running unit is seen. It is
    /// not started again: its side effects may have happened. The reason
    /// says when it was accepted.
    Uncertain(String),
    /// Already refused or ended: left alone.
    Done,
}

/// One keeper pass over the requests addressed to this host.
#[derive(Clone, Debug)]
pub struct KeepPass {
    pub host: HostKey,
    pub steps: Vec<(Id, KeepStep)>,
}

/// The least time between two inbox reads by one run's post hook.
pub const INBOX_POST_INTERVAL_SECS: u64 = 60;

/// The longest message body an inbox hook shows; a longer one is cut, and
/// says where the whole of it is.
pub const INBOX_BODY_CHARS: usize = 4000;

/// Which claude hook `spawn inbox` answers.
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
        "spawn inbox: the inbox could not be read: {error:#}"
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
                    format!("{cut}\n[cut at {INBOX_BODY_CHARS} characters; `orient_poll` shows the whole message]")
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
            LaunchExtras::default(),
        )
    }

    /// Launch the zooid for a Compass goal's current revision. The brief is
    /// rendered from the goal ([`super::goal_brief`]); the spawn records the
    /// goal and the revision, and the zooid notes its launch on the goal. The
    /// goal's status is the zooid's to move: nothing here touches it.
    pub fn goal(&self, request: &GoalRequest) -> Result<Launched> {
        let reading = Compass::with_storage(self.storage.clone())
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
            LaunchExtras::default(),
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
                    "--output-format".to_owned(),
                    "text".to_owned(),
                ],
                env: Vec::new(),
                secrets: Vec::new(),
                stdout_is_report: true,
            },
            Provider::Local => {
                let Some(template) = &self.launch.local else {
                    bail!("{LOCAL_UNCONFIGURED}");
                };
                let values = [
                    ("{prompt}", text(&files.prompt)),
                    ("{report}", text(&files.report)),
                    ("{name}", files.name.clone()),
                    ("{effort}", effort.to_owned()),
                    ("{script}", text(&files.script)),
                ];
                let argv = template
                    .iter()
                    .map(|word| {
                        values
                            .iter()
                            .fold(word.clone(), |word, (key, value)| word.replace(key, value))
                    })
                    .collect();
                // The executor writes the report file; its stdout goes to the
                // unit's log. A missing report is settled as no report.
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

    fn launch(
        &self,
        brief: Brief,
        workdir: &Path,
        effort: &str,
        spawner_label: &str,
        provider: Provider,
        extras: LaunchExtras,
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
        let (provider, provider_reason) = self.resolve_provider(provider)?;
        let (spawn_fragment, spawn, brief_handle, name) = super::spawn_fragment(&brief.text)?;
        let unit = extras
            .unit
            .clone()
            .unwrap_or_else(|| super::unit_for(&name));
        let launch = &self.launch;
        let files = [
            ("prompt", launch.file(&name, "prompt")),
            ("script", launch.file(&name, "sh")),
            ("report", launch.file(&name, "report")),
            ("log", launch.file(&name, "log")),
        ];
        for (what, path) in &files {
            super::require_unit_safe(what, &path.display().to_string())?;
        }
        let [(_, prompt_path), (_, script_path), (_, report_path), (_, log_path)] = files;
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

        let relations = Relations::with_storage(self.storage.clone());
        let spawner = match relations.person_state(spawner_label)? {
            Some(state) if !state.retired => state.person,
            Some(_) => bail!("spawner {spawner_label} is retired"),
            None => bail!("no persona matches spawner {spawner_label}"),
        };
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
            let wake = self.renderer.wake(&self.storage)?;
            let about = Some(brief.about.as_str()).filter(|about| !about.trim().is_empty());
            // The tint chooses among equal recollections; it is never worth
            // a launch. Without it the zooid wakes into the untinted tail.
            let (tail, untinted) = match self.renderer.tail(&self.storage, about) {
                Ok(tail) => (tail, None),
                Err(error) if about.is_some() => (
                    self.renderer.tail(&self.storage, None)?,
                    Some(format!("{error:#}")),
                ),
                Err(error) => return Err(error),
            };
            let preamble = super::preamble(&name, spawner_label, provider == Provider::Local);
            let prompt = super::assemble_prompt(
                &wake,
                &tail,
                &preamble,
                &brief.text,
                extras.handoff.as_deref(),
            );
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
                untinted: untinted.clone(),
                request: extras.request,
            });
            let text = |path: &Path| path.display().to_string();
            let agent = self.agent(
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
                },
            )?;
            let script = super::unit_script(&ScriptFacts {
                name: &name,
                run,
                request: extras.request,
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
            })?;
            started = true;
            let mut record = spawn_fragment;
            record += run_fragment;
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
                    "spawned {name} run {run:x} from goal revision {}. `spawn show {name}` reads \
                     it; the goal's status is {name}'s to move.",
                    revision
                        .iter()
                        .map(|id| format!("{id:x}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                match Compass::with_storage(self.storage.clone()).note(
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
                untinted,
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
        let (already_ended, earlier_message, linked) = self.observe(|facts, _| {
            Ok((
                find!(at: IntervalValue, pattern!(facts, [{ run @ metadata::finished_at: ?at }]))
                    .next()
                    .is_some(),
                find!(id: Id, pattern!(facts, [{ run @ attrs::message: ?id }])).next(),
                request.request.map_or(true, |served| {
                    find!(
                        id: Id,
                        pattern!(facts, [{ run @ metadata::tag: KIND_RUN, attrs::request: ?id }])
                    )
                    .any(|id| id == served)
                }),
            ))
        })?;
        let mut failures = Vec::new();
        let mut report_handle = None;
        // The run's link to its request, from inside the unit: launch
        // publishes it only after the unit started, and a keeper that
        // stopped in between would otherwise see no run for the request.
        let link = request
            .request
            .filter(|_| !linked)
            .map(|served| super::run_request_link(run, served));
        if already_ended {
            if let Some(link) = link {
                if let Err(error) = self.publish(link) {
                    failures.push(error.context("link the run to its request"));
                }
            }
        } else {
            let report = match &request.report {
                Some(path) => match std::fs::read(path) {
                    Ok(bytes) if bytes.is_empty() => None,
                    Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        return Err(error).with_context(|| format!("read {}", path.display()))
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
            );
            report_handle = handle;
            let mut fragment = fragment;
            if let Some(link) = link {
                fragment += link;
            }
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
        // A run a keeper started for a request ends that request's task:
        // the requests its record names, and the one the unit was told.
        let mut served = match &recorded {
            Ok(Some(view)) => view.request.clone(),
            _ => Vec::new(),
        };
        served.extend(request.request.filter(|id| !served.contains(id)));
        let unit = match &recorded {
            Ok(Some(view)) => view.unit.first().cloned(),
            _ => None,
        };
        // Without its run record the run names no spawner; the request
        // records who asked, and that is who the run's end is for.
        let request_spawner = || {
            request.request.and_then(|served| {
                self.observe(|facts, _| {
                    Ok(find!(
                        id: Id,
                        pattern!(facts, [{ served @ archive_attrs::author: ?id }])
                    )
                    .next())
                })
                .ok()
                .flatten()
            })
        };
        if !served.is_empty() {
            let recorded_ends = self
                .observe(|facts, _| {
                    Ok(served
                        .iter()
                        .map(|request| has_outcome(facts, *request, OUTCOME_TERMINAL))
                        .collect::<Vec<_>>())
                })
                .unwrap_or_else(|_| vec![false; served.len()]);
            for (request, _) in served
                .iter()
                .zip(recorded_ends)
                .filter(|(_, recorded)| !recorded)
            {
                let unit = unit
                    .clone()
                    .unwrap_or_else(|| super::unit_for_request(*request));
                let (fragment, _) = super::outcome_fragment(
                    *request,
                    OUTCOME_TERMINAL,
                    crate::clock::point_now()?,
                    Some(run),
                    Some(&unit),
                    None,
                );
                if let Err(error) = self.publish(fragment) {
                    failures.push(error.context(format!("record the end of request {request:x}")));
                }
            }
        }
        let (spawner, end, clean, report) = match &recorded {
            Ok(Some(view)) => (
                view.spawner.first().copied().or_else(request_spawner),
                describe_end(view),
                ended_clean(view),
                view.report.first().copied().or(report_handle),
            ),
            Ok(None) => (
                request_spawner(),
                request.exit.describe(),
                request.exit.clean(),
                report_handle,
            ),
            Err(error) => {
                failures.push(anyhow!("read the run back: {error:#}"));
                (
                    request_spawner(),
                    request.exit.describe(),
                    request.exit.clean(),
                    report_handle,
                )
            }
        };
        let mut message = earlier_message;
        if message.is_none() {
            let sent = match spawner {
                Some(spawner) => {
                    let text = format!(
                        "{} finished, {end}, report {}. `spawn show {}` reads it.{}",
                        request.name,
                        match report {
                            None => "none".to_owned(),
                            Some(report) if clean => super::handle_text(report),
                            // Whatever a failed or stopped run left is not
                            // its finished answer.
                            Some(report) => format!(
                                "{} (partial: the run did not end cleanly)",
                                super::handle_text(report)
                            ),
                        },
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

    /// Write one run request into the spawn collection. For a new brief the
    /// spawn record travels in the same commit, so the request never names
    /// a zooid its readers lack.
    pub fn request(&self, input: &RequestInput) -> Result<Requested> {
        if !super::EFFORTS.contains(&input.effort.as_str()) {
            bail!(
                "effort {:?} is not one of {}",
                input.effort,
                super::EFFORTS.join(", ")
            );
        }
        let spawner =
            match Relations::with_storage(self.storage.clone()).person_state(&input.spawner)? {
                Some(state) if !state.retired => state.person,
                Some(_) => bail!("spawner {} is retired", input.spawner),
                None => bail!("no persona matches spawner {}", input.spawner),
            };
        let (mut fragment, zooid, name) = match &input.zooid {
            ZooidRef::Name(name) => {
                let view = self.show(name)?;
                (Fragment::empty(), view.id, view.name)
            }
            ZooidRef::Brief(brief) => {
                let (fragment, id, _, name) = super::spawn_fragment(brief)?;
                (fragment, id, name)
            }
        };
        let host = match input.host {
            Some(host) => host,
            None => self.local_host()?,
        };
        let (request, id) = super::request_fragment(&RequestFacts {
            zooid,
            host,
            host_label: input.host_label.clone(),
            provider: input.provider.name().to_owned(),
            effort: input.effort.clone(),
            previous: input.previous,
            handoff: input.handoff.clone(),
            requested_at: crate::clock::point_now()?,
            spawner,
        });
        fragment += request;
        self.publish(fragment)?;
        Ok(Requested {
            request: id,
            zooid,
            name,
            host,
            host_label: input.host_label.clone(),
            unit: super::unit_for_request(id),
        })
    }

    /// This host's node key: the key this store signs with.
    pub fn local_host(&self) -> Result<HostKey> {
        self.storage
            .with_store(|_, signer, _| Ok(signer.verifying_key().to_inline()))
    }

    /// Every request, or only those addressed to `host`, oldest first.
    pub fn requests(&self, host: Option<HostKey>) -> Result<Vec<RequestView>> {
        self.observe(|facts, reader| Ok(requests_in(facts, reader, host)))
    }

    /// One keeper pass: reconcile every request addressed to this host
    /// against its unit and the pile. A request is only ever seen here when
    /// its COMMIT's signer is admitted to write the spawn collection: the
    /// collection read applies that, so no allowlist is kept beside it.
    ///
    /// The whole pass holds this host's keep lock ([`KeepLock`]), and reads
    /// the requests only after taking it: a pass that waited reads what the
    /// pass before it wrote, so it sees that pass's `accepted`, run and unit.
    pub fn keep_once(&self, config: &KeepConfig) -> Result<KeepPass> {
        let _turn = KeepLock::acquire(&config.lock)?;
        let host = self.local_host()?;
        let mut steps = Vec::new();
        for view in self.requests(Some(host))? {
            let step = match self.keep_one(config, &view) {
                Ok(step) => step,
                Err(error) => KeepStep::Waiting(format!("this pass failed: {error:#}")),
            };
            steps.push((view.id, step));
        }
        Ok(KeepPass { host, steps })
    }

    /// [`Self::keep_once`] every `poll`, forever. A failed pass is reported
    /// and the next one runs anyway.
    pub fn keep(
        &self,
        config: &KeepConfig,
        poll: Duration,
        mut report: impl FnMut(Result<&KeepPass>),
    ) -> ! {
        loop {
            match self.keep_once(config) {
                Ok(pass) => report(Ok(&pass)),
                Err(error) => report(Err(error)),
            }
            std::thread::sleep(poll);
        }
    }

    fn outcome(
        &self,
        view: &RequestView,
        kind: Id,
        run: Option<Id>,
        reason: Option<&str>,
    ) -> Result<()> {
        if view.has(kind) {
            return Ok(());
        }
        let unit = super::unit_for_request(view.id);
        let (fragment, _) = super::outcome_fragment(
            view.id,
            kind,
            crate::clock::point_now()?,
            run,
            Some(&unit),
            reason,
        );
        self.publish(fragment)
    }

    fn refuse(&self, view: &RequestView, reason: String) -> Result<KeepStep> {
        self.outcome(view, OUTCOME_REFUSED, None, Some(&reason))?;
        Ok(KeepStep::Refused(reason))
    }

    fn keep_one(&self, config: &KeepConfig, view: &RequestView) -> Result<KeepStep> {
        if view.has(OUTCOME_REFUSED) || view.has(OUTCOME_TERMINAL) {
            return Ok(KeepStep::Done);
        }
        let unit = super::unit_for_request(view.id);
        let running = is_running(&self.launcher.state(&unit)?);
        if let Some(&(run, _)) = view.runs.first() {
            // A run was started for it: never start another.
            if running {
                self.outcome(view, OUTCOME_RUNNING, Some(run), None)?;
                return Ok(KeepStep::Running);
            }
            return match view.runs.iter().find(|(_, ended)| *ended) {
                Some(&(run, _)) => {
                    self.outcome(view, OUTCOME_TERMINAL, Some(run), None)?;
                    Ok(KeepStep::Ended)
                }
                None => Ok(KeepStep::Waiting(format!(
                    "{unit} is not running and run {run:x} has no recorded end: its settlement did not finish. Settle it again; this keeper does not restart it."
                ))),
            };
        }
        if running {
            // Started by an earlier pass that stopped before the run record
            // was published. systemd holds the one launch; do not start another.
            self.outcome(
                view,
                OUTCOME_RUNNING,
                None,
                Some("the unit is running but its run record is not here yet"),
            )?;
            return Ok(KeepStep::Running);
        }
        if let Some(accepted) = view
            .outcomes
            .iter()
            .find(|outcome| outcome.kind == OUTCOME_ACCEPTED)
        {
            // Accepted is recorded before the unit starts. A keeper that
            // stopped after starting it, with a settlement that never linked
            // the run, leaves exactly this; the absence of a process is not
            // evidence that starting again is safe.
            let at = accepted
                .at
                .first()
                .map(|at| crate::care::format_time(*at))
                .unwrap_or_else(|| "an unrecorded time".to_owned());
            return Ok(KeepStep::Uncertain(format!(
                "accepted at {at}, but no run and no unit were seen; not restarting, because \
                 its side effects may have happened"
            )));
        }
        if !view.handoff.is_empty() {
            return self.refuse(
                view,
                "a handoff needs the disclosure check, which this keeper does not have".to_owned(),
            );
        }
        let provider = match view.provider.as_slice() {
            [one] => match Provider::parse(one) {
                Ok(provider) => provider,
                Err(error) => return self.refuse(view, format!("{error:#}")),
            },
            [] => return self.refuse(view, "the request names no provider".to_owned()),
            several => {
                return self.refuse(
                    view,
                    format!(
                        "the request names {} providers: {}",
                        several.len(),
                        several.join(", ")
                    ),
                )
            }
        };
        if provider == Provider::Local && self.launch.local.is_none() {
            return self.refuse(
                view,
                "no local executor on this host (SPAWN_LOCAL_EXECUTOR is unset)".to_owned(),
            );
        }
        // A request is started unattended, by whatever wrote it (a rule, an
        // ignite pass). Until JP agrees to unattended spending, money is
        // only spent by a direct `spawn run --provider mistral`.
        if provider == Provider::Mistral {
            return self.refuse(view, MISTRAL_KEEP_REFUSAL.to_owned());
        }
        let effort = match view.effort.as_slice() {
            [one] if super::EFFORTS.contains(&one.as_str()) => one.clone(),
            [one] => {
                return self.refuse(
                    view,
                    format!("effort {one:?} is not one of {}", super::EFFORTS.join(", ")),
                )
            }
            other => {
                return self.refuse(view, format!("the request names {} efforts", other.len()))
            }
        };
        let spawner = match view.spawner.as_slice() {
            [one] => *one,
            other => {
                return self.refuse(
                    view,
                    format!("the request names {} spawners; it needs one", other.len()),
                )
            }
        };
        if view.zooid.len() != 1 {
            return self.refuse(
                view,
                format!(
                    "the request names {} zooids; it needs one",
                    view.zooid.len()
                ),
            );
        }
        // Absence here is not knowledge: records and blobs may still arrive.
        let (Some(brief), Some(text)) = (view.brief, view.brief_text.clone()) else {
            return Ok(KeepStep::Waiting(
                "the zooid's spawn record or brief is not here yet".to_owned(),
            ));
        };
        let name = super::name_for(brief);
        if let Some(state) = Relations::with_storage(self.storage.clone()).person_state(&name)? {
            if !state.retired {
                return Ok(KeepStep::Waiting(format!(
                    "{name} is still running elsewhere (its persona is live)"
                )));
            }
        }
        self.outcome(view, OUTCOME_ACCEPTED, None, None)?;
        let launched = self.launch(
            Brief {
                about: super::about(&text).to_owned(),
                text,
                goal: view.goal.clone(),
            },
            &config.workdir,
            &effort,
            &format!("{spawner:x}"),
            provider,
            LaunchExtras {
                unit: Some(unit.clone()),
                request: Some(view.id),
                // Requests with a handoff are refused above.
                handoff: None,
            },
        );
        match launched {
            Ok(launched) => {
                self.outcome(view, OUTCOME_RUNNING, Some(launched.run), None)?;
                Ok(KeepStep::Launched {
                    run: launched.run,
                    unit,
                    name,
                })
            }
            Err(error) => {
                if is_running(&self.launcher.state(&unit)?) {
                    // It started; only recording it failed. The next pass
                    // reconciles; refusing now would hide a live run.
                    return Err(error);
                }
                self.refuse(view, format!("the launch failed: {error:#}"))
            }
        }
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
        self.storage.with_store(|pile, signer, runtime| {
            let source = self.storage.open_collection_read(
                pile,
                DEFAULT_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
            let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
            runtime
                .block_on(async {
                    crate::storage::tolerate_own_lag(
                        pile.maintain_attached(succinct, signer).await,
                    )?;
                    crate::storage::tolerate_own_lag(pile.maintain_attached(rank9, signer).await)
                })
                .context("maintain Spawn fact collection")?;
            let snapshot = AcquiringReader::new(pile.snapshot()?, Arc::clone(runtime));
            let facts = crate::storage::acquire_facts(&snapshot, rank9)
                .context("read Spawn Rank9 collection")?;
            read(&facts, &snapshot)
        })
    }

    fn publish(&self, fragment: Fragment) -> Result<()> {
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
                "spawn list",
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

fn read_text<R: BlobStoreGet + ?Sized>(reader: &R, handle: TextHandle) -> Option<String> {
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

/// Every request in these facts, or only those addressed to `host`, oldest
/// first. [`Spawn::requests`] over facts a caller already holds.
pub(super) fn requests_in<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    host: Option<HostKey>,
) -> Vec<RequestView> {
    let mut ids = find!(
        (request: Id, at: IntervalValue),
        pattern!(facts, [{ ?request @
            metadata::tag: KIND_REQUEST,
            metadata::created_at: ?at,
        }])
    )
    .collect::<Vec<_>>();
    ids.sort_by_key(|(request, at)| {
        let start = at
            .try_from_inline::<(Epoch, Epoch)>()
            .map(|(start, _)| start.to_tai_duration().total_nanoseconds())
            .unwrap_or(i128::MIN);
        (start, *request)
    });
    // A request with two times sorts by its earliest and is read once.
    let mut seen = std::collections::HashSet::new();
    ids.into_iter()
        .filter(|(request, _)| seen.insert(*request))
        .map(|(request, _)| request_view(facts, reader, request))
        .filter(|view| host.map_or(true, |host| view.host.contains(&host)))
        .collect()
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
    Ok(SpawnView {
        id: spawn,
        name: super::name_for(brief),
        brief,
        brief_text: read_text(reader, brief),
        goal: find!(id: Id, pattern!(facts, [{ spawn @ attrs::goal: ?id }])).collect(),
        goal_revision: find!(id: Id, pattern!(facts, [{ spawn @ attrs::goal_revision: ?id }]))
            .collect(),
        runs,
    })
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
    }
}

fn counts(values: Vec<Inline<inlineencodings::U256BE>>) -> Vec<u64> {
    values
        .into_iter()
        .filter_map(|value| value.try_from_inline::<u64>().ok())
        .collect()
}

fn has_outcome(facts: &FactArchive, request: Id, kind: Id) -> bool {
    find!(
        outcome: Id,
        pattern!(facts, [{ ?outcome @
            metadata::tag: KIND_OUTCOME,
            attrs::request: request,
            attrs::outcome: kind,
        }])
    )
    .next()
    .is_some()
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
    let zooid: Vec<Id> =
        find!(id: Id, pattern!(facts, [{ request @ attrs::zooid: ?id }])).collect();
    let handoff: Vec<TextHandle> =
        find!(handle: TextHandle, pattern!(facts, [{ request @ attrs::handoff: ?handle }]))
            .collect();
    let (brief, goal) = match zooid.as_slice() {
        [spawn] => {
            let spawn = *spawn;
            let brief = find!(
                handle: TextHandle,
                pattern!(facts, [{ spawn @ metadata::tag: KIND_SPAWN, attrs::brief: ?handle }])
            )
            .next();
            let goal = find!(id: Id, pattern!(facts, [{ spawn @ attrs::goal: ?id }])).next();
            let revision =
                find!(id: Id, pattern!(facts, [{ spawn @ attrs::goal_revision: ?id }])).collect();
            (brief, goal.map(|goal| (goal, revision)))
        }
        _ => (None, None),
    };
    let outcomes = find!(
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
        unit: texts(
            find!(handle: TextHandle, pattern!(facts, [{ outcome @ attrs::unit: ?handle }]))
                .collect(),
        ),
        reason: texts(
            find!(handle: TextHandle, pattern!(facts, [{ outcome @ attrs::reason: ?handle }]))
                .collect(),
        ),
    })
    .collect();
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
        handoff_text: handoff
            .first()
            .and_then(|handle| read_text(reader, *handle)),
        handoff,
        spawner: find!(id: Id, pattern!(facts, [{ request @ archive_attrs::author: ?id }]))
            .collect(),
        requested_at: find!(
            at: IntervalValue,
            pattern!(facts, [{ request @ metadata::created_at: ?at }])
        )
        .collect(),
        brief_text: brief.and_then(|handle| read_text(reader, handle)),
        brief,
        goal,
        zooid,
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

/// Requests with their outcomes, one block each.
pub fn render_requests(requests: &[RequestView]) -> String {
    let mut out = String::new();
    if requests.is_empty() {
        out.push_str("No requests.\n");
    }
    for request in requests {
        writeln!(
            out,
            "request {:x}  zooid {}  host {}{}",
            request.id,
            request.name().unwrap_or_else(|| ids(&request.zooid)),
            join(
                &request
                    .host
                    .iter()
                    .map(|h| super::host_text(*h))
                    .collect::<Vec<_>>()
            ),
            if request.host_label.is_empty() {
                String::new()
            } else {
                format!(" ({})", join(&request.host_label))
            }
        )
        .unwrap();
        writeln!(
            out,
            "  provider {}  effort {}  unit {}",
            join(&request.provider),
            join(&request.effort),
            super::unit_for_request(request.id)
        )
        .unwrap();
        if !request.previous.is_empty() {
            writeln!(out, "  previous run: {}", ids(&request.previous)).unwrap();
        }
        if !request.handoff.is_empty() {
            writeln!(
                out,
                "  handoff: {}",
                join(
                    &request
                        .handoff
                        .iter()
                        .map(|h| super::handle_text(*h))
                        .collect::<Vec<_>>()
                )
            )
            .unwrap();
        }
        writeln!(out, "  spawner: {}", ids(&request.spawner)).unwrap();
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
            writeln!(out, "  no outcome yet").unwrap();
        } else if request.runs.is_empty()
            && request.has(OUTCOME_ACCEPTED)
            && !request.has(OUTCOME_REFUSED)
            && !request.has(OUTCOME_TERMINAL)
        {
            writeln!(
                out,
                "  accepted, but no run is recorded: once its unit is gone the keeper reports \
                 it uncertain and never starts it again"
            )
            .unwrap();
        }
    }
    out
}

/// One line per request a pass looked at.
pub fn render_pass(pass: &KeepPass) -> String {
    let mut out = format!(
        "keep pass on host {}: {} request(s) addressed here\n",
        super::host_text(pass.host),
        pass.steps.len()
    );
    for (request, step) in &pass.steps {
        let what = match step {
            KeepStep::Launched { run, unit, name } => {
                format!("launched {name} in {unit} (run {run:x})")
            }
            KeepStep::Refused(why) => format!("refused: {why}"),
            KeepStep::Running => "running".to_owned(),
            KeepStep::Ended => "task-terminal".to_owned(),
            KeepStep::Waiting(why) => format!("waiting: {why}"),
            KeepStep::Uncertain(why) => format!("uncertain: {why}"),
            KeepStep::Done => "done".to_owned(),
        };
        writeln!(out, "  {request:x}: {what}").unwrap();
    }
    out
}

fn join<T: std::fmt::Display>(values: &[T]) -> String {
    if values.is_empty() {
        return "-".to_owned();
    }
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn ids(values: &[Id]) -> String {
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

/// Whether a recorded run ended as a success: exit 0 alone, and not stopped
/// by its runtime limit.
pub fn ended_clean(run: &RunView) -> bool {
    run.exit_code == [0]
        && run.signal.is_empty()
        && !run
            .result
            .iter()
            .any(|result| result == super::TIMEOUT_RESULT)
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
        writeln!(
            out,
            "{}  runs {}  latest {}  {}",
            spawn.name,
            spawn.runs.len(),
            latest,
            first_line
        )
        .unwrap();
    }
    out
}

/// A spawn with every run; `with_brief` adds the brief itself.
pub fn render_show(spawn: &SpawnView, with_brief: bool) -> String {
    let mut out = String::new();
    writeln!(out, "spawn: {} ({:x})", spawn.name, spawn.id).unwrap();
    writeln!(out, "brief: {}", super::handle_text(spawn.brief)).unwrap();
    if !spawn.goal.is_empty() {
        writeln!(out, "goal: {}", ids(&spawn.goal)).unwrap();
        writeln!(out, "goal revision: {}", ids(&spawn.goal_revision)).unwrap();
    }
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
        /// Start the unit, then stop the caller the way a crashed keeper
        /// stops: before anything after the start runs.
        crash_after_start: bool,
        /// The unit ends and systemd forgets it as soon as it starts, so its
        /// name can refuse nothing afterwards.
        collected_at_once: bool,
    }

    impl Launcher for FakeLauncher {
        fn start(&self, unit: &UnitSpec) -> Result<()> {
            if self.refuse {
                bail!("fake systemd refuses");
            }
            self.started.lock().unwrap().push(unit.clone());
            *self.state.lock().unwrap() = if self.collected_at_once {
                "inactive".to_owned()
            } else {
                "active".to_owned()
            };
            if self.crash_after_start {
                panic!("the keeper stops after the unit started");
            }
            Ok(())
        }
        fn state(&self, _unit: &str) -> Result<String> {
            Ok(self.state.lock().unwrap().clone())
        }
    }

    #[derive(Default)]
    struct StubRenderer {
        /// Refuse a tinted tail the way WeMM refuses an over-long query.
        refuse_tint: bool,
    }

    impl Renderer for StubRenderer {
        fn wake(&self, _storage: &Storage) -> Result<String> {
            Ok("STUB-WAKE".to_owned())
        }
        fn tail(&self, _storage: &Storage, about: Option<&str>) -> Result<String> {
            match about {
                Some(_) if self.refuse_tint => bail!("WeMM text preparation: too long"),
                Some(about) => Ok(format!("STUB-TAIL about {about}")),
                None => Ok("STUB-TAIL untinted".to_owned()),
            }
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
            spawn: "spawn".to_owned(),
            faculties: PathBuf::from("faculties"),
            local: None,
            local_max_runtime: None,
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
            }
        }
        fn settle(&self, name: &str, launched: &Launched) -> Result<Settled> {
            self.spawn.settle(&SettleRequest {
                name: name.to_owned(),
                run: launched.run,
                request: None,
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

    impl Fixture {
        fn ask(&self, brief: &str, host: Option<HostKey>, provider: Provider) -> Requested {
            self.ask_with(brief, host, provider, None)
        }
        fn ask_with(
            &self,
            brief: &str,
            host: Option<HostKey>,
            provider: Provider,
            handoff: Option<&str>,
        ) -> Requested {
            self.spawn
                .request(&RequestInput {
                    zooid: ZooidRef::Brief(brief.to_owned()),
                    host,
                    host_label: Some("testhost".to_owned()),
                    provider,
                    effort: "medium".to_owned(),
                    spawner: "agent-cc".to_owned(),
                    previous: None,
                    handoff: handoff.map(str::to_owned),
                })
                .unwrap()
        }
        fn keep(&self) -> KeepPass {
            self.spawn.keep_once(&self.keep_config()).unwrap()
        }
        fn keep_config(&self) -> KeepConfig {
            KeepConfig {
                workdir: self.directory.path().join("lane"),
                lock: self.directory.path().join("state/spawn/keep.lock"),
            }
        }
        fn request_view(&self, id: Id) -> RequestView {
            self.spawn
                .requests(None)
                .unwrap()
                .into_iter()
                .find(|view| view.id == id)
                .expect("the request is here")
        }
    }

    fn other_host() -> HostKey {
        ed25519_dalek::SigningKey::from_bytes(&[42; 32])
            .verifying_key()
            .to_inline()
    }

    fn kinds(view: &RequestView) -> Vec<String> {
        let mut kinds: Vec<_> = view
            .outcomes
            .iter()
            .map(|outcome| outcome_name(outcome.kind))
            .collect();
        kinds.sort();
        kinds
    }

    #[test]
    fn a_request_round_trips_with_everything_it_asks_for() {
        let f = fixture();
        let here = f.spawn.local_host().unwrap();
        let previous = Id::new([5; 16]).unwrap();
        let requested = f
            .spawn
            .request(&RequestInput {
                zooid: ZooidRef::Brief("Requested task.".to_owned()),
                host: None,
                host_label: Some("sky".to_owned()),
                provider: Provider::Codex,
                effort: "xhigh".to_owned(),
                spawner: "agent-cc".to_owned(),
                previous: Some(previous),
                handoff: Some("Open: the review thread.".to_owned()),
            })
            .unwrap();
        assert_eq!(requested.host, here);
        assert_eq!(requested.unit, format!("spawn-req-{:x}", requested.request));
        let view = f.request_view(requested.request);
        let (_, spawn, brief, name) = super::super::spawn_fragment("Requested task.").unwrap();
        assert_eq!(requested.zooid, spawn);
        assert_eq!(view.zooid, [spawn]);
        assert_eq!(view.brief, Some(brief));
        assert_eq!(view.name().as_deref(), Some(name.as_str()));
        assert_eq!(view.host, [here]);
        assert_eq!(view.host_label, ["sky"]);
        assert_eq!(view.provider, ["codex"]);
        assert_eq!(view.effort, ["xhigh"]);
        assert_eq!(view.previous, [previous]);
        assert_eq!(
            view.handoff_text.as_deref(),
            Some("Open: the review thread.")
        );
        assert_eq!(view.spawner, [f.persona("agent-cc").unwrap().person]);
        assert!(view.outcomes.is_empty() && view.runs.is_empty());
        // The spawn record travelled with it, so the zooid is readable.
        assert_eq!(f.spawn.show(&name).unwrap().id, spawn);
        // An existing zooid can be requested by name.
        let again = f
            .spawn
            .request(&RequestInput {
                zooid: ZooidRef::Name(name.clone()),
                host: Some(other_host()),
                host_label: None,
                provider: Provider::Claude,
                effort: "high".to_owned(),
                spawner: "agent-cc".to_owned(),
                previous: None,
                handoff: None,
            })
            .unwrap();
        assert_eq!(again.zooid, spawn);
        assert_ne!(again.request, requested.request);
        assert_eq!(f.spawn.requests(Some(here)).unwrap().len(), 1);
        assert_eq!(f.spawn.requests(None).unwrap().len(), 2);
        // Nothing has been launched by writing requests.
        assert!(f.launcher.started.lock().unwrap().is_empty());
    }

    #[test]
    fn keep_launches_one_unit_for_a_request_here_and_none_for_another_host() {
        let f = fixture();
        let here = f.ask("Kept task.", None, Provider::Claude);
        let there = f.ask("Elsewhere task.", Some(other_host()), Provider::Claude);
        let pass = f.keep();
        assert_eq!(pass.host, f.spawn.local_host().unwrap());
        assert_eq!(pass.steps.len(), 1, "{pass:?}");
        let started = f.launcher.started.lock().unwrap().clone();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].unit, format!("spawn-req-{:x}", here.request));
        assert_eq!(
            started[0].workdir,
            std::fs::canonicalize(f.directory.path().join("lane")).unwrap()
        );
        let KeepStep::Launched { run, unit, name } = &pass.steps[0].1 else {
            panic!("{pass:?}");
        };
        assert_eq!(unit, &started[0].unit);
        assert_eq!(name, &here.name);
        let view = f.request_view(here.request);
        assert_eq!(kinds(&view), ["accepted", "running"]);
        assert_eq!(view.runs, [(*run, false)]);
        let running = view
            .outcomes
            .iter()
            .find(|o| o.kind == OUTCOME_RUNNING)
            .unwrap();
        assert_eq!(running.run, [*run]);
        // The run is an ordinary run of the zooid, marked with its request.
        let spawn = f.spawn.show(&here.name).unwrap();
        assert_eq!(spawn.runs[0].request, [here.request]);
        assert_eq!(spawn.runs[0].effort, ["medium"]);
        assert_eq!(spawn.runs[0].unit, [unit.clone()]);
        // The request for another host is untouched.
        assert!(f.request_view(there.request).outcomes.is_empty());
        assert!(f.persona(&there.name).is_none());
    }

    #[test]
    fn a_rerun_launches_nothing_new_and_an_ended_run_is_never_restarted() {
        let f = fixture();
        let asked = f.ask("Once only.", None, Provider::Claude);
        let first = f.keep();
        let KeepStep::Launched { run, .. } = first.steps[0].1.clone() else {
            panic!("{first:?}");
        };
        // Still running: reconciled, not relaunched.
        assert_eq!(f.keep().steps, [(asked.request, KeepStep::Running)]);
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
        // The unit is gone but settlement has not run: wait, never restart.
        *f.launcher.state.lock().unwrap() = "inactive".to_owned();
        let pass = f.keep();
        assert!(
            matches!(&pass.steps[0].1, KeepStep::Waiting(why) if why.contains("settle")),
            "{pass:?}"
        );
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
        // Settlement records the end and the request's task-terminal outcome.
        f.spawn
            .settle(&SettleRequest {
                name: asked.name.clone(),
                run,
                request: Some(asked.request),
                exit: Exit::from_systemd("exited", "0", "success"),
                report: None,
                usage: None,
            })
            .unwrap();
        let view = f.request_view(asked.request);
        assert_eq!(kinds(&view), ["accepted", "running", "task-terminal"]);
        let terminal = view
            .outcomes
            .iter()
            .find(|o| o.kind == OUTCOME_TERMINAL)
            .unwrap();
        assert_eq!(terminal.run, [run]);
        assert_eq!(terminal.at.len(), 1);
        // Settling again records no second terminal observation.
        f.spawn
            .settle(&SettleRequest {
                name: asked.name.clone(),
                run,
                request: Some(asked.request),
                exit: Exit::from_systemd("exited", "0", "success"),
                report: None,
                usage: None,
            })
            .unwrap();
        let view = f.request_view(asked.request);
        let terminal = view
            .outcomes
            .iter()
            .find(|o| o.kind == OUTCOME_TERMINAL)
            .unwrap();
        assert_eq!(terminal.at.len(), 1);
        // The persona is retired, yet the ended request is left alone.
        assert_eq!(f.keep().steps, [(asked.request, KeepStep::Done)]);
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
        assert_eq!(f.spawn.show(&asked.name).unwrap().runs.len(), 1);
    }

    #[test]
    fn two_keepers_at_once_start_a_request_once() {
        two_keepers_race(fixture());
    }

    /// The case only a reread under the lock catches: the first pass's run
    /// started, ended and was collected before the second pass looks, so
    /// systemd's state says nothing and only the pile's `accepted` and run
    /// record stop a second start.
    #[test]
    fn two_keepers_at_once_start_a_request_once_even_when_its_unit_is_already_gone() {
        two_keepers_race(fixture_with(FakeLauncher {
            collected_at_once: true,
            ..FakeLauncher::default()
        }));
    }

    fn two_keepers_race(f: Fixture) {
        let asked = f.ask("Raced task.", None, Provider::Claude);
        let barrier = std::sync::Barrier::new(2);
        let passes: Vec<KeepPass> = std::thread::scope(|scope| {
            let keepers: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        f.keep()
                    })
                })
                .collect();
            keepers.into_iter().map(|k| k.join().unwrap()).collect()
        });
        let started = f.launcher.started.lock().unwrap().clone();
        assert_eq!(started.len(), 1, "{passes:?}");
        assert_eq!(started[0].unit, format!("spawn-req-{:x}", asked.request));
        let mut steps: Vec<_> = passes
            .iter()
            .flat_map(|pass| pass.steps.iter().map(|(_, step)| step.clone()))
            .collect();
        steps.sort_by_key(|step| !matches!(step, KeepStep::Launched { .. }));
        assert!(
            matches!(
                steps.as_slice(),
                [
                    KeepStep::Launched { .. },
                    KeepStep::Running | KeepStep::Waiting(_)
                ]
            ),
            "{steps:?}"
        );
        assert_eq!(f.request_view(asked.request).runs.len(), 1);
    }

    /// A pass that waits for the lock reads the requests after it gets it.
    /// While this test holds the lock it plays the other keeper: it starts
    /// the request, and the run ends, is collected and settles, so neither
    /// systemd nor a live persona is left to refuse a second start. Only the
    /// waiting pass's fresh read of `accepted`, the run and its end does.
    #[test]
    fn keep_waits_while_another_pass_holds_the_host_lock_and_rereads_after_it() {
        let f = fixture_with(FakeLauncher {
            collected_at_once: true,
            ..FakeLauncher::default()
        });
        let asked = f.ask("Waiting task.", None, Provider::Claude);
        let config = f.keep_config();
        std::fs::create_dir_all(config.lock.parent().unwrap()).unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&config.lock)
            .unwrap();
        holder.lock().unwrap();
        std::thread::scope(|scope| {
            let keeper = scope.spawn(|| f.keep());
            std::thread::sleep(Duration::from_millis(500));
            assert!(!keeper.is_finished());
            assert!(f.launcher.started.lock().unwrap().is_empty());
            // The other keeper's pass, under the lock this test holds.
            let view = f.request_view(asked.request);
            let KeepStep::Launched { run, name, .. } = f.spawn.keep_one(&config, &view).unwrap()
            else {
                panic!("the holder's pass did not launch");
            };
            f.spawn
                .settle(&SettleRequest {
                    name,
                    run,
                    request: Some(asked.request),
                    exit: Exit::from_systemd("exited", "0", "success"),
                    report: None,
                    usage: None,
                })
                .unwrap();
            drop(holder);
            let pass = keeper.join().unwrap();
            assert!(
                matches!(pass.steps.as_slice(), [(_, KeepStep::Done)]),
                "{pass:?}"
            );
        });
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
        // The lock file stays, for the next pass to lock the same inode.
        assert!(config.lock.exists());
    }

    #[test]
    fn keep_refuses_local_and_never_falls_back_to_a_hosted_model() {
        let f = fixture();
        let asked = f.ask("Local only.", None, Provider::Local);
        let pass = f.keep();
        assert_eq!(
            pass.steps,
            [(
                asked.request,
                KeepStep::Refused(
                    "no local executor on this host (SPAWN_LOCAL_EXECUTOR is unset)".to_owned()
                )
            )]
        );
        assert!(f.launcher.started.lock().unwrap().is_empty());
        let view = f.request_view(asked.request);
        assert_eq!(kinds(&view), ["refused"]);
        assert_eq!(
            view.outcomes[0].reason,
            ["no local executor on this host (SPAWN_LOCAL_EXECUTOR is unset)"]
        );
        assert!(view.runs.is_empty());
        assert!(f.persona(&asked.name).is_none());
        assert_eq!(f.keep().steps, [(asked.request, KeepStep::Done)]);
        assert!(f.launcher.started.lock().unwrap().is_empty());
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
                request: None,
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

    #[test]
    fn a_local_zooid_runs_the_hosts_executor_and_its_report_file_is_the_report() {
        let f = fixture();
        let executor = local_executor(&f, "local-ok", "printf 'local report\\n' > \"$1\"");
        let spawn = f.relaunch(|launch| {
            launch.local = Some(
                parse_local_executor(&format!(
                    "{} {{report}} --name={{name}} --effort {{effort}} --prompt {{prompt}} \
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
        // The report is the executor's file, never its stdout.
        assert!(
            !script.contains(&format!("> {}", launched.report.display())),
            "{script}"
        );

        let run = run_unit_and_settle(&f, &spawn, &launched);
        assert_eq!(run.exit_code, [0]);
        assert_eq!(run.report_text.as_deref(), Some("local report\n"));
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
        assert!(told.contains("finished, exit 0, report blake3:"), "{told}");
        assert!(!told.contains("partial"), "{told}");
    }

    #[test]
    fn a_local_executor_that_fails_is_recorded_as_failed_and_nothing_else_runs() {
        let f = fixture();
        let executor = local_executor(&f, "local-fail", "echo 'out of memory' > \"$1\"; exit 3");
        let spawn = f.relaunch(|launch| {
            launch.local =
                Some(parse_local_executor(&format!("{} {{report}}", executor.display())).unwrap())
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
    fn a_local_executor_that_writes_no_report_settles_with_none() {
        let f = fixture();
        let executor = local_executor(&f, "local-silent", "echo 'stdout is not the report'");
        let spawn = f.relaunch(|launch| {
            launch.local =
                Some(parse_local_executor(&format!("{} {{report}}", executor.display())).unwrap())
        });
        let launched = spawn.run(&local_request(&f, "Silent local work.")).unwrap();
        let run = run_unit_and_settle(&f, &spawn, &launched);
        assert_eq!(run.exit_code, [0]);
        assert!(run.report.is_empty());
        assert!(run.report_text.is_none());
    }

    #[test]
    fn keep_starts_a_local_request_on_a_host_with_an_executor() {
        let f = fixture();
        let spawn = f.relaunch(|launch| {
            launch.local = Some(parse_local_executor("/opt/drive/zooid {report}").unwrap())
        });
        let asked = f.ask("Local by request.", None, Provider::Local);
        let pass = spawn.keep_once(&f.keep_config()).unwrap();
        assert!(
            !matches!(pass.steps[0].1, KeepStep::Refused(_)),
            "{:?}",
            pass.steps
        );
        let started = f.launcher.started.lock().unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(
            started[0].unit,
            super::super::unit_for_request(asked.request)
        );
        drop(started);
        let script = f.script(0);
        assert!(script.contains("/opt/drive/zooid"), "{script}");
        assert!(!script.contains("/fake/"), "{script}");
    }

    #[test]
    fn only_a_local_unit_gets_the_runtime_limit() {
        let f = fixture();
        let spawn = f.relaunch(|launch| {
            launch.local = Some(parse_local_executor("/opt/local {report}").unwrap());
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

    /// The runtime limit through real systemd: a local executor that sleeps
    /// past a two-second limit, and one that catches the SIGTERM, writes a
    /// partial report and exits 0. Neither settles as a success. Skipped
    /// where no systemd user manager answers.
    #[test]
    fn a_local_unit_stopped_at_its_runtime_limit_settles_as_a_failure() {
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
        let settle = f.directory.path().join("bin").join("spawn");
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
            "trap 'echo partial > \"$1\"; exit 0' TERM\nsleep 60 &\nwait",
        );
        let real = |executor: &Path| {
            let mut launch = f.spawn.launch.clone();
            launch.local =
                Some(parse_local_executor(&format!("{} {{report}}", executor.display())).unwrap());
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
            assert!(!exit.clean(), "{args}");
            spawn
                .settle(&SettleRequest {
                    name: launched.name.clone(),
                    run: launched.run,
                    request: None,
                    exit,
                    report: Some(launched.report.clone()),
                    usage: None,
                })
                .unwrap();
            let run = &spawn.show(&launched.name).unwrap().runs[0];
            assert!(!ended_clean(run));
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
                    told.contains("(partial: the run did not end cleanly)"),
                    "{told}"
                ),
                None => assert!(told.contains("report none"), "{told}"),
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
            read(" /x/run  --out={report}\t{prompt} ").unwrap(),
            Some(vec![
                "/x/run".to_owned(),
                "--out={report}".to_owned(),
                "{prompt}".to_owned()
            ])
        );
        for bad in [
            "/x/run {prompt}",
            "/x/run {report} {model}",
            "/x/run {report",
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
    fn keep_refuses_mistral_because_a_request_would_spend_money_unattended() {
        let f = fixture();
        let asked = f.ask("Paid task.", None, Provider::Mistral);
        assert_eq!(
            f.keep().steps,
            [(
                asked.request,
                KeepStep::Refused(MISTRAL_KEEP_REFUSAL.to_owned())
            )]
        );
        assert!(f.launcher.started.lock().unwrap().is_empty());
        let view = f.request_view(asked.request);
        assert_eq!(kinds(&view), ["refused"]);
        assert_eq!(view.outcomes[0].reason, [MISTRAL_KEEP_REFUSAL]);
        assert!(view.runs.is_empty());
        assert!(f.persona(&asked.name).is_none());
        // A direct run may still choose it.
        let mut request = f.request("Paid task, run directly.");
        request.provider = Provider::Mistral;
        f.spawn.run(&request).unwrap();
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_request_from_an_unadmitted_signer_is_not_seen_and_not_launched() {
        let f = fixture();
        let here = f.spawn.local_host().unwrap();
        let spawner = f.persona("agent-cc").unwrap().person;
        let (mut fragment, zooid, _, _) = super::super::spawn_fragment("Forged task.").unwrap();
        let (request, id) = super::super::request_fragment(&RequestFacts {
            zooid,
            host: here,
            host_label: None,
            provider: "claude".to_owned(),
            effort: "high".to_owned(),
            previous: None,
            handoff: None,
            requested_at: crate::clock::point_now().unwrap(),
            spawner,
        });
        fragment += request;
        let stranger = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        f.storage
            .with_store(|pile, signer, runtime| {
                let collection = f.storage.open_collection_write(
                    pile,
                    DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                    runtime,
                )?;
                pile.commit(collection, &stranger, fragment)
                    .context("commit as a stranger")?;
                Ok(())
            })
            .unwrap();
        let pass = f.keep();
        assert!(pass.steps.is_empty(), "{pass:?}");
        assert!(f.launcher.started.lock().unwrap().is_empty());
        assert!(f
            .spawn
            .requests(None)
            .unwrap()
            .iter()
            .all(|view| view.id != id));
        // The same record from the admitted signer is kept.
        let admitted = f.ask("Forged task.", None, Provider::Claude);
        assert_eq!(f.keep().steps.len(), 1);
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
        assert_ne!(admitted.request, id);
    }

    #[test]
    fn the_handoff_comes_once_after_the_brief() {
        // keep refuses handoffs in v1; the placement is the launch's own,
        // ready for a keeper that has the disclosure check.
        let f = fixture();
        let brief = "Continue the move.";
        let handoff = "HANDOFF: release route #cc, the review promised to JP.";
        let launched = f
            .spawn
            .launch(
                Brief {
                    about: super::super::about(brief).to_owned(),
                    text: brief.to_owned(),
                    goal: None,
                },
                &f.directory.path().join("lane"),
                "medium",
                "agent-cc",
                Provider::Claude,
                LaunchExtras {
                    unit: None,
                    request: None,
                    handoff: Some(handoff.to_owned()),
                },
            )
            .unwrap();
        let prompt = std::fs::read_to_string(
            f.launcher.started.lock().unwrap()[0]
                .script
                .with_extension("prompt"),
        )
        .unwrap();
        let brief_at = prompt.rfind(brief).unwrap();
        let handoff_at = prompt.find(handoff).unwrap();
        assert!(prompt.find("STUB-WAKE").unwrap() < prompt.find("STUB-TAIL").unwrap());
        assert!(prompt.find("STUB-TAIL").unwrap() < brief_at);
        assert!(brief_at < prompt.find("# Handoff").unwrap());
        assert!(prompt.find("# Handoff").unwrap() < handoff_at);
        assert_eq!(prompt.matches(handoff).count(), 1);
        assert!(prompt.trim_end().ends_with(handoff));
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.prompt, [super::super::text_handle(&prompt)]);
        // A plain run has no handoff section.
        f.spawn.run(&f.request("No handoff.")).unwrap();
        let plain = std::fs::read_to_string(
            f.launcher.started.lock().unwrap()[1]
                .script
                .with_extension("prompt"),
        )
        .unwrap();
        assert!(!plain.contains("# Handoff"));
    }

    #[test]
    fn keep_refuses_a_request_that_carries_a_handoff() {
        let f = fixture();
        let asked = f.ask_with(
            "Continue the move.",
            None,
            Provider::Claude,
            Some("HANDOFF: private text."),
        );
        let reason = "a handoff needs the disclosure check, which this keeper does not have";
        assert_eq!(
            f.keep().steps,
            [(asked.request, KeepStep::Refused(reason.to_owned()))]
        );
        assert!(f.launcher.started.lock().unwrap().is_empty());
        let view = f.request_view(asked.request);
        assert_eq!(kinds(&view), ["refused"]);
        assert_eq!(view.outcomes[0].reason, [reason]);
        assert!(view.runs.is_empty());
        assert!(f.persona(&asked.name).is_none());
        assert_eq!(f.keep().steps, [(asked.request, KeepStep::Done)]);
        assert!(f.launcher.started.lock().unwrap().is_empty());
    }

    /// What the unit's settle line passes: its run and its request.
    fn settle_ids(script: &str) -> (Id, Option<Id>) {
        let line = script
            .lines()
            .find(|line| line.contains(" settle ") && line.contains(" --run "))
            .expect("a settle line");
        let after = |flag: &str| {
            line.split_whitespace()
                .skip_while(|word| *word != flag)
                .nth(1)
                .map(|hex| Id::from_hex(hex).unwrap())
        };
        (after("--run").expect("a run"), after("--request"))
    }

    /// A keeper that stops after systemd started the unit, before the run
    /// record is published: the unit runs, the pile has only `accepted`.
    fn keep_crashing_after_start(f: &Fixture) {
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.keep()));
        assert!(crashed.is_err(), "the fake keeper stops inside start");
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
    }

    #[test]
    fn settle_links_the_run_when_the_keeper_stopped_before_publishing_it() {
        let f = fixture_with(FakeLauncher {
            crash_after_start: true,
            ..Default::default()
        });
        let asked = f.ask("Crash window.", None, Provider::Claude);
        keep_crashing_after_start(&f);
        let view = f.request_view(asked.request);
        assert_eq!(kinds(&view), ["accepted"]);
        assert!(view.runs.is_empty());
        // The unit ends and is collected; its settle line still runs.
        *f.launcher.state.lock().unwrap() = "inactive".to_owned();
        let (run, request) = settle_ids(&f.script(0));
        assert_eq!(request, Some(asked.request));
        f.spawn
            .settle(&SettleRequest {
                name: asked.name.clone(),
                run,
                request,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: None,
                usage: None,
            })
            .unwrap();
        let view = f.request_view(asked.request);
        assert_eq!(view.runs, [(run, true)]);
        assert_eq!(kinds(&view), ["accepted", "task-terminal"]);
        // The spawner came from the request, so the zooid could message it.
        assert!(f.inbox().iter().any(|text| matches!(
            text,
            crate::message::MessageText::Text(text) if text.contains(&asked.name)
        )));
        assert!(f.persona(&asked.name).unwrap().retired);
        // Settling again adds nothing; the keeper launches nothing new.
        f.spawn
            .settle(&SettleRequest {
                name: asked.name.clone(),
                run,
                request,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: None,
                usage: None,
            })
            .unwrap();
        assert_eq!(f.request_view(asked.request).runs, [(run, true)]);
        assert_eq!(f.keep().steps, [(asked.request, KeepStep::Done)]);
        assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_accepted_request_with_no_run_and_no_unit_is_uncertain_and_never_restarted() {
        let f = fixture_with(FakeLauncher {
            crash_after_start: true,
            ..Default::default()
        });
        let asked = f.ask("Crash and no settle.", None, Provider::Claude);
        keep_crashing_after_start(&f);
        // Settlement failed and left nothing; the unit was collected.
        *f.launcher.state.lock().unwrap() = "inactive".to_owned();
        // Even with the persona retired by hand, nothing says replay is safe.
        Relations::with_storage(f.storage.clone())
            .retire(&asked.name)
            .unwrap();
        for _ in 0..2 {
            let pass = f.keep();
            assert!(
                matches!(&pass.steps[..], [(id, KeepStep::Uncertain(why))]
                    if *id == asked.request
                        && why.starts_with("accepted at ")
                        && why.contains("not restarting")),
                "{pass:?}"
            );
            assert_eq!(f.launcher.started.lock().unwrap().len(), 1);
        }
        let view = f.request_view(asked.request);
        assert_eq!(kinds(&view), ["accepted"]);
        assert!(view.runs.is_empty());
        // It stays visible, and says why it is not started.
        let listed = render_requests(&f.spawn.requests(None).unwrap());
        assert!(listed.contains("never starts it again"), "{listed}");
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
        // The stub tail quotes the brief, so the brief itself is its last occurrence.
        let brief_at = prompt.rfind(brief).unwrap();
        assert!(at("STUB-WAKE") < at("STUB-TAIL about Brief body"));
        assert!(at("STUB-TAIL") < at(&format!("You are {}", launched.name)));
        assert!(at("spawned by agent-cc") < brief_at);
        assert!(prompt.trim_end().ends_with(brief));
        assert!(prompt.contains("Do not arm a colony watcher"));
        assert!(launched.untinted.is_none());
        assert_eq!(launched.prompt_chars, prompt.chars().count());
        let recorded = f.spawn.show(&launched.name).unwrap().runs[0].prompt[0];
        assert_eq!(recorded, super::super::text_handle(&prompt));
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
                request: None,
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
                request: None,
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
                request: None,
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
    fn a_tint_that_fails_falls_back_to_the_untinted_tail_and_says_so() {
        let f = fixture_full(
            FakeLauncher::default(),
            StubRenderer { refuse_tint: true },
            "export TRIBLESPACE_PEERS=none\n",
        );
        let launched = f.spawn.run(&f.request("Tint refused.")).unwrap();
        let why = launched.untinted.clone().unwrap();
        assert!(why.contains("WeMM"), "{why}");
        let prompt = std::fs::read_to_string(
            f.launcher.started.lock().unwrap()[0]
                .script
                .with_extension("prompt"),
        )
        .unwrap();
        assert!(prompt.contains("STUB-TAIL untinted"));
        let run = &f.spawn.show(&launched.name).unwrap().runs[0];
        assert_eq!(run.untinted, [why]);
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
        assert!(script.contains("--output-format text < "));
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
                "--mcp-config {} --settings {} --output-format text",
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
        // The script answers them through `spawn inbox`, for this run, in the
        // CLI's environment and before the agent's own exports.
        let inbox = script
            .find(&format!(
                "if [ \"${{1:-}}\" = inbox ]; then exec spawn inbox {} --run {:x} --hook \"${{2:-}}\" 2>> {}; fi\n",
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
                " || { echo \"spawn: reading MISTRAL_API_KEY from Secrets failed\" >&2; \
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
                request: None,
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
                request: None,
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
        let compass = Compass::with_storage(f.storage.clone());
        let parent = compass
            .add("The larger aim", crate::compass::AddOptions::default())
            .unwrap();
        let parent_hex = format!("{:x}", parent.goal);
        let goal = compass
            .add(
                "Build the widget",
                crate::compass::AddOptions {
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
        assert!(prompt.contains("STUB-TAIL about Build the widget. Serves: The larger aim"));

        // The zooid noted its launch on the goal, as itself.
        let note = launched.goal_note.expect("a launch note");
        assert!(launched.warnings.is_empty(), "{:?}", launched.warnings);
        assert_eq!(view.runs[0].goal_note, [note]);
        let reading = compass.reading(&goal_hex).unwrap();
        let launch_note = reading.notes.iter().find(|n| n.id == note).unwrap();
        assert!(launch_note.text.starts_with(&format!(
            "spawned {} run {:x} from goal revision",
            launched.name, launched.run
        )));
        assert_eq!(launch_note.by, [launched.persona]);

        // Settling does not move the goal.
        f.settle(&launched.name, &launched).unwrap();
        let after = compass.reading(&goal_hex).unwrap();
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
                request: None,
                exit: Exit::from_systemd("exited", "0", "success"),
                report: None,
                usage: None,
            })
            .unwrap();
        assert!(spawn.show(&launched.name).unwrap().runs[0].message.len() == 1);
    }
}
