//! `zooid drive-task`: the local executor that launches one zooid into
//! Drive's living loop, `drive run --mind mini`.
//!
//! A local run's unit script exports the run's pinned collection routes and
//! then execs the host's executor template. This turns that run into exactly
//! one `drive run` command line and execs it: each pinned
//! `TRIBLESPACE_COLLECTION_*` route becomes an explicit `--native-collection
//! ROLE=DESCRIPTOR`, the archive route is also the `--native-archive`, the
//! zooid's name is its `--native-persona`, where its archived history ends
//! is its `--native-resume`, and its brief file (`{brief}`, without the
//! wake: Drive reads its own) and sender (`{spawner}`) are `--brief-file`
//! and `--brief-from`. Everything the run does not decide (tenant, home,
//! model, window, wake allowance, telemetry, provider) comes from one binding
//! file the host provisions. An omitted wake allowance leaves Drive's default
//! in effect. Nothing is truncated or falls back to a hosted model. Every
//! pinned faculties route is passed on;
//! a route variable that names no faculties collection, a malformed or
//! repeated route, or a missing archive route refuses before Drive starts,
//! naming the routes. Which roles Drive accepts is Drive's own table.
//! A playground backend-arg may name `{tenant}`, the binding's tenant, and
//! `{cgroup}`, the delegated cgroup of the zooid's own unit, which this
//! prepares for the shell sandbox before Drive starts ([`delegated_cgroup`]).
//!
//! How the zooid ends: `drive run` with a native mind is a living loop with
//! no turn budget or deadline. It ends when it is interrupted (SIGINT),
//! after its ordinary teardown (the open response closed, the archive
//! epoch published, the session ended, the piles closed), or when its mind
//! fails. Spawn starts a local unit with `KillSignal=SIGINT` and
//! `KillMode=mixed`, so stopping the unit, or its optional runtime limit,
//! is that interrupt; settlement reads an exit 0 after it as a normal end.
//! Drive writes no closing message: the zooid's words are in the archive.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::Deserialize;
use triblespace::prelude::Id;

/// The role whose route is also Drive's `--native-archive`, the collection
/// Drive writes the zooid's own history to.
pub const ARCHIVE_ROLE: &str = "archive";

/// Everything a host provisions for its local zooids. Only `memory-chars`
/// may be omitted and no other key is accepted.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Binding {
    /// The `drive` executable, built with the `mini-native` feature.
    pub drive: PathBuf,
    pub tenant: String,
    /// The durable home pile: the zooid's Memory, Archive and cognition, and
    /// the wake Drive reads.
    pub home: PathBuf,
    pub home_key: PathBuf,
    pub model: PathBuf,
    pub model_root: String,
    pub model_collection: String,
    pub context_window: u64,
    /// Override Drive's recalled-memory character allowance independently of
    /// the full context window. Zero selects no recalled memories.
    pub memory_chars: Option<usize>,
    /// The largest brief file passed on; a larger one refuses.
    pub max_brief_bytes: u64,
    pub telemetry: Telemetry,
    pub provider: DriveProvider,
}

/// `"off"`, or `{"pile": PATH}`: Drive's `--no-telemetry` or
/// `--telemetry-pile`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum Telemetry {
    Off,
    Pile(PathBuf),
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
    /// Read and check a binding file.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("read the Drive binding {}", path.display()))?;
        Self::parse(&bytes, path)
    }

    /// Parse and check binding bytes already read from `path`. The adapter
    /// hashes the same bytes it parses, so the digest it logs names the
    /// configuration it executes even if the file changes afterwards.
    pub fn parse(bytes: &[u8], path: &Path) -> Result<Self> {
        let binding: Self = serde_json::from_slice(bytes)
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
            ("context-window", self.context_window),
            ("max-brief-bytes", self.max_brief_bytes),
        ] {
            ensure!(value > 0, "binding key {key} must be positive");
        }
        if let Telemetry::Pile(pile) = &self.telemetry {
            ensure!(
                !pile.as_os_str().is_empty(),
                "binding telemetry pile must not be empty"
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
    /// `{brief}`: the brief file, Drive's `--brief-file`, passed untouched.
    pub brief: &'a Path,
    /// `{spawner}`: who sent the brief, Drive's `--brief-from`.
    pub spawner: &'a str,
    /// `{name}`: the zooid's persona.
    pub name: &'a str,
    /// `TRIBLESPACE_COLLECTION_*` variable and value, as the unit exported.
    pub routes: &'a [(String, String)],
    /// The run's pile and signing key: Drive's native faculties owner.
    pub pile: &'a Path,
    pub key: &'a Path,
    /// `{resume}`: the archived node the zooid's history ends at, Drive's
    /// `--native-resume`; `None` for a zooid with no history, which Drive
    /// starts fresh.
    pub resume: Option<Id>,
}

/// `{resume}` as spawn substituted it: 32 hex digits, or empty for a zooid
/// with no archived history. Anything else refuses.
pub fn parse_resume(word: &str) -> Result<Option<Id>> {
    if word.is_empty() {
        return Ok(None);
    }
    ensure!(
        word.len() == 32 && word.bytes().all(|b| b.is_ascii_hexdigit()),
        "--resume takes the 32-hex archived node spawn found, or nothing; got {word:?}"
    );
    Id::from_hex(word)
        .map(Some)
        .ok_or_else(|| anyhow!("--resume {word} is not an id"))
}

/// One `drive run` invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub program: PathBuf,
    /// Everything after the program, starting with `run`.
    pub args: Vec<OsString>,
}

/// The pinned routes as `role -> descriptor`, every one of them passed on.
/// Which roles Drive accepts is Drive's own table, and Drive refuses an
/// unknown role itself, naming it; a second copy of that table here would go
/// stale whenever Drive's faculties pin moves. A route whose variable names
/// no faculties collection, a repeated variable, an empty or malformed
/// descriptor, or a missing archive route refuses here, naming every
/// offending route at once.
pub fn route_roles(routes: &[(String, String)]) -> Result<BTreeMap<&'static str, String>> {
    let known: Vec<(String, &'static str)> = crate::collection_names::table()
        .into_iter()
        .map(|(scope, name)| (crate::collection_names::override_env_name(scope), name))
        .collect();
    let mut roles = BTreeMap::new();
    let mut unknown = Vec::new();
    let mut malformed = Vec::new();
    for (variable, value) in routes {
        let Some((_, role)) = known.iter().find(|(known, _)| known == variable) else {
            unknown.push(variable.clone());
            continue;
        };
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

/// `{tenant}` in a playground backend-arg: the binding's tenant, the
/// provisioned sandbox principal Drive's `--tenant` names.
pub const TENANT_PLACEHOLDER: &str = "{tenant}";
/// `{cgroup}` in a playground backend-arg: the delegated cgroup this unit
/// owns, found by [`delegated_cgroup`].
pub const CGROUP_PLACEHOLDER: &str = "{cgroup}";

/// Whether the binding's provider asks for the delegated cgroup.
pub fn wants_cgroup(binding: &Binding) -> bool {
    match &binding.provider {
        DriveProvider::Playground(playground) => playground
            .backend_args
            .iter()
            .any(|arg| arg.contains(CGROUP_PLACEHOLDER)),
        DriveProvider::Mcp(_) => false,
    }
}

/// The delegated cgroup for a local zooid's shell sandbox, prepared from
/// inside the unit. Spawn starts a local unit with `Delegate=cpu memory
/// pids` and `DelegateSubgroup=supervisor`, so this process lives in
/// `<unit>/supervisor` (`own` is `/proc/self/cgroup`) and the unit's cgroup
/// is the parent. The parent must hold no process, and cpu, memory and pids
/// are enabled for its children, which playground requires before it makes
/// its own groups there. `root` is the cgroup-v2 mount, `/sys/fs/cgroup`.
/// Anything else refuses, naming what it found.
pub fn delegated_cgroup(own: &str, root: &Path) -> Result<PathBuf> {
    let found = own
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| anyhow!("refusing to start Drive: no cgroup-v2 line in {own:?}"))?;
    let parent = found
        .strip_suffix("/supervisor")
        .filter(|parent| parent.starts_with('/') && parent.len() > 1)
        .ok_or_else(|| {
            anyhow!(
                "refusing to start Drive: {{cgroup}} needs a delegated unit, and this process is \
                 in {found:?}, not a unit's supervisor subgroup"
            )
        })?;
    let parent = root.join(parent.trim_start_matches('/'));
    let procs = std::fs::read_to_string(parent.join("cgroup.procs"))
        .with_context(|| format!("read {}/cgroup.procs", parent.display()))?;
    ensure!(
        procs.trim().is_empty(),
        "refusing to start Drive: the delegated cgroup {} holds processes ({}), so it cannot \
         enable controllers for its children",
        parent.display(),
        procs.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    let control = parent.join("cgroup.subtree_control");
    std::fs::write(&control, "+cpu +memory +pids\n").with_context(|| {
        format!(
            "refusing to start Drive: could not enable cpu, memory and pids in {}",
            control.display()
        )
    })?;
    let enabled =
        std::fs::read_to_string(&control).with_context(|| format!("read {}", control.display()))?;
    // The kernel reads back `cpu memory pids`; a plain file (the tests'
    // fake mount) echoes what was written.
    for controller in ["cpu", "memory", "pids"] {
        ensure!(
            enabled
                .split_whitespace()
                .any(|word| word.trim_start_matches('+') == controller),
            "refusing to start Drive: {} enables {:?} after the write, without {controller}",
            control.display(),
            enabled.trim()
        );
    }
    Ok(parent)
}

/// One backend-arg with `{tenant}` and `{cgroup}` substituted. A tenant
/// substituted must be one playground can provision.
fn substitute(arg: &str, tenant: &str, cgroup: Option<&Path>) -> Result<String> {
    if arg.contains(TENANT_PLACEHOLDER) {
        ensure!(
            !tenant.is_empty()
                && tenant.len() <= 64
                && tenant
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "refusing to start Drive: the tenant {tenant:?} is not a sandbox tenant \
             (1..64 ASCII letters, digits, '-' or '_')"
        );
    }
    let arg = arg.replace(TENANT_PLACEHOLDER, tenant);
    if !arg.contains(CGROUP_PLACEHOLDER) {
        return Ok(arg);
    }
    let cgroup = cgroup
        .ok_or_else(|| anyhow!("refusing to start Drive: {arg:?} needs the delegated cgroup"))?;
    let cgroup = cgroup
        .to_str()
        .ok_or_else(|| anyhow!("the delegated cgroup {} is not UTF-8", cgroup.display()))?;
    Ok(arg.replace(CGROUP_PLACEHOLDER, cgroup))
}

/// The `drive run` command line for one run, or why there is none.
/// `brief_len` is the brief file's size; a brief larger than the binding's
/// bound refuses rather than being cut. `cgroup` is the delegated cgroup a
/// `{cgroup}` backend-arg names, when one does.
pub fn plan(
    binding: &Binding,
    run: &RunInputs<'_>,
    brief_len: u64,
    cgroup: Option<&Path>,
) -> Result<Plan> {
    ensure!(
        brief_len <= binding.max_brief_bytes,
        "refusing to start Drive: the brief is {brief_len} bytes and max-brief-bytes is {}; \
         nothing is truncated",
        binding.max_brief_bytes
    );
    ensure!(brief_len > 0, "refusing to start Drive: the brief is empty");
    ensure!(!run.name.is_empty(), "the zooid name is empty");
    ensure!(!run.spawner.is_empty(), "the spawner is empty");
    let roles = route_roles(run.routes)?;

    let mut args: Vec<OsString> = vec!["run".into(), "--mind".into(), "mini".into()];
    let mut flag = |name: &str, value: OsString| {
        args.push(OsString::from(name));
        args.push(value);
    };
    flag("--native-persona", run.name.into());
    flag("--native-faculties-pile", run.pile.into());
    flag("--native-faculties-key", run.key.into());
    for (role, descriptor) in &roles {
        flag("--native-collection", format!("{role}={descriptor}").into());
    }
    flag("--native-archive", roles[ARCHIVE_ROLE].clone().into());
    if let Some(node) = run.resume {
        flag("--native-resume", format!("{node:x}").into());
    }
    flag("--brief-file", run.brief.into());
    flag("--brief-from", run.spawner.into());
    flag("--home", binding.home.clone().into());
    flag("--home-key", binding.home_key.clone().into());
    flag("--tenant", binding.tenant.clone().into());
    flag("--model", binding.model.clone().into());
    flag("--model-root", binding.model_root.clone().into());
    flag(
        "--model-collection",
        binding.model_collection.clone().into(),
    );
    flag(
        "--context-window",
        binding.context_window.to_string().into(),
    );
    if let Some(chars) = binding.memory_chars {
        flag("--memory-chars", chars.to_string().into());
    }
    match &binding.telemetry {
        Telemetry::Off => args.push("--no-telemetry".into()),
        Telemetry::Pile(pile) => args.extend(["--telemetry-pile".into(), pile.clone().into()]),
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
                let arg = substitute(arg, &binding.tenant, cgroup)?;
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
    Ok(Plan {
        program: binding.drive.clone(),
        args,
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
/// only on a refusal or a failed exec. Stdin passes through; Drive never
/// reads it.
pub fn exec(binding: &Path, run: &RunInputs<'_>) -> Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt as _;
    let bytes = std::fs::read(binding)
        .with_context(|| format!("read the Drive binding {}", binding.display()))?;
    let parsed = Binding::parse(&bytes, binding)?;
    let brief = std::fs::metadata(run.brief)
        .with_context(|| format!("the brief {}", run.brief.display()))?;
    ensure!(
        brief.is_file(),
        "the brief {} is not a regular file",
        run.brief.display()
    );
    let cgroup = if wants_cgroup(&parsed) {
        let own = std::fs::read_to_string("/proc/self/cgroup").context("read /proc/self/cgroup")?;
        Some(delegated_cgroup(&own, Path::new("/sys/fs/cgroup"))?)
    } else {
        None
    };
    let plan = plan(&parsed, run, brief.len(), cgroup.as_deref())?;
    eprintln!(
        "zooid drive-task: binding {} blake3:{}; brief {} bytes from {}; {}",
        binding.display(),
        blake3::hash(&bytes).to_hex(),
        brief.len(),
        run.spawner,
        match run.resume {
            Some(node) => format!("resuming at {node:x}"),
            None => "fresh".to_owned(),
        }
    );
    eprintln!(
        "zooid drive-task: exec {} {}",
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
        "context-window": 4096,
        "max-brief-bytes": 500,
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
            brief: Path::new("/state dir/z.brief"),
            spawner: "tutor",
            name: "zooid-name",
            routes,
            pile: Path::new("/piles/colony pile.pile"),
            key: Path::new("/piles/colony.key"),
            resume: None,
        }
    }

    fn strings(plan: &Plan) -> Vec<String> {
        plan.args
            .iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_parsed_binding_is_the_hashed_bytes_not_a_second_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("binding.json");
        let original = FAKE_BINDING.as_bytes().to_vec();
        std::fs::write(&path, &original).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        // The file changes after the adapter captured and hashed it.
        std::fs::write(&path, FAKE_BINDING.replace("fake-tenant", "another tenant")).unwrap();
        let parsed = Binding::parse(&bytes, &path).unwrap();
        assert_eq!(parsed.tenant, binding().tenant);
        assert_eq!(bytes, original);
    }

    #[test]
    fn the_whole_argv_from_routes_and_a_binding() {
        let pinned = routes(&["archive", "compass", "message", "memory-journal"]);
        let plan = plan(&binding(), &run(&pinned), 500, None).unwrap();
        assert_eq!(plan.program, PathBuf::from("/fake/bin/drive"));
        let collection = |role: &str| format!("{role}={DESCRIPTOR}");
        let expected: Vec<String> = [
            "run",
            "--mind",
            "mini",
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
            "--brief-file",
            "/state dir/z.brief",
            "--brief-from",
            "tutor",
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
            "--context-window",
            "4096",
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
        // A living loop: none of a finite task's flags.
        for task in [
            "--turns",
            "--deadline-ms",
            "--prompt-file",
            "--message-file",
        ] {
            assert!(!expected.iter().any(|arg| arg == task), "{task}");
        }
    }

    #[test]
    fn a_found_continuation_is_drives_native_resume_and_none_adds_nothing() {
        let pinned = routes(&["archive", "compass"]);
        let fresh = strings(&plan(&binding(), &run(&pinned), 500, None).unwrap());
        assert!(
            !fresh.iter().any(|arg| arg == "--native-resume"),
            "{fresh:?}"
        );

        let node = Id::from_hex("0123456789abcdef0123456789abcdef").unwrap();
        let mut resumed = run(&pinned);
        resumed.resume = Some(node);
        let args = strings(&plan(&binding(), &resumed, 500, None).unwrap());
        let at = args
            .iter()
            .position(|arg| arg == "--native-resume")
            .unwrap();
        assert_eq!(args[at + 1], "0123456789abcdef0123456789abcdef");
        assert_eq!(
            args.iter().filter(|arg| *arg == "--native-resume").count(),
            1
        );
        // Everything else is the fresh plan's, in order.
        let mut without = args.clone();
        without.drain(at..at + 2);
        assert_eq!(without, fresh);

        assert_eq!(parse_resume("").unwrap(), None);
        assert_eq!(
            parse_resume("0123456789abcdef0123456789abcdef").unwrap(),
            Some(node)
        );
        for bad in [
            "0123",
            "zz23456789abcdef0123456789abcdef",
            " 0123456789abcdef0123456789abcdef",
        ] {
            assert!(parse_resume(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn telemetry_to_a_pile_and_an_mcp_provider() {
        let mut binding = binding();
        binding.telemetry = Telemetry::Pile("/t p.pile".into());
        binding.provider = DriveProvider::Mcp(Mcp {
            url: "http://fake.invalid/mcp".into(),
            token_file: "/fake token".into(),
        });
        let pinned = routes(&["archive"]);
        let args = strings(&plan(&binding, &run(&pinned), 1, None).unwrap());
        let tail = &args[args.len() - 6..];
        assert_eq!(
            tail,
            [
                "--telemetry-pile",
                "/t p.pile",
                "--mcp-url",
                "http://fake.invalid/mcp",
                "--mcp-token-file",
                "/fake token"
            ]
        );
    }

    #[test]
    fn every_pinned_faculties_route_is_passed_on() {
        // Drive, not this adapter, decides which roles it accepts; a Drive
        // pinned to an older faculties refuses the newer ones itself.
        let pinned = routes(&["archive", "compass", "spawn", "alarm"]);
        let args = strings(&plan(&binding(), &run(&pinned), 1, None).unwrap());
        for role in ["archive", "compass", "spawn", "alarm"] {
            let collection = format!("{role}={DESCRIPTOR}");
            assert!(args.contains(&collection), "{role}: {args:?}");
        }
    }

    #[test]
    fn an_unknown_route_variable_refuses() {
        let mut pinned = routes(&["archive"]);
        pinned.push((
            "TRIBLESPACE_COLLECTION_NOT_A_FACULTY".to_owned(),
            DESCRIPTOR.to_owned(),
        ));
        let error = plan(&binding(), &run(&pinned), 1, None)
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
        let error = plan(&binding(), &run(&pinned), 1, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("TRIBLESPACE_COLLECTION_ARCHIVE"), "{error}");
        assert!(error.contains("--native-archive"), "{error}");
    }

    #[test]
    fn a_malformed_or_repeated_route_refuses() {
        let mut pinned = routes(&["archive", "compass"]);
        pinned[1].1 = "blake3:12".to_owned();
        assert!(plan(&binding(), &run(&pinned), 1, None).is_err());
        let mut twice = routes(&["archive", "compass"]);
        twice.push(twice[1].clone());
        let error = plan(&binding(), &run(&twice), 1, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("given twice"), "{error}");
    }

    #[test]
    fn a_brief_over_the_bound_or_empty_refuses_and_one_at_it_passes() {
        let pinned = routes(&["archive"]);
        assert!(plan(&binding(), &run(&pinned), 500, None).is_ok());
        let error = plan(&binding(), &run(&pinned), 501, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("nothing is truncated"), "{error}");
        assert!(plan(&binding(), &run(&pinned), 0, None).is_err());
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
            .insert("turns".to_owned(), 5.into());
        assert!(serde_json::from_value::<Binding>(extra).is_err());
        let mut no_args = full.clone();
        no_args["provider"]["playground"]
            .as_object_mut()
            .unwrap()
            .remove("backend-args");
        assert!(serde_json::from_value::<Binding>(no_args).is_err());
        let mut zero = full;
        zero["max-brief-bytes"] = 0.into();
        let zero: Binding = serde_json::from_value(zero).unwrap();
        assert!(zero.check().is_err());
    }

    fn playground(binding: &mut Binding, args: &[&str]) {
        let DriveProvider::Playground(playground) = &mut binding.provider else {
            unreachable!()
        };
        playground.backend_args = args.iter().map(|arg| arg.to_string()).collect();
    }

    #[test]
    fn tenant_and_cgroup_placeholders_are_substituted_in_backend_args() {
        let mut binding = binding();
        binding.tenant = "zooid-a".to_owned();
        playground(
            &mut binding,
            &[
                "--bubblewrap-tenant",
                "{tenant}",
                "--bubblewrap-cgroup-parent={cgroup}",
                "{name}",
            ],
        );
        assert!(wants_cgroup(&binding));
        let pinned = routes(&["archive"]);
        let cgroup = Path::new("/sys/fs/cgroup/user.slice/z.service");
        let args = strings(&plan(&binding, &run(&pinned), 1, Some(cgroup)).unwrap());
        let backend: Vec<&str> = args
            .windows(2)
            .filter(|pair| pair[0] == "--backend-arg")
            .map(|pair| pair[1].as_str())
            .collect();
        // No other placeholder: `{name}` passes as written.
        assert_eq!(
            backend,
            [
                "--bubblewrap-tenant",
                "zooid-a",
                "--bubblewrap-cgroup-parent=/sys/fs/cgroup/user.slice/z.service",
                "{name}"
            ]
        );
        // Without the cgroup a {cgroup} arg refuses; without one, nothing is asked.
        assert!(plan(&binding, &run(&pinned), 1, None).is_err());
        assert!(!wants_cgroup(&self::binding()));
        // A tenant playground could not provision refuses where it is named.
        binding.tenant = "two words".to_owned();
        let error = plan(&binding, &run(&pinned), 1, Some(cgroup))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a sandbox tenant"), "{error}");
        playground(&mut binding, &["--plain"]);
        assert!(plan(&binding, &run(&pinned), 1, None).is_ok());
    }

    /// A fake cgroup mount: the unit's cgroup with its two control files.
    fn cgroup_root(procs: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let unit = root.path().join("user.slice/z.service");
        std::fs::create_dir_all(unit.join("supervisor")).unwrap();
        std::fs::write(unit.join("cgroup.procs"), procs).unwrap();
        std::fs::write(unit.join("cgroup.subtree_control"), "").unwrap();
        (root, unit)
    }

    #[test]
    fn the_delegated_cgroup_is_the_supervisors_parent_with_controllers_enabled() {
        let (root, unit) = cgroup_root("");
        let own = "0::/user.slice/z.service/supervisor\n";
        assert_eq!(delegated_cgroup(own, root.path()).unwrap(), unit);
        assert_eq!(
            std::fs::read_to_string(unit.join("cgroup.subtree_control")).unwrap(),
            "+cpu +memory +pids\n"
        );
    }

    #[test]
    fn the_delegated_cgroup_refuses_outside_a_supervisor_subgroup_or_with_processes() {
        let (root, unit) = cgroup_root("");
        for own in [
            "0::/user.slice/z.service\n",
            "0::/supervisor\n",
            "1:name=x:/a\n",
        ] {
            let error = delegated_cgroup(own, root.path()).unwrap_err().to_string();
            assert!(
                error.contains("refusing to start Drive"),
                "{own:?}: {error}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(unit.join("cgroup.subtree_control")).unwrap(),
            ""
        );
        let (root, unit) = cgroup_root("4242\n");
        let error = delegated_cgroup("0::/user.slice/z.service/supervisor", root.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("holds processes (4242)"), "{error}");
        assert_eq!(
            std::fs::read_to_string(unit.join("cgroup.subtree_control")).unwrap(),
            ""
        );
        // A failed write refuses: here the control file is a directory.
        let (root, unit) = cgroup_root("");
        std::fs::remove_file(unit.join("cgroup.subtree_control")).unwrap();
        std::fs::create_dir(unit.join("cgroup.subtree_control")).unwrap();
        let error = delegated_cgroup("0::/user.slice/z.service/supervisor", root.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("could not enable cpu, memory and pids"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_binding_file_refuses() {
        let error = Binding::read(Path::new("/nonexistent/fake binding.json"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("read the Drive binding"), "{error}");
    }
}
