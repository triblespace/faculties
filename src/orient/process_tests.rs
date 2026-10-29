//! Real subprocess controls, authored but not run by source preparation.
//! Only generated scratch pile/key/callback files; no installed owner/route.
use crate::{clock, collection_names, message, relations, schemas, storage};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use triblespace::prelude::*;

struct Fixture {
    dir: tempfile::TempDir,
    pile: PathBuf,
    key: PathBuf,
    who: Id,
    event: Id,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("synthetic.pile");
        let key = dir.path().join("synthetic.key");
        fs::File::create(&pile).unwrap();
        let signer = storage::initialize_signer(&pile, Some(&key)).unwrap();
        let mut store = storage::open_pile_strict_as(&pile, signer.verifying_key()).unwrap();
        let who = *fucid();
        let sender = *fucid();
        let people = collection_names::open_configured(&mut store,
            schemas::relations::DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        for person in [who, sender] {
            let (fragment, _, _) = relations::person_fragment(person, relations::ProfileInput {
                label: format!("fixture-{person:x}"), ..Default::default()
            }).unwrap();
            store.commit(people, &signer, fragment).unwrap();
        }
        let messages = collection_names::open_configured(&mut store,
            schemas::message::DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        let mut fragment = Fragment::empty();
        let body = fragment.put("synthetic immutable report".to_owned());
        let envelope = message::envelope_fragment(sender, who, body, clock::point_now().unwrap(), None, None);
        let event = envelope.root().unwrap();
        fragment += envelope;
        store.commit(messages, &signer, fragment).unwrap();
        store.close().unwrap();
        fs::write(dir.path().join("attempts"), "").unwrap();
        fs::write(dir.path().join("callback.sh"), r#"set -eu
root=$1
mode=$2
printf 'x\n' >> "$root/attempts"
n=$(wc -l < "$root/attempts" | tr -d ' ')
cat > "$root/report.$n"
test ! -e "$root/paused" || exit 78
: > "$root/ready.$n"
case "$mode" in
  always) exit 75 ;;
  once) test "$n" -gt 1 || exit 75 ;;
  unknown) exit 1 ;;
esac
"#).unwrap();
        Self { dir, pile, key, who, event }
    }

    fn spawn(&self, mode: &str, run_for: &str) -> Owned {
        let executable = PathBuf::from(std::env::var_os("ORIENT_TEST_BIN")
            .expect("controller must supply its exact preserved candidate binary"));
        assert!(executable.is_absolute() && executable.is_file());
        let child = Command::new(executable)
            .env_clear().env("HOME", self.dir.path()).env("PATH", "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", self.dir.path()).env("XDG_STATE_HOME", self.dir.path())
            .env("XDG_CACHE_HOME", self.dir.path()).env("TMPDIR", self.dir.path())
            .current_dir(self.dir.path()).process_group(0)
            .args(["--pile"]).arg(&self.pile).args(["--key"]).arg(&self.key)
            .args(["--persona", &format!("{:x}", self.who), "daemon",
                "--retry-known-not-submitted", "--callback-timeout", "2s",
                "--run-for", run_for, "--poll-ms", "10", "--callback", "/bin/sh",
                "--callback-arg"]).arg(self.dir.path().join("callback.sh"))
            .arg("--callback-arg").arg(self.dir.path()).args(["--callback-arg", mode])
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(self.dir.path().join("stdout")).unwrap()))
            .stderr(Stdio::from(fs::File::create(self.dir.path().join("stderr")).unwrap()))
            .spawn().unwrap();
        eprintln!("synthetic daemon started pid={} mode={mode} run_for={run_for}", child.id());
        Owned { child, reaped: false }
    }

    fn ready(&self, child: &mut Owned) {
        let end = Instant::now() + Duration::from_secs(20);
        while !self.dir.path().join("ready.1").exists() {
            if let Some(status) = child.child.try_wait().unwrap() {
                child.reaped = true;
                panic!("exited before first callback: {status}");
            }
            assert!(Instant::now() < end, "first callback not reached");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(fs::read_to_string(self.dir.path().join("report.1")).unwrap()
            .starts_with("Orient report prepared at "));
        eprintln!("synthetic callback ready pid={} attempt=1", child.child.id());
    }

    fn attempts(&self) -> usize {
        let count = fs::read_to_string(self.dir.path().join("attempts")).unwrap().lines().count();
        eprintln!("synthetic callback attempts={count}");
        count
    }

    fn presented(&self) -> bool {
        let signer = storage::load_signer(&self.pile, Some(&self.key)).unwrap();
        let mut store = storage::open_pile_strict_as(&self.pile, signer.verifying_key()).unwrap();
        let collection = store.collection(schemas::orient::RECEIPT_COLLECTION_NAME,
            collection_names::private_policy(signer.verifying_key())).unwrap();
        let snapshot = store.snapshot().unwrap();
        let (facts, _) = storage::read_fact_collection(collection, &snapshot).unwrap();
        let present = triblespace::macros::exists!(triblespace::macros::pattern!(&facts, [{
            schemas::orient::presentation::event: &self.event,
        }]));
        drop(facts);
        drop(snapshot);
        store.close().unwrap();
        eprintln!("synthetic event Presented={present}");
        present
    }
}

struct Owned { child: Child, reaped: bool }
impl Owned {
    fn wait(&mut self) -> ExitStatus {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.reaped = true;
                eprintln!("synthetic daemon reaped pid={} status={status}", self.child.id());
                return status;
            }
            assert!(Instant::now() < end, "child exceeded finite test wait");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.reaped {
            // This test creates the process group and never targets another owner.
            unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL); }
            let _ = self.child.wait();
        }
    }
}

#[test]
#[ignore = "requires separately built ORIENT_TEST_BIN; explicit controller phase"]
fn daemon_retry_subprocess_same_report_then_accepts() {
    let f = Fixture::new();
    let mut child = f.spawn("once", "12s");
    f.ready(&mut child);
    assert!(child.wait().success());
    assert_eq!(f.attempts(), 2);
    assert_eq!(fs::read(f.dir.path().join("report.1")).unwrap(),
               fs::read(f.dir.path().join("report.2")).unwrap());
    assert!(f.presented());
}

#[test]
#[ignore = "requires separately built ORIENT_TEST_BIN; explicit controller phase"]
fn daemon_retry_subprocess_run_for_expires_without_receipt() {
    let f = Fixture::new();
    let started = Instant::now();
    let mut child = f.spawn("always", "12s");
    f.ready(&mut child);
    assert!(child.wait().success());
    assert!(started.elapsed() < Duration::from_secs(18));
    assert!(f.attempts() <= 2);
    assert!(!f.presented());
}

#[test]
#[ignore = "requires separately built ORIENT_TEST_BIN; explicit controller phase"]
fn daemon_retry_subprocess_sigterm_during_backoff() {
    let f = Fixture::new();
    let mut child = f.spawn("always", "30s");
    f.ready(&mut child);
    std::thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    assert_eq!(unsafe { libc::kill(child.child.id() as i32, libc::SIGTERM) }, 0);
    eprintln!("sent actual SIGTERM to synthetic daemon pid={}", child.child.id());
    assert!(child.wait().success());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(f.attempts(), 1);
    assert!(!f.presented());
}

#[test]
#[ignore = "requires separately built ORIENT_TEST_BIN; explicit controller phase"]
fn daemon_retry_subprocess_pause_during_backoff_is_terminal() {
    let f = Fixture::new();
    let mut child = f.spawn("always", "20s");
    f.ready(&mut child);
    fs::write(f.dir.path().join("paused"), "").unwrap();
    assert!(!child.wait().success());
    assert_eq!(f.attempts(), 2);
    assert!(!f.presented());
}

#[test]
#[ignore = "requires separately built ORIENT_TEST_BIN; explicit controller phase"]
fn daemon_retry_subprocess_unknown_is_not_retried() {
    let f = Fixture::new();
    let mut child = f.spawn("unknown", "20s");
    f.ready(&mut child);
    assert!(!child.wait().success());
    assert_eq!(f.attempts(), 1);
    assert!(!f.presented());
}
