//! `zooid drive-task`, the local executor for a finite Drive task, run as
//! the real `zooid` binary against a fake `drive` script. Nothing here
//! starts Drive, a model or a provider.

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

/// A fake `drive`: leaves its arguments one per line, the bytes it got on
/// stdin and a copy of the prompt file it was named in `$FAKE_OUT`, then
/// does what `$FAKE_MODE` says.
const FAKE_DRIVE: &str = r#"#!/bin/sh
out="$FAKE_OUT"
: > "$out/args"
for word in "$@"; do printf '%s\n' "$word" >> "$out/args"; done
wc -c > "$out/stdin"
prompt=""; message=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prompt-file) prompt="$2"; shift ;;
    --message-file) message="$2"; shift ;;
  esac
  shift
done
cp "$prompt" "$out/prompt-seen"
case "$FAKE_MODE" in
  message) printf 'fake message' > "$message" ;;
  fail) exit 3 ;;
  silent) exit 0 ;;
  sleep) exec sleep 60 ;;
esac
"#;

/// A directory whose path has a space in it, with a fake drive, a binding,
/// a prompt and an output directory.
struct Setup {
    _directory: tempfile::TempDir,
    root: PathBuf,
    drive: PathBuf,
    binding: PathBuf,
    prompt: PathBuf,
    message: PathBuf,
    out: PathBuf,
    pile: PathBuf,
    key: PathBuf,
}

/// Bytes a prompt carries through untouched: multi-byte characters, a tab,
/// a carriage return, trailing spaces and no final newline.
const PROMPT: &[u8] = "# Wake\n\nGrüße, ✓ tab\there\r\nlast line   ".as_bytes();

fn binding_json(drive: &Path, home: &Path, margin: u64, max_prompt: usize) -> String {
    serde_json::json!({
        "drive": drive,
        "tenant": "fake-tenant",
        "home": home.join("session.pile"),
        "home-key": home.join("session.key"),
        "model": "/fake/models/model.pile",
        "model-root": "FAKEROOT",
        "model-collection": MODEL_DESCRIPTOR,
        "resident-bytes": 1000,
        "chunk-bytes": 100,
        "context-window": 4096,
        "turns": 8,
        "max-prompt-bytes": max_prompt,
        "max-message-bytes": 300,
        "teardown-margin-secs": margin,
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
        binding_json(&drive, &root.join("drive home"), 30, 4096),
    )
    .unwrap();
    let prompt = root.join("state dir").join("z.prompt");
    std::fs::write(&prompt, PROMPT).unwrap();
    Setup {
        message: root.join("state dir").join("z.report"),
        out: root.join("out"),
        pile: root.join("piles").join("colony pile.pile"),
        key: root.join("piles").join("colony.key"),
        drive,
        binding,
        prompt,
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

/// Run `zooid drive-task` as a unit script would: a scrubbed environment,
/// the routes and the run's pile and key exported, the prompt on stdin.
fn adapter(setup: &Setup, routes: &[(String, String)], runtime: u64, mode: &str) -> Output {
    let mut command = Command::new(SPAWN);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PILE", &setup.pile)
        .env("TRIBLESPACE_KEY", &setup.key)
        .env("FAKE_OUT", &setup.out)
        .env("FAKE_MODE", mode)
        .envs(routes.iter().map(|(name, value)| (name, value)))
        .args(["drive-task", "--binding"])
        .arg(&setup.binding)
        .arg("--prompt")
        .arg(&setup.prompt)
        .arg("--message")
        .arg(&setup.message)
        .args(["--name", "zooid-name", "--runtime", &runtime.to_string()])
        .stdin(std::fs::File::open(&setup.prompt).unwrap());
    command.output().unwrap()
}

fn drive_ran(setup: &Setup) -> bool {
    setup.out.join("args").exists()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_adapter_execs_drive_task_with_the_whole_argv() {
    let setup = setup();
    let routes = [route("archive"), route("compass"), route("message")];
    let output = adapter(&setup, &routes, 3600, "message");
    assert!(output.status.success(), "{}", stderr(&output));
    let args = std::fs::read_to_string(setup.out.join("args")).unwrap();
    let home = setup.root.join("drive home");
    let path = |path: &Path| path.display().to_string();
    let collection = |role: &str| format!("{role}={DESCRIPTOR}");
    let expected = [
        "task".to_owned(),
        "--prompt-file".to_owned(),
        path(&setup.prompt),
        "--max-prompt-bytes".to_owned(),
        "4096".to_owned(),
        "--message-file".to_owned(),
        path(&setup.message),
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
        "--resident-bytes".to_owned(),
        "1000".to_owned(),
        "--chunk-bytes".to_owned(),
        "100".to_owned(),
        "--context-window".to_owned(),
        "4096".to_owned(),
        "--turns".to_owned(),
        "8".to_owned(),
        "--deadline-ms".to_owned(),
        "3570000".to_owned(),
        "--max-message-bytes".to_owned(),
        "300".to_owned(),
        "--no-telemetry".to_owned(),
        "--playground".to_owned(),
        "/fake/playground".to_owned(),
        "--backend".to_owned(),
        "fake-backend".to_owned(),
        "--backend-arg".to_owned(),
        "two words".to_owned(),
    ];
    assert_eq!(args.lines().collect::<Vec<_>>(), expected);
    // The prompt Drive reads is the bytes spawn wrote, and stdin still
    // carries them whole.
    assert_eq!(
        std::fs::read(setup.out.join("prompt-seen")).unwrap(),
        PROMPT
    );
    let stdin = std::fs::read_to_string(setup.out.join("stdin")).unwrap();
    assert_eq!(stdin.trim().parse::<usize>().unwrap(), PROMPT.len());
    assert_eq!(std::fs::read(&setup.message).unwrap(), b"fake message");
    // The run's log says what Drive was given.
    let log = stderr(&output);
    assert!(
        log.contains("deadline-ms 3570000 (runtime 3600 s less 30 s"),
        "{log}"
    );
    assert!(log.contains("zooid drive-task: exec "), "{log}");
}

#[test]
fn drive_exit_codes_pass_through_and_no_message_is_invented() {
    let setup = setup();
    let routes = [route("archive")];
    let failed = adapter(&setup, &routes, 3600, "fail");
    assert_eq!(failed.status.code(), Some(3), "{}", stderr(&failed));
    assert!(!setup.message.exists());
    let silent = adapter(&setup, &routes, 3600, "silent");
    assert_eq!(silent.status.code(), Some(0), "{}", stderr(&silent));
    assert!(!setup.message.exists());
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
    let output = adapter(&setup, &unknown, 3600, "message");
    refused(&setup, &output, "TRIBLESPACE_COLLECTION_NOT_A_FACULTY");
    let output = adapter(&setup, &[route("compass")], 3600, "message");
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
    let output = adapter(&setup, &routes, 3600, "message");
    refused(&setup, &output, "missing field `tenant`");
    let mut extra = full;
    extra["exec-timeout"] = 5.into();
    std::fs::write(&setup.binding, extra.to_string()).unwrap();
    let output = adapter(&setup, &routes, 3600, "message");
    refused(&setup, &output, "unknown field `exec-timeout`");
    std::fs::remove_file(&setup.binding).unwrap();
    let output = adapter(&setup, &routes, 3600, "message");
    refused(&setup, &output, "read the Drive binding");
}

#[test]
fn a_short_runtime_an_oversized_prompt_an_existing_message_file_or_no_key_refuses() {
    let setup = setup();
    let routes = [route("archive")];
    let output = adapter(&setup, &routes, 30, "message");
    refused(&setup, &output, "SPAWN_LOCAL_MAX_RUNTIME");

    std::fs::write(
        &setup.binding,
        binding_json(
            &setup.drive,
            &setup.root.join("drive home"),
            30,
            PROMPT.len() - 1,
        ),
    )
    .unwrap();
    let output = adapter(&setup, &routes, 3600, "message");
    refused(&setup, &output, "nothing is truncated");
    assert_eq!(std::fs::read(&setup.prompt).unwrap(), PROMPT);
    std::fs::write(
        &setup.binding,
        binding_json(
            &setup.drive,
            &setup.root.join("drive home"),
            30,
            PROMPT.len(),
        ),
    )
    .unwrap();

    std::fs::write(&setup.message, "an old message").unwrap();
    let output = adapter(&setup, &routes, 3600, "message");
    refused(&setup, &output, "already exists");
    std::fs::remove_file(&setup.message).unwrap();

    let output = Command::new(SPAWN)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PILE", &setup.pile)
        .envs(routes.iter().map(|(name, value)| (name, value)))
        .args(["drive-task", "--binding"])
        .arg(&setup.binding)
        .arg("--prompt")
        .arg(&setup.prompt)
        .arg("--message")
        .arg(&setup.message)
        .args(["--name", "zooid-name", "--runtime", "3600"])
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
        "{SPAWN} drive-task --binding {} --prompt {{prompt}} --message {{message}} --name {{name}} \
         --runtime {{runtime}}",
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
    assert_eq!(launcher.started.lock().unwrap()[0].runtime_max, Some(600));
    let output = Command::new("/bin/bash")
        .arg(launched.report.with_extension("sh"))
        .current_dir(base.join("lane"))
        .env_clear()
        .env("HOME", base)
        .env("FAKE_OUT", &setup.out)
        .env("FAKE_MODE", "message")
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
    assert!(!ended_clean(&run), "{}", describe_end(&run));
    assert!(run.report_text.is_none());
}

/// The runtime limit through real systemd: the adapter derives a deadline
/// from a three-second limit, the fake Drive ignores it and sleeps, and
/// RuntimeMaxSec stops the unit. Its result is a timeout, never a clean end.
/// Skipped where no systemd user manager answers.
#[test]
fn a_drive_that_overruns_is_stopped_by_the_runtime_limit() {
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
    std::fs::write(
        &setup.binding,
        binding_json(&setup.drive, &setup.root.join("drive home"), 1, 4096),
    )
    .unwrap();
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
    let stop_post = setup.out.join("stop-post.sh");
    executable(
        &stop_post,
        &format!(
            "#!/bin/sh\necho \"$EXIT_CODE $EXIT_STATUS $SERVICE_RESULT\" > {}\n",
            faculties::spawn::shell_word(&ended.display().to_string())
        ),
    );
    let mut command = Command::new("systemd-run");
    command.args(["--user", "--unit", &unit, "--collect", "--quiet"]);
    command.args(["-p", "RuntimeMaxSec=3"]);
    command
        .arg("-p")
        .arg(format!("ExecStopPost=/bin/sh \"{}\"", stop_post.display()));
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
        ("FAKE_MODE".to_owned(), "sleep".to_owned()),
        route("archive"),
    ] {
        command.arg(format!("{name}={value}"));
    }
    command
        .arg(SPAWN)
        .args(["drive-task", "--binding"])
        .arg(&setup.binding)
        .arg("--prompt")
        .arg(&setup.prompt)
        .arg("--message")
        .arg(&setup.message)
        .args(["--name", "zooid-name", "--runtime", "3"]);
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
    assert!(!exit.clean());
    assert!(drive_ran(&setup));
    let args = std::fs::read_to_string(setup.out.join("args")).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let at = lines
        .iter()
        .position(|line| *line == "--deadline-ms")
        .unwrap();
    assert_eq!(lines[at + 1], "2000");
    assert!(!setup.message.exists());
}
