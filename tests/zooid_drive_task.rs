//! `zooid drive-task`, the local executor that launches a zooid into
//! `drive run --mind mini`, run as the real `zooid` binary against a fake
//! `drive` script. Nothing here starts Drive, a model or a provider.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use faculties::orient::WakeTint;
use faculties::relations::{ProfileInput, Relations};
use faculties::spawn::operations::{describe_end, ended_clean};
use faculties::spawn::{
    Exit, LaunchConfig, Launcher, Provider, Renderer, RunRequest, SettleRequest, Spawn, UnitSpec,
};
use faculties::storage::{initialize_signer, Storage};

const SPAWN: &str = env!("CARGO_BIN_EXE_zooid");
const DESCRIPTOR: &str = "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MODEL_DESCRIPTOR: &str =
    "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A fake `drive`: leaves its arguments one per line and a copy of the
/// brief file it was named in `$FAKE_OUT`, then does what `$FAKE_MODE`
/// says. `live` is a living loop that tears down on SIGINT, as Drive does.
const FAKE_DRIVE: &str = r#"#!/bin/sh
out="$FAKE_OUT"
: > "$out/args"
for word in "$@"; do printf '%s\n' "$word" >> "$out/args"; done
brief=""
while [ $# -gt 0 ]; do
  case "$1" in
    --brief-file) brief="$2"; shift ;;
  esac
  shift
done
cp "$brief" "$out/brief-seen"
case "$FAKE_MODE" in
  fail) exit 3 ;;
  silent) exit 0 ;;
  live) trap 'echo torn-down > "$out/teardown"; exit 0' INT; sleep 60 & wait ;;
  cgroup)
    own=$(sed -n 's/^0:://p' /proc/self/cgroup)
    echo "$own" > "$out/own"
    cat "/sys/fs/cgroup${own%/supervisor}/cgroup.subtree_control" > "$out/subtree_control"
    exit 0 ;;
esac
"#;

/// A directory whose path has a space in it, with a fake drive, a binding,
/// a brief and an output directory.
struct Setup {
    _directory: tempfile::TempDir,
    root: PathBuf,
    drive: PathBuf,
    binding: PathBuf,
    brief: PathBuf,
    out: PathBuf,
    pile: PathBuf,
    key: PathBuf,
}

/// Bytes a brief carries through untouched: multi-byte characters, a tab,
/// a carriage return, trailing spaces and no final newline.
const BRIEF: &[u8] = "# Who you are\n\nGrüße, ✓ tab\there\r\nlast line   ".as_bytes();

fn binding_json(drive: &Path, home: &Path, max_brief: usize) -> String {
    serde_json::json!({
        "drive": drive,
        "tenant": "fake-tenant",
        "home": home.join("session.pile"),
        "home-key": home.join("session.key"),
        "model": "/fake/models/model.pile",
        "model-root": "FAKEROOT",
        "model-collection": MODEL_DESCRIPTOR,
        "context-window": 4096,
        "max-brief-bytes": max_brief,
        "telemetry": "off",
        "provider": {"playground": {"path": "/fake/playground", "backend": "fake-backend",
                     "backend-args": ["two words"]}}
    })
    .to_string()
}

fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn setup() -> Setup {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("a dir");
    for dir in ["bin", "drive home", "state dir", "out", "piles"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let drive = root.join("bin").join("fake drive");
    executable(&drive, FAKE_DRIVE);
    let binding = root.join("fake binding.json");
    std::fs::write(
        &binding,
        binding_json(&drive, &root.join("drive home"), 4096),
    )
    .unwrap();
    let brief = root.join("state dir").join("z.brief");
    std::fs::write(&brief, BRIEF).unwrap();
    Setup {
        out: root.join("out"),
        pile: root.join("piles").join("colony pile.pile"),
        key: root.join("piles").join("colony.key"),
        drive,
        binding,
        brief,
        root,
        _directory: directory,
    }
}

fn route(role: &str) -> (String, String) {
    let variable = format!(
        "TRIBLESPACE_COLLECTION_{}",
        role.to_uppercase().replace('-', "_")
    );
    (variable, DESCRIPTOR.to_owned())
}

/// The adapter's arguments, as spawn's template substitutes them.
fn adapter_args(setup: &Setup, resume: &str) -> Vec<String> {
    vec![
        "drive-task".to_owned(),
        "--binding".to_owned(),
        setup.binding.display().to_string(),
        "--brief".to_owned(),
        setup.brief.display().to_string(),
        "--spawner".to_owned(),
        "tutor".to_owned(),
        "--name".to_owned(),
        "zooid-name".to_owned(),
        format!("--resume={resume}"),
    ]
}

/// Run `zooid drive-task` as a unit script would: a scrubbed environment,
/// the routes and the run's pile and key exported.
fn adapter(setup: &Setup, routes: &[(String, String)], resume: &str, mode: &str) -> Output {
    Command::new(SPAWN)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PILE", &setup.pile)
        .env("TRIBLESPACE_KEY", &setup.key)
        .env("FAKE_OUT", &setup.out)
        .env("FAKE_MODE", mode)
        .envs(routes.iter().map(|(name, value)| (name, value)))
        .args(adapter_args(setup, resume))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn drive_ran(setup: &Setup) -> bool {
    setup.out.join("args").exists()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_adapter_execs_drive_run_with_the_whole_argv_fresh_and_resumed() {
    let setup = setup();
    let routes = [route("archive"), route("compass"), route("message")];
    let output = adapter(&setup, &routes, "", "silent");
    assert!(output.status.success(), "{}", stderr(&output));
    let args = std::fs::read_to_string(setup.out.join("args")).unwrap();
    let home = setup.root.join("drive home");
    let path = |path: &Path| path.display().to_string();
    let collection = |role: &str| format!("{role}={DESCRIPTOR}");
    let fresh = [
        "run".to_owned(),
        "--mind".to_owned(),
        "mini".to_owned(),
        "--native-persona".to_owned(),
        "zooid-name".to_owned(),
        "--native-faculties-pile".to_owned(),
        path(&setup.pile),
        "--native-faculties-key".to_owned(),
        path(&setup.key),
        "--native-collection".to_owned(),
        collection("archive"),
        "--native-collection".to_owned(),
        collection("compass"),
        "--native-collection".to_owned(),
        collection("message"),
        "--native-archive".to_owned(),
        DESCRIPTOR.to_owned(),
        "--brief-file".to_owned(),
        path(&setup.brief),
        "--brief-from".to_owned(),
        "tutor".to_owned(),
        "--home".to_owned(),
        path(&home.join("session.pile")),
        "--home-key".to_owned(),
        path(&home.join("session.key")),
        "--tenant".to_owned(),
        "fake-tenant".to_owned(),
        "--model".to_owned(),
        "/fake/models/model.pile".to_owned(),
        "--model-root".to_owned(),
        "FAKEROOT".to_owned(),
        "--model-collection".to_owned(),
        MODEL_DESCRIPTOR.to_owned(),
        "--context-window".to_owned(),
        "4096".to_owned(),
        "--no-telemetry".to_owned(),
        "--playground".to_owned(),
        "/fake/playground".to_owned(),
        "--backend".to_owned(),
        "fake-backend".to_owned(),
        "--backend-arg".to_owned(),
        "two words".to_owned(),
    ];
    assert_eq!(args.lines().collect::<Vec<_>>(), fresh);
    // The brief Drive reads is the bytes spawn wrote.
    assert_eq!(std::fs::read(setup.out.join("brief-seen")).unwrap(), BRIEF);
    let log = stderr(&output);
    assert!(log.contains("brief 50 bytes from tutor; fresh"), "{log}");
    assert!(log.contains("zooid drive-task: exec "), "{log}");

    // Resumed: the same line plus where her history ends, and the same
    // brief file, which Drive uses for the tint and does not deliver again.
    let node = "0123456789abcdef0123456789abcdef";
    let output = adapter(&setup, &routes, node, "silent");
    assert!(output.status.success(), "{}", stderr(&output));
    let args = std::fs::read_to_string(setup.out.join("args")).unwrap();
    let at = fresh.iter().position(|arg| arg == "--brief-file").unwrap();
    let mut resumed = fresh.to_vec();
    resumed.splice(at..at, ["--native-resume".to_owned(), node.to_owned()]);
    assert_eq!(args.lines().collect::<Vec<_>>(), resumed);
    assert!(stderr(&output).contains(&format!("resuming at {node}")));
}

#[test]
fn an_explicit_memory_allowance_is_forwarded_independently_of_the_window() {
    let setup = setup();
    let routes = [route("archive")];
    let mut binding: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&setup.binding).unwrap()).unwrap();
    binding["context-window"] = 1_048_576.into();
    for chars in [1_800_000, 0] {
        binding["memory-chars"] = chars.into();
        std::fs::write(&setup.binding, binding.to_string()).unwrap();

        let output = adapter(&setup, &routes, "", "silent");
        assert!(output.status.success(), "{}", stderr(&output));
        let args = std::fs::read_to_string(setup.out.join("args")).unwrap();
        let args: Vec<_> = args.lines().collect();
        assert_eq!(
            args.windows(2)
                .filter(|pair| pair[0] == "--context-window")
                .collect::<Vec<_>>(),
            [["--context-window", "1048576"]]
        );
        assert_eq!(
            args.windows(2)
                .filter(|pair| pair[0] == "--memory-chars")
                .collect::<Vec<_>>(),
            [["--memory-chars", chars.to_string().as_str()]]
        );
    }
}

#[test]
fn drive_exit_codes_pass_through() {
    let setup = setup();
    let routes = [route("archive")];
    let failed = adapter(&setup, &routes, "", "fail");
    assert_eq!(failed.status.code(), Some(3), "{}", stderr(&failed));
    let silent = adapter(&setup, &routes, "", "silent");
    assert_eq!(silent.status.code(), Some(0), "{}", stderr(&silent));
}

/// Each refusal exits nonzero before Drive starts, and says why.
fn refused(setup: &Setup, output: &Output, why: &str) {
    assert!(!output.status.success(), "accepted; wanted: {why}");
    assert!(!drive_ran(setup), "drive ran; wanted: {why}");
    let log = stderr(output);
    assert!(log.contains(why), "{log}");
}

#[test]
fn route_refusals_happen_before_drive_starts() {
    let setup = setup();
    let unknown = [
        route("archive"),
        (
            "TRIBLESPACE_COLLECTION_NOT_A_FACULTY".to_owned(),
            DESCRIPTOR.to_owned(),
        ),
    ];
    let output = adapter(&setup, &unknown, "", "silent");
    refused(&setup, &output, "TRIBLESPACE_COLLECTION_NOT_A_FACULTY");
    let output = adapter(&setup, &[route("compass")], "", "silent");
    refused(&setup, &output, "--native-archive");
}

#[test]
fn a_missing_binding_or_binding_key_refuses() {
    let setup = setup();
    let routes = [route("archive")];
    let full: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&setup.binding).unwrap()).unwrap();
    let mut partial = full.clone();
    partial.as_object_mut().unwrap().remove("tenant");
    std::fs::write(&setup.binding, partial.to_string()).unwrap();
    let output = adapter(&setup, &routes, "", "silent");
    refused(&setup, &output, "missing field `tenant`");
    // A finite task's keys are gone, not kept alongside.
    let mut extra = full;
    extra["turns"] = 5.into();
    std::fs::write(&setup.binding, extra.to_string()).unwrap();
    let output = adapter(&setup, &routes, "", "silent");
    refused(&setup, &output, "unknown field `turns`");
    std::fs::remove_file(&setup.binding).unwrap();
    let output = adapter(&setup, &routes, "", "silent");
    refused(&setup, &output, "read the Drive binding");
}

#[test]
fn an_oversized_or_empty_brief_a_bad_resume_or_no_key_refuses() {
    let setup = setup();
    let routes = [route("archive")];
    let home = setup.root.join("drive home");
    std::fs::write(
        &setup.binding,
        binding_json(&setup.drive, &home, BRIEF.len() - 1),
    )
    .unwrap();
    let output = adapter(&setup, &routes, "", "silent");
    refused(&setup, &output, "nothing is truncated");
    assert_eq!(std::fs::read(&setup.brief).unwrap(), BRIEF);
    std::fs::write(
        &setup.binding,
        binding_json(&setup.drive, &home, BRIEF.len()),
    )
    .unwrap();

    std::fs::write(&setup.brief, b"").unwrap();
    let output = adapter(&setup, &routes, "", "silent");
    refused(&setup, &output, "the brief is empty");
    std::fs::write(&setup.brief, BRIEF).unwrap();

    let output = adapter(&setup, &routes, "0123", "silent");
    refused(&setup, &output, "32-hex");

    let output = Command::new(SPAWN)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PILE", &setup.pile)
        .envs(routes.iter().map(|(name, value)| (name, value)))
        .args(adapter_args(&setup, ""))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    refused(&setup, &output, "TRIBLESPACE_KEY");
}

#[derive(Default)]
struct FakeLauncher {
    started: Mutex<Vec<UnitSpec>>,
}

impl Launcher for FakeLauncher {
    fn start(&self, unit: &UnitSpec) -> Result<()> {
        self.started.lock().unwrap().push(unit.clone());
        Ok(())
    }
    fn state(&self, _unit: &str) -> Result<String> {
        Ok("active".to_owned())
    }
    fn active(&self, _prefix: &str) -> Result<usize> {
        Ok(self.started.lock().unwrap().len())
    }
}

struct StubRenderer;

impl Renderer for StubRenderer {
    fn tint(&self, _storage: &Storage, about: &str) -> Result<WakeTint> {
        Ok(WakeTint {
            about: about.to_owned(),
            scores: None,
            ..Default::default()
        })
    }
    fn wake(
        &self,
        _storage: &Storage,
        _tint: Option<&WakeTint>,
        _memory_chars: usize,
    ) -> Result<String> {
        Ok("STUB-WAKE".to_owned())
    }
}

/// Through spawn itself: a run whose executor template names the adapter.
/// A fresh pile with no route overrides pins no routes, so the run has no
/// archive route for Drive's --native-archive: the adapter refuses before
/// Drive starts, and spawn settles the run as a failure, by its exit, with no message.
#[test]
fn spawn_settles_an_adapter_refusal_as_a_failed_run_without_a_message() {
    let setup = setup();
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path();
    let pile = base.join("spawn.pile");
    let key = base.join("spawn.key");
    std::fs::File::create(&pile).unwrap();
    initialize_signer(&pile, Some(&key)).unwrap();
    let storage = Storage::new(pile, Some(key));
    std::fs::write(base.join("colony.env"), "export TRIBLESPACE_PEERS=none\n").unwrap();
    std::fs::create_dir(base.join("lane")).unwrap();
    std::fs::create_dir(base.join("bin")).unwrap();
    executable(&base.join("bin").join("faculties"), "#!/bin/sh\n");
    // The template is whitespace-separated, so its binding path has none.
    let binding = base.join("binding.json");
    std::fs::copy(&setup.binding, &binding).unwrap();
    let template = format!(
        "{SPAWN} drive-task --binding {} --brief {{brief}} --spawner {{spawner}} --name {{name}} \
         --resume={{resume}}",
        binding.display()
    );
    let launch = LaunchConfig {
        state_dir: base.join("state"),
        claude: PathBuf::from("/fake/claude"),
        model: "fake-model".to_owned(),
        codex: PathBuf::from("/fake/codex"),
        codex_model: None,
        codex_auth: None,
        vibe: PathBuf::from("/fake/vibe"),
        mistral_model: "fake-mistral".to_owned(),
        mistral_key_secret: "00000000000000000000000000000000".to_owned(),
        mistral_max_price: 1.0,
        secrets: "secrets".to_owned(),
        colony_env: Some(base.join("colony.env")),
        path: format!("/usr/bin:/bin:{}", base.join("bin").display()),
        memory_max: "1G".to_owned(),
        spawn: "zooid".to_owned(),
        faculties: PathBuf::from("faculties"),
        local: Some(template.split_whitespace().map(str::to_owned).collect()),
        local_max_runtime: Some(600),
        codex_models: None,
        claude_projects: None,
    };
    let launcher = Arc::new(FakeLauncher::default());
    let spawn = Spawn::new(storage.clone(), launch)
        .with_launcher(launcher.clone())
        .with_renderer(Arc::new(StubRenderer));
    Relations::with_storage(storage.clone())
        .add(
            ProfileInput {
                label: "agent-parent".to_owned(),
                affinities: vec!["zooid".to_owned()],
                ..Default::default()
            },
            None,
            &[],
        )
        .unwrap();
    let launched = spawn
        .run(&RunRequest {
            brief: "Report the doing goals.".to_owned(),
            workdir: base.join("lane"),
            effort: "high".to_owned(),
            spawner: "agent-parent".to_owned(),
            provider: Provider::Local,
            cover: Default::default(),
        })
        .unwrap();
    let started = launcher.started.lock().unwrap()[0].clone();
    assert_eq!(started.runtime_max, Some(600));
    assert!(started.interrupt);
    let output = Command::new("/bin/bash")
        .arg(launched.report.with_extension("sh"))
        .current_dir(base.join("lane"))
        .env_clear()
        .env("HOME", base)
        .env("FAKE_OUT", &setup.out)
        .env("FAKE_MODE", "silent")
        .output()
        .unwrap();
    let log = stderr(&output);
    assert!(!output.status.success(), "{log}");
    assert!(!drive_ran(&setup), "{log}");
    assert!(log.contains("TRIBLESPACE_COLLECTION_ARCHIVE"), "{log}");
    assert!(log.contains("--native-archive"), "{log}");
    let code = output.status.code().unwrap().to_string();
    spawn
        .settle(&SettleRequest {
            name: launched.name.clone(),
            run: launched.run,
            request: None,
            exit: Exit::from_systemd("exited", &code, "exit-code"),
            report: Some(launched.report.clone()),
            usage: None,
        })
        .unwrap();
    let run = spawn.show(&launched.name).unwrap().runs.remove(0);
    assert!(!ended_clean(&run, true), "{}", describe_end(&run));
    assert!(run.report_text.is_none());
}

/// A living zooid through real systemd, stopped as spawn stops a local
/// unit: its runtime limit interrupts the main process (SIGINT, mixed kill
/// mode), the fake Drive tears down and exits 0, and that is a clean end
/// noted as the limit. Skipped where no systemd user manager answers.
#[test]
fn a_living_drive_is_interrupted_by_the_runtime_limit_and_tears_down_cleanly() {
    let reachable = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !reachable {
        eprintln!("skipped: no systemd user manager here");
        return;
    }
    let setup = setup();
    let ended = setup.out.join("ended");
    let unit = format!(
        "spawn-drive-task-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // systemd expands $VARIABLE in Exec lines itself, so the stop-post step
    // is a script that reads the variables systemd gives it.
    // Spawn's stop-post line names an unquoted, unit-safe path (spawn's
    // state files are checked for that), so this one has no space.
    let stop_post = setup.root.parent().unwrap().join("stop-post.sh");
    executable(
        &stop_post,
        &format!(
            "#!/bin/sh\necho \"$EXIT_CODE $EXIT_STATUS $SERVICE_RESULT\" > {}\n",
            faculties::spawn::shell_word(&ended.display().to_string())
        ),
    );
    let spec = UnitSpec {
        unit: unit.clone(),
        workdir: setup.root.clone(),
        script: stop_post.clone(),
        log: setup.out.join("log"),
        memory_max: "1G".to_owned(),
        runtime_max: Some(3),
        interrupt: true,
    };
    // spawn's own unit properties (its stop-post step runs this test's
    // script), with this test's main command.
    let properties: Vec<String> = spec
        .systemd_run_args()
        .into_iter()
        .take_while(|arg| arg != "--")
        .collect();
    let mut command = Command::new("systemd-run");
    command.args(&properties);
    // The user manager hands its own environment to the unit, routes
    // included: start from an empty one, as spawn's unit script unsets them.
    command.args(["--", "/usr/bin/env", "-i"]);
    for (name, value) in [
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
        ("PILE".to_owned(), setup.pile.display().to_string()),
        (
            "TRIBLESPACE_KEY".to_owned(),
            setup.key.display().to_string(),
        ),
        ("FAKE_OUT".to_owned(), setup.out.display().to_string()),
        ("FAKE_MODE".to_owned(), "live".to_owned()),
        route("archive"),
    ] {
        command.arg(format!("{name}={value}"));
    }
    command.arg(SPAWN).args(adapter_args(&setup, ""));
    assert!(command.status().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ended.exists() {
        assert!(Instant::now() < deadline, "{unit} never ended");
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(200));
    let words = std::fs::read_to_string(&ended).unwrap();
    let words: Vec<&str> = words.split_whitespace().collect();
    let exit = Exit::from_systemd(words[0], words[1], words.get(2).copied().unwrap_or(""));
    assert_eq!(exit.result.as_deref(), Some("timeout"), "{words:?}");
    assert_eq!(exit.code, Some(0), "{words:?}");
    assert!(exit.clean(true), "{words:?}");
    assert!(!exit.clean(false));
    assert_eq!(
        std::fs::read_to_string(setup.out.join("teardown")).unwrap(),
        "torn-down\n"
    );
}

/// A binding whose playground backend-args name the tenant and the cgroup.
fn delegating(setup: &Setup) {
    let mut binding: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&setup.binding).unwrap()).unwrap();
    binding["tenant"] = "zooid-a".into();
    binding["provider"]["playground"]["backend-args"] = serde_json::json!([
        "--bubblewrap-tenant",
        "{tenant}",
        "--bubblewrap-cgroup-parent",
        "{cgroup}"
    ]);
    std::fs::write(&setup.binding, binding.to_string()).unwrap();
}

#[test]
fn a_cgroup_placeholder_refuses_outside_a_delegated_unit_before_drive_starts() {
    let setup = setup();
    delegating(&setup);
    let output = adapter(&setup, &[route("archive")], "", "silent");
    refused(&setup, &output, "supervisor subgroup");
}

/// The delegation through real systemd: spawn's local unit properties
/// delegate cpu, memory and pids with a `supervisor` subgroup, and the
/// adapter, inside it, enables the controllers for the unit's children and
/// passes the unit's cgroup and the binding's tenant on. Skipped where no
/// systemd user manager answers.
#[test]
fn inside_a_local_unit_the_adapter_passes_its_delegated_cgroup_with_controllers() {
    let reachable = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !reachable {
        eprintln!("skipped: no systemd user manager here");
        return;
    }
    let setup = setup();
    delegating(&setup);
    let unit = format!(
        "spawn-drive-task-cgroup-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let stop_post = setup.root.parent().unwrap().join("stop-post.sh");
    executable(&stop_post, "#!/bin/sh\nexit 0\n");
    let spec = UnitSpec {
        unit: unit.clone(),
        workdir: setup.root.clone(),
        script: stop_post,
        log: setup.out.join("log"),
        memory_max: "1G".to_owned(),
        runtime_max: Some(60),
        interrupt: true,
    };
    let properties: Vec<String> = spec
        .systemd_run_args()
        .into_iter()
        .take_while(|arg| arg != "--")
        .collect();
    let mut command = Command::new("systemd-run");
    command.args(&properties).args(["--wait", "--"]);
    command.args(["/usr/bin/env", "-i"]);
    for (name, value) in [
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
        ("PILE".to_owned(), setup.pile.display().to_string()),
        (
            "TRIBLESPACE_KEY".to_owned(),
            setup.key.display().to_string(),
        ),
        ("FAKE_OUT".to_owned(), setup.out.display().to_string()),
        ("FAKE_MODE".to_owned(), "cgroup".to_owned()),
        route("archive"),
    ] {
        command.arg(format!("{name}={value}"));
    }
    command.arg(SPAWN).args(adapter_args(&setup, ""));
    let status = command.status().unwrap();
    let log = std::fs::read_to_string(setup.out.join("log")).unwrap_or_default();
    assert!(status.success(), "{unit}: {log}");
    let own = std::fs::read_to_string(setup.out.join("own")).unwrap();
    let own = own.trim();
    assert!(
        own.ends_with(&format!("/{unit}.service/supervisor")),
        "{own}"
    );
    let parent = format!("/sys/fs/cgroup{}", own.strip_suffix("/supervisor").unwrap());
    let args = std::fs::read_to_string(setup.out.join("args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    let backend: Vec<&str> = args
        .windows(2)
        .filter(|pair| pair[0] == "--backend-arg")
        .map(|pair| pair[1])
        .collect();
    assert_eq!(
        backend,
        [
            "--bubblewrap-tenant",
            "zooid-a",
            "--bubblewrap-cgroup-parent",
            parent.as_str()
        ]
    );
    let enabled = std::fs::read_to_string(setup.out.join("subtree_control")).unwrap();
    for controller in ["cpu", "memory", "pids"] {
        assert!(
            enabled.split_whitespace().any(|word| word == controller),
            "{controller}: {enabled:?}"
        );
    }
}
