//! `spawn drive-task`: the local executor that runs one finite Drive task.
//!
//! A local run's unit script exports the run's pinned collection routes and
//! then execs the host's executor template. This turns that run into exactly
//! one `drive task` command line and execs it: each pinned
//! `TRIBLESPACE_COLLECTION_*` route becomes an explicit `--native-collection
//! ROLE=DESCRIPTOR`, the archive route is also the `--native-archive`, and
//! everything the run does not decide (tenant, session home, model, sizes,
//! telemetry, provider) comes from one binding file the host provisions. No
//! value has a default, nothing is truncated, no text is added to the
//! prompt, and nothing falls back to a hosted model. Any route Drive cannot
//! take refuses the launch before Drive starts, naming the routes.
//!
//! The deadline Drive gets ends a stated teardown margin before the unit's
//! `RuntimeMaxSec`, so Drive's own cooperative deadline fires first and the
//! runtime limit stays a backstop.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::Deserialize;

/// The collection roles the frozen `drive task` source accepts in
/// `--native-collection ROLE=DESCRIPTOR`: the names of
/// `faculties::collection_names::table()` at the faculties revision that
/// Drive build pins, in its order. A pinned route whose role is not here
/// refuses the launch; it is never dropped.
///
/// Source: Drive `src/main.rs` `Options::native_faculties` resolves each role
/// with `collection_names::table()`; the pinned faculties is commit
/// 0fd9a9b870ca55c11c947bb9228ee4c1d3b9cce7, `src/collection_names.rs`.
pub const DRIVE_TASK_ROLES: [&str; 32] = [
    "atlas",
    "archive",
    "body",
    "code",
    "cognition",
    "compass",
    "config",
    "decide",
    "discord",
    "embeddings",
    "files",
    "habit",
    "headspace",
    "mail",
    "memory-journal",
    "memory-comb",
    "message",
    "models",
    "orient",
    "orient-receipts",
    "planner",
    "posture-policy",
    "posture-scan",
    "quota",
    "relations",
    "secrets",
    "status",
    "swarm-health",
    "teams",
    "voice",
    "web",
    "wiki",
];

/// The role whose route is also Drive's `--native-archive`, the collection
/// Drive writes the task's own history to.
pub const ARCHIVE_ROLE: &str = "archive";

/// Everything a host provisions for its local Drive tasks. Every key is
/// required and no other key is accepted.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Binding {
    /// The `drive` executable that has the `task` command.
    pub drive: PathBuf,
    pub tenant: String,
    pub home: PathBuf,
    pub home_key: PathBuf,
    pub model: PathBuf,
    pub model_root: String,
    pub model_collection: String,
    pub resident_bytes: u64,
    pub chunk_bytes: u64,
    pub context_window: u64,
    pub turns: u64,
    pub max_prompt_bytes: u64,
    pub max_report_bytes: u64,
    /// Seconds between Drive's deadline and the unit's runtime limit.
    pub teardown_margin_secs: u64,
    pub telemetry: Telemetry,
    pub provider: DriveProvider,
}

/// `"off"`, or `{"pile": {"pile": PATH, "key": PATH}}`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum Telemetry {
    Off,
    Pile(TelemetryPile),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct TelemetryPile {
    pub pile: PathBuf,
    pub key: PathBuf,
}

/// `{"playground": {...}}` or `{"mcp": {...}}`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum DriveProvider {
    Playground(Playground),
    Mcp(Mcp),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Playground {
    pub path: PathBuf,
    pub backend: String,
    /// Required, possibly empty: each becomes one `--backend-arg`.
    pub backend_args: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Mcp {
    pub url: String,
    pub token_file: PathBuf,
}

impl Binding {
    /// Read and check a binding file. A missing file, a missing or extra
    /// key, an empty value or a zero size refuses.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read the Drive binding {}", path.display()))?;
        let binding: Self = serde_json::from_str(&text)
            .with_context(|| format!("the Drive binding {} is not valid", path.display()))?;
        binding.check()?;
        Ok(binding)
    }

    fn check(&self) -> Result<()> {
        let paths = [
            ("drive", &self.drive),
            ("home", &self.home),
            ("home-key", &self.home_key),
            ("model", &self.model),
        ];
        for (key, path) in paths {
            ensure!(!path.as_os_str().is_empty(), "binding key {key} is empty");
        }
        for (key, text) in [
            ("tenant", &self.tenant),
            ("model-root", &self.model_root),
            ("model-collection", &self.model_collection),
        ] {
            ensure!(!text.is_empty(), "binding key {key} is empty");
        }
        for (key, value) in [
            ("resident-bytes", self.resident_bytes),
            ("chunk-bytes", self.chunk_bytes),
            ("context-window", self.context_window),
            ("turns", self.turns),
            ("max-prompt-bytes", self.max_prompt_bytes),
            ("max-report-bytes", self.max_report_bytes),
            ("teardown-margin-secs", self.teardown_margin_secs),
        ] {
            ensure!(value > 0, "binding key {key} must be positive");
        }
        if let Telemetry::Pile(pile) = &self.telemetry {
            ensure!(
                !pile.pile.as_os_str().is_empty() && !pile.key.as_os_str().is_empty(),
                "binding telemetry pile and key must not be empty"
            );
        }
        match &self.provider {
            DriveProvider::Playground(playground) => {
                ensure!(
                    !playground.path.as_os_str().is_empty() && !playground.backend.is_empty(),
                    "binding playground path and backend must not be empty"
                );
                ensure!(
                    playground.backend_args.iter().all(|arg| !arg.is_empty()),
                    "binding playground backend-args must not hold an empty argument"
                );
            }
            DriveProvider::Mcp(mcp) => ensure!(
                !mcp.url.is_empty() && !mcp.token_file.as_os_str().is_empty(),
                "binding mcp url and token-file must not be empty"
            ),
        }
        Ok(())
    }
}

/// What one run supplies: spawn's placeholders, the run's pinned routes and
/// the pile and key the run is recorded in.
#[derive(Clone, Debug)]
pub struct RunInputs<'a> {
    /// `{prompt}`: the prompt file, passed to Drive untouched.
    pub prompt: &'a Path,
    /// `{report}`: the report file Drive publishes, passed untouched.
    pub report: &'a Path,
    /// `{name}`: the zooid's persona.
    pub name: &'a str,
    /// `{runtime}`: the unit's `RuntimeMaxSec`.
    pub runtime_secs: u64,
    /// `TRIBLESPACE_COLLECTION_*` variable and value, as the unit exported.
    pub routes: &'a [(String, String)],
    /// The run's pile and signing key: Drive's native faculties owner.
    pub pile: &'a Path,
    pub key: &'a Path,
}

/// One `drive task` invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub program: PathBuf,
    /// Everything after the program, starting with `task`.
    pub args: Vec<OsString>,
    pub deadline_ms: u64,
}

/// Drive's deadline: the runtime limit less the teardown margin. A missing
/// or too short limit refuses.
pub fn deadline_ms(runtime_secs: u64, margin_secs: u64) -> Result<u64> {
    ensure!(margin_secs > 0, "the teardown margin must be positive");
    ensure!(
        runtime_secs > margin_secs,
        "the runtime limit ({runtime_secs} s) leaves no time before the teardown margin \
         ({margin_secs} s); raise SPAWN_LOCAL_MAX_RUNTIME or lower teardown-margin-secs"
    );
    (runtime_secs - margin_secs)
        .checked_mul(1000)
        .ok_or_else(|| anyhow!("the Drive deadline overflows"))
}

/// The pinned routes as `role -> descriptor` for the roles `drive_roles`
/// accepts. A route whose variable names no faculties collection, a route
/// whose role Drive does not accept, a repeated variable, an empty or
/// malformed descriptor, or a missing archive route refuses, naming every
/// offending route at once.
pub fn route_roles(
    routes: &[(String, String)],
    drive_roles: &[&str],
) -> Result<BTreeMap<&'static str, String>> {
    let known: Vec<(String, &'static str)> = crate::collection_names::table()
        .into_iter()
        .map(|(scope, name)| (crate::collection_names::override_env_name(scope), name))
        .collect();
    let mut roles = BTreeMap::new();
    let mut unknown = Vec::new();
    let mut not_drive = Vec::new();
    let mut malformed = Vec::new();
    for (variable, value) in routes {
        let Some((_, role)) = known.iter().find(|(known, _)| known == variable) else {
            unknown.push(variable.clone());
            continue;
        };
        if !drive_roles.contains(role) {
            not_drive.push(format!("{variable} ({role})"));
            continue;
        }
        if !is_descriptor(value) {
            malformed.push(variable.clone());
            continue;
        }
        ensure!(
            roles.insert(*role, value.clone()).is_none(),
            "route {variable} is given twice"
        );
    }
    let mut problems = Vec::new();
    if !unknown.is_empty() {
        problems.push(format!(
            "routes that name no faculties collection: {}",
            unknown.join(", ")
        ));
    }
    if !not_drive.is_empty() {
        problems.push(format!(
            "pinned routes whose role this Drive task does not accept: {}",
            not_drive.join(", ")
        ));
    }
    if !malformed.is_empty() {
        problems.push(format!(
            "routes whose value is not blake3: and 64 hex digits: {}",
            malformed.join(", ")
        ));
    }
    if !problems.is_empty() {
        bail!(
            "refusing to start Drive, nothing was dropped: {}",
            problems.join("; ")
        );
    }
    ensure!(
        roles.contains_key(ARCHIVE_ROLE),
        "refusing to start Drive: the run pins no {} route, which is Drive's --native-archive",
        crate::collection_names::override_env_name(
            crate::collection_names::table()
                .into_iter()
                .find(|(_, name)| *name == ARCHIVE_ROLE)
                .map(|(scope, _)| scope)
                .expect("archive is a faculties collection")
        )
    );
    Ok(roles)
}

fn is_descriptor(value: &str) -> bool {
    value
        .strip_prefix("blake3:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The `drive task` command line for one run, or why there is none.
/// `prompt_len` is the prompt file's size; a prompt larger than the binding's
/// bound refuses rather than being cut.
pub fn plan(
    binding: &Binding,
    run: &RunInputs<'_>,
    prompt_len: u64,
    drive_roles: &[&str],
) -> Result<Plan> {
    ensure!(
        prompt_len <= binding.max_prompt_bytes,
        "refusing to start Drive: the prompt is {prompt_len} bytes and max-prompt-bytes is {}; \
         nothing is truncated",
        binding.max_prompt_bytes
    );
    ensure!(!run.name.is_empty(), "the zooid name is empty");
    ensure!(
        run.prompt != run.report,
        "prompt and report are the same file"
    );
    let deadline_ms = deadline_ms(run.runtime_secs, binding.teardown_margin_secs)?;
    let roles = route_roles(run.routes, drive_roles)?;

    let mut args: Vec<OsString> = Vec::new();
    let mut flag = |name: &str, value: OsString| {
        args.push(OsString::from(name));
        args.push(value);
    };
    let n = |value: u64| OsString::from(value.to_string());
    flag("--prompt-file", run.prompt.into());
    flag("--max-prompt-bytes", n(binding.max_prompt_bytes));
    flag("--report-file", run.report.into());
    flag("--native-persona", run.name.into());
    flag("--native-faculties-pile", run.pile.into());
    flag("--native-faculties-key", run.key.into());
    for (role, descriptor) in &roles {
        flag("--native-collection", format!("{role}={descriptor}").into());
    }
    flag("--native-archive", roles[ARCHIVE_ROLE].clone().into());
    flag("--home", binding.home.clone().into());
    flag("--home-key", binding.home_key.clone().into());
    flag("--tenant", binding.tenant.clone().into());
    flag("--model", binding.model.clone().into());
    flag("--model-root", binding.model_root.clone().into());
    flag(
        "--model-collection",
        binding.model_collection.clone().into(),
    );
    flag("--resident-bytes", n(binding.resident_bytes));
    flag("--chunk-bytes", n(binding.chunk_bytes));
    flag("--context-window", n(binding.context_window));
    flag("--turns", n(binding.turns));
    flag("--deadline-ms", n(deadline_ms));
    flag("--max-report-bytes", n(binding.max_report_bytes));
    match &binding.telemetry {
        Telemetry::Off => args.push("--no-telemetry".into()),
        Telemetry::Pile(pile) => {
            args.extend([
                "--telemetry-pile".into(),
                pile.pile.clone().into(),
                "--telemetry-key".into(),
                pile.key.clone().into(),
            ]);
        }
    }
    match &binding.provider {
        DriveProvider::Playground(playground) => {
            args.extend([
                "--playground".into(),
                playground.path.clone().into(),
                "--backend".into(),
                playground.backend.clone().into(),
            ]);
            for arg in &playground.backend_args {
                args.extend(["--backend-arg".into(), arg.into()]);
            }
        }
        DriveProvider::Mcp(mcp) => args.extend([
            "--mcp-url".into(),
            mcp.url.clone().into(),
            "--mcp-token-file".into(),
            mcp.token_file.clone().into(),
        ]),
    }
    args.insert(0, "task".into());
    Ok(Plan {
        program: binding.drive.clone(),
        args,
        deadline_ms,
    })
}

/// The run's routes from this process's environment: every
/// `TRIBLESPACE_COLLECTION_*` variable, sorted. A non-UTF-8 one refuses.
pub fn routes_from_env() -> Result<Vec<(String, String)>> {
    let prefix = crate::collection_names::COLLECTION_OVERRIDE_PREFIX;
    let mut routes = Vec::new();
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }
        let value = value
            .into_string()
            .map_err(|_| anyhow!("route {name} is not UTF-8"))?;
        routes.push((name, value));
    }
    routes.sort();
    Ok(routes)
}

/// Plan the run from the binding file, the placeholders and the
/// environment, say on stderr what Drive is given, and exec Drive. Returns
/// only on a refusal or a failed exec. Stdin (the prompt, as spawn
/// redirects it) passes through; Drive never reads it.
pub fn exec(binding: &Path, run: &RunInputs<'_>) -> Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt as _;
    let bytes = std::fs::read(binding)
        .with_context(|| format!("read the Drive binding {}", binding.display()))?;
    let parsed = Binding::read(binding)?;
    let prompt = std::fs::metadata(run.prompt)
        .with_context(|| format!("the prompt {}", run.prompt.display()))?;
    ensure!(
        prompt.is_file(),
        "the prompt {} is not a regular file",
        run.prompt.display()
    );
    ensure!(
        !run.report.try_exists()?,
        "the report {} already exists; Drive publishes a new file only",
        run.report.display()
    );
    let plan = plan(&parsed, run, prompt.len(), &DRIVE_TASK_ROLES)?;
    eprintln!(
        "spawn drive-task: binding {} blake3:{}; deadline-ms {} (runtime {} s less {} s \
         teardown); prompt {} bytes",
        binding.display(),
        blake3::hash(&bytes).to_hex(),
        plan.deadline_ms,
        run.runtime_secs,
        parsed.teardown_margin_secs,
        prompt.len()
    );
    eprintln!(
        "spawn drive-task: exec {} {}",
        plan.program.display(),
        plan.args
            .iter()
            .map(|arg| super::shell_word(&arg.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let error = std::process::Command::new(&plan.program)
        .args(&plan.args)
        .exec();
    Err(error).with_context(|| format!("exec {}", plan.program.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIPTOR: &str =
        "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn binding() -> Binding {
        serde_json::from_str(FAKE_BINDING).unwrap()
    }

    /// The fake example under `acceptance/` has this shape.
    const FAKE_BINDING: &str = r#"{
        "drive": "/fake/bin/drive",
        "tenant": "fake-tenant",
        "home": "/fake/drive home/session.pile",
        "home-key": "/fake/drive home/session.key",
        "model": "/fake/models/model.pile",
        "model-root": "FAKEROOT",
        "model-collection": "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "resident-bytes": 1000,
        "chunk-bytes": 100,
        "context-window": 4096,
        "turns": 8,
        "max-prompt-bytes": 500,
        "max-report-bytes": 300,
        "teardown-margin-secs": 60,
        "telemetry": "off",
        "provider": {"playground": {"path": "/fake/playground", "backend": "fake-backend",
                     "backend-args": ["--one", "two words"]}}
    }"#;

    fn routes(roles: &[&str]) -> Vec<(String, String)> {
        roles
            .iter()
            .map(|role| {
                let scope = crate::collection_names::table()
                    .into_iter()
                    .find(|(_, name)| name == role)
                    .unwrap()
                    .0;
                (
                    crate::collection_names::override_env_name(scope),
                    DESCRIPTOR.to_owned(),
                )
            })
            .collect()
    }

    fn run<'a>(routes: &'a [(String, String)]) -> RunInputs<'a> {
        RunInputs {
            prompt: Path::new("/state dir/z.prompt"),
            report: Path::new("/state dir/z.report"),
            name: "zooid-name",
            runtime_secs: 3600,
            routes,
            pile: Path::new("/piles/colony pile.pile"),
            key: Path::new("/piles/colony.key"),
        }
    }

    fn strings(plan: &Plan) -> Vec<String> {
        plan.args
            .iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn every_drive_role_is_a_faculties_collection_and_its_variable_is_the_route() {
        let names: Vec<&str> = crate::collection_names::table()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        for role in DRIVE_TASK_ROLES {
            assert!(names.contains(&role), "{role}");
        }
        // The five collections faculties gained after Drive's pin.
        let newer: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| !DRIVE_TASK_ROLES.contains(name))
            .collect();
        assert_eq!(newer, ["alarm", "home", "imagine", "rules", "spawn"]);
    }

    #[test]
    fn the_whole_argv_from_routes_and_a_binding() {
        let pinned = routes(&["archive", "compass", "message", "memory-journal"]);
        let plan = plan(&binding(), &run(&pinned), 500, &DRIVE_TASK_ROLES).unwrap();
        assert_eq!(plan.program, PathBuf::from("/fake/bin/drive"));
        assert_eq!(plan.deadline_ms, 3_540_000);
        let collection = |role: &str| format!("{role}={DESCRIPTOR}");
        let expected: Vec<String> = [
            "task",
            "--prompt-file",
            "/state dir/z.prompt",
            "--max-prompt-bytes",
            "500",
            "--report-file",
            "/state dir/z.report",
            "--native-persona",
            "zooid-name",
            "--native-faculties-pile",
            "/piles/colony pile.pile",
            "--native-faculties-key",
            "/piles/colony.key",
            "--native-collection",
            &collection("archive"),
            "--native-collection",
            &collection("compass"),
            "--native-collection",
            &collection("memory-journal"),
            "--native-collection",
            &collection("message"),
            "--native-archive",
            DESCRIPTOR,
            "--home",
            "/fake/drive home/session.pile",
            "--home-key",
            "/fake/drive home/session.key",
            "--tenant",
            "fake-tenant",
            "--model",
            "/fake/models/model.pile",
            "--model-root",
            "FAKEROOT",
            "--model-collection",
            "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--resident-bytes",
            "1000",
            "--chunk-bytes",
            "100",
            "--context-window",
            "4096",
            "--turns",
            "8",
            "--deadline-ms",
            "3540000",
            "--max-report-bytes",
            "300",
            "--no-telemetry",
            "--playground",
            "/fake/playground",
            "--backend",
            "fake-backend",
            "--backend-arg",
            "--one",
            "--backend-arg",
            "two words",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        assert_eq!(strings(&plan), expected);
    }

    #[test]
    fn telemetry_to_a_pile_and_an_mcp_provider() {
        let mut binding = binding();
        binding.telemetry = Telemetry::Pile(TelemetryPile {
            pile: "/t p.pile".into(),
            key: "/t.key".into(),
        });
        binding.provider = DriveProvider::Mcp(Mcp {
            url: "http://fake.invalid/mcp".into(),
            token_file: "/fake token".into(),
        });
        let pinned = routes(&["archive"]);
        let args = strings(&plan(&binding, &run(&pinned), 1, &DRIVE_TASK_ROLES).unwrap());
        let tail = &args[args.len() - 8..];
        assert_eq!(
            tail,
            [
                "--telemetry-pile",
                "/t p.pile",
                "--telemetry-key",
                "/t.key",
                "--mcp-url",
                "http://fake.invalid/mcp",
                "--mcp-token-file",
                "/fake token"
            ]
        );
    }

    #[test]
    fn a_route_drive_does_not_take_refuses_and_names_them_all() {
        let pinned = routes(&["archive", "compass", "spawn", "alarm"]);
        let error = plan(&binding(), &run(&pinned), 1, &DRIVE_TASK_ROLES)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("TRIBLESPACE_COLLECTION_SPAWN (spawn)"),
            "{error}"
        );
        assert!(
            error.contains("TRIBLESPACE_COLLECTION_ALARM (alarm)"),
            "{error}"
        );
        assert!(error.contains("nothing was dropped"), "{error}");
    }

    #[test]
    fn an_unknown_route_variable_refuses() {
        let mut pinned = routes(&["archive"]);
        pinned.push((
            "TRIBLESPACE_COLLECTION_NOT_A_FACULTY".to_owned(),
            DESCRIPTOR.to_owned(),
        ));
        let error = plan(&binding(), &run(&pinned), 1, &DRIVE_TASK_ROLES)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("TRIBLESPACE_COLLECTION_NOT_A_FACULTY"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_archive_route_refuses() {
        let pinned = routes(&["compass"]);
        let error = plan(&binding(), &run(&pinned), 1, &DRIVE_TASK_ROLES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("TRIBLESPACE_COLLECTION_ARCHIVE"), "{error}");
        assert!(error.contains("--native-archive"), "{error}");
    }

    #[test]
    fn a_malformed_or_repeated_route_refuses() {
        let mut pinned = routes(&["archive", "compass"]);
        pinned[1].1 = "blake3:12".to_owned();
        assert!(plan(&binding(), &run(&pinned), 1, &DRIVE_TASK_ROLES).is_err());
        let mut twice = routes(&["archive", "compass"]);
        twice.push(twice[1].clone());
        let error = plan(&binding(), &run(&twice), 1, &DRIVE_TASK_ROLES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("given twice"), "{error}");
    }

    #[test]
    fn the_deadline_ends_the_margin_before_the_runtime_limit() {
        assert_eq!(deadline_ms(3600, 60).unwrap(), 3_540_000);
        assert_eq!(deadline_ms(61, 60).unwrap(), 1000);
        assert!(deadline_ms(60, 60).is_err());
        assert!(deadline_ms(10, 60).is_err());
        assert!(deadline_ms(100, 0).is_err());
        let pinned = routes(&["archive"]);
        let mut short = run(&pinned);
        short.runtime_secs = 60;
        let error = plan(&binding(), &short, 1, &DRIVE_TASK_ROLES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("SPAWN_LOCAL_MAX_RUNTIME"), "{error}");
    }

    #[test]
    fn a_prompt_over_the_bound_refuses_and_one_at_it_passes() {
        let pinned = routes(&["archive"]);
        assert!(plan(&binding(), &run(&pinned), 500, &DRIVE_TASK_ROLES).is_ok());
        let error = plan(&binding(), &run(&pinned), 501, &DRIVE_TASK_ROLES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("nothing is truncated"), "{error}");
    }

    #[test]
    fn every_binding_key_is_required_and_no_other_is_accepted() {
        let full: serde_json::Value = serde_json::from_str(FAKE_BINDING).unwrap();
        for key in full.as_object().unwrap().keys() {
            let mut partial = full.clone();
            partial.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<Binding>(partial).is_err(),
                "a binding without {key} was accepted"
            );
        }
        let mut extra = full.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("exec-timeout".to_owned(), 5.into());
        assert!(serde_json::from_value::<Binding>(extra).is_err());
        let mut no_args = full.clone();
        no_args["provider"]["playground"]
            .as_object_mut()
            .unwrap()
            .remove("backend-args");
        assert!(serde_json::from_value::<Binding>(no_args).is_err());
        let mut zero = full;
        zero["turns"] = 0.into();
        let zero: Binding = serde_json::from_value(zero).unwrap();
        assert!(zero.check().is_err());
    }

    #[test]
    fn a_missing_binding_file_refuses() {
        let error = Binding::read(Path::new("/nonexistent/fake binding.json"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("read the Drive binding"), "{error}");
    }
}
