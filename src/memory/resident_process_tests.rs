//! Separately admitted operational fixture for the actual two CLI binaries.
//! The outer controller must supply bounded synthetic storage and a delegated
//! cgroup. This ignored test never selects a user's default pile or signer.
#![cfg(target_os = "linux")]

use crate::out::{Out, Part};
use crate::storage::{CollectionRoutes, Storage};
use anyhow::{ensure, Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, CollectionStoreExt};

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait(child: &mut Child, seconds: u64) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        ensure!(
            Instant::now() < deadline,
            "synthetic child exceeded deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Explicit paths let the lean library test artifact exercise the separately
// built, controller-verified CLIs without Cargo constructing all binary targets.
fn binary(name: &str) -> Result<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name).context("explicit test binary required")?);
    ensure!(
        path.is_absolute() && fs::canonicalize(&path)? == path,
        "test binary must be canonical"
    );
    let metadata = fs::metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "test binary must be a regular executable"
    );
    Ok(path)
}

#[test]
#[ignore = "requires separately admitted native binaries, bounded synthetic storage and outer cgroup"]
fn real_cli_create_show_and_control_eof_preserve_native_semantics() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("FACULTIES_NATIVE_SYNTHETIC_ROOT")
            .context("explicit synthetic root required")?,
    );
    ensure!(
        root.is_absolute() && fs::canonicalize(&root)? == root,
        "synthetic root must be canonical"
    );
    // Creating a new private directory refuses collisions; all failure evidence
    // remains under the controller-owned root for export before unmount.
    let root = root.join(format!("memory-cli-{}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    let pile = root.join("synthetic.pile");
    let key = root.join("synthetic.key");
    fs::File::create(&pile)?;
    let signer = crate::storage::initialize_signer(&pile, Some(&key))?;
    let storage = Storage::shared_local(pile.clone(), Some(key.clone()));
    let scope = crate::schemas::memory::DEFAULT_SCOPE_ID;
    let handle = storage.with_store(|store, _, _| {
        Ok(store
            .collection(
                crate::collection_names::require_name(scope),
                CollectionPolicy::new(
                    AdmissionPolicy::direct(signer.verifying_key()),
                    AdmissionPolicy::direct(signer.verifying_key()),
                ),
            )?
            .handle())
    })?;
    storage.close()?;
    let socket = root.join("socket");
    let config = root.join("config.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&config)?;
    file.write_all(&serde_json::to_vec(&serde_json::json!({"socket":socket, "pile":pile, "key":key, "expected_signer":hex::encode(signer.verifying_key().to_bytes()), "memory_collection":hex::encode(handle.raw), "request_timeout_ms":10000, "min_free_bytes":1024 * 1024}))?)?;
    let mut worker = Owned(
        Command::new(binary("FACULTIES_NATIVE_TEST_WORKER")?)
            .args(["memory-mcp", "--config"])
            .arg(&config)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let stdout = worker.0.stdout.as_mut().unwrap();
    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "nonblocking readiness"
    );
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut ready = Vec::new();
    loop {
        let mut buffer = [0u8; 32];
        match stdout.read(&mut buffer) {
            Ok(0) => anyhow::bail!("worker readiness EOF"),
            Ok(count) => ready.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if ready.contains(&b'\n') {
            break;
        }
        ensure!(
            Instant::now() < deadline && ready.len() <= 32,
            "worker readiness deadline/budget"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(ready, b"{\"ready\":true}\n");
    let cli = |args: &[&str]| -> Result<(bool, Vec<u8>, Vec<u8>)> {
        let mut child = Owned(
            Command::new(binary("FACULTIES_NATIVE_TEST_CLIENT")?)
                .args(args)
                .env_clear()
                .env("FACULTIES_MEMORY_SOCKET", &socket)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
        let success = wait(&mut child.0, 15)?.success();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        child
            .0
            .stdout
            .take()
            .unwrap()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut stdout)?;
        child
            .0
            .stderr
            .take()
            .unwrap()
            .take(16 * 1024 + 1)
            .read_to_end(&mut stderr)?;
        ensure!(
            stdout.len() <= 1024 * 1024 && stderr.len() <= 16 * 1024,
            "CLI output budget"
        );
        Ok((success, stdout, stderr))
    };
    let summary = "native literal @/private/host\nline two";
    let range = "2026-09-01T00:00:00..2026-09-02T00:00:00";
    let (success, receipt, diagnostics) = cli(&["create", range, summary])?;
    assert!(success, "{}", String::from_utf8_lossy(&diagnostics));
    assert!(diagnostics.is_empty());
    let receipt = String::from_utf8(receipt)?;
    assert_eq!(receipt.lines().count(), 2);
    assert!(receipt.starts_with(&format!("range: {range}\n")));
    let id = receipt
        .lines()
        .nth(1)
        .unwrap()
        .strip_prefix("id: ")
        .context("canonical receipt id")?;
    // A separate host reader of exactly the Drive Memory route observes the
    // acknowledged write while the native worker still owns its shared store.
    let selected = Storage::new(pile, Some(key))
        .with_collections(CollectionRoutes::from([(scope, handle)]))
        .with_expected_signer(signer.verifying_key());
    let mut expected = String::new();
    crate::memory::Memory::with_storage(selected).show(
        id,
        &mut Out::new(&mut |part| {
            if let Part::Text { text } = part {
                expected.push_str(&text);
            }
            Ok(())
        }),
    )?;
    let (success, shown, diagnostics) = cli(&[id])?;
    assert!(success && diagnostics.is_empty());
    assert_eq!(shown, expected.as_bytes());
    let (success, shown, diagnostics) = cli(&["meta", id])?;
    assert!(!success && shown.is_empty());
    assert!(String::from_utf8_lossy(&diagnostics).contains("unsupported"));
    worker.0.stdin.take();
    assert!(wait(&mut worker.0, 25)?.success());
    assert!(!socket.exists());
    Ok(())
}

#[test]
#[ignore = "requires separately admitted native binary, bounded synthetic storage and outer cgroup"]
fn watchdog_exits_with_full_stderr_and_blocked_startup() -> Result<()> {
    let worker_binary = binary("FACULTIES_NATIVE_TEST_WORKER")?;
    let root = PathBuf::from(
        std::env::var_os("FACULTIES_NATIVE_SYNTHETIC_ROOT")
            .context("explicit synthetic root required")?,
    );
    ensure!(
        root.is_absolute() && fs::canonicalize(&root)? == root,
        "synthetic root must be canonical"
    );
    let root = root.join(format!("memory-watchdog-{}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    let pile = root.join("synthetic.pile");
    let key = root.join("synthetic.key");
    fs::File::create(&pile)?;
    let signer = crate::storage::initialize_signer(&pile, Some(&key))?;
    let storage = Storage::shared_local(pile.clone(), Some(key.clone()));
    let handle = storage.with_store(|store, _, _| {
        Ok(store
            .collection(
                crate::collection_names::require_name(crate::schemas::memory::DEFAULT_SCOPE_ID),
                CollectionPolicy::new(
                    AdmissionPolicy::direct(signer.verifying_key()),
                    AdmissionPolicy::direct(signer.verifying_key()),
                ),
            )?
            .handle())
    })?;
    storage.close()?;
    drop(storage);
    let socket = root.join("socket");
    let config = root.join("config.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&config)?;
    file.write_all(&serde_json::to_vec(&serde_json::json!({
        "socket": socket,
        "pile": pile,
        "key": key,
        "expected_signer": hex::encode(signer.verifying_key().to_bytes()),
        "memory_collection": hex::encode(handle.raw),
        "request_timeout_ms": 1000,
        "min_free_bytes": 1024 * 1024,
    }))?)?;
    drop(file);

    // One nonblocking attempt: fixture lock acquisition must itself never hang.
    // PileFile::refresh takes LOCK_SH. Startup reaches it through
    // Storage::with_store -> open_store_as -> open_pile_strict_as, after the
    // resident watchdog is armed. This exclusive lock is held until exit.
    let locked = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&pile)?;
    ensure!(
        unsafe { libc::flock(locked.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "synthetic pile lock unavailable: {}",
        std::io::Error::last_os_error()
    );

    let mut descriptors = [-1; 2];
    ensure!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } == 0,
        "create synthetic stderr pipe: {}",
        std::io::Error::last_os_error()
    );
    // Both descriptors become owned immediately, including on later failure.
    let reader = unsafe { fs::File::from_raw_fd(descriptors[0]) };
    let mut writer = unsafe { fs::File::from_raw_fd(descriptors[1]) };
    let fd = writer.as_raw_fd();
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, 4096) } >= 0,
        "set finite stderr pipe capacity"
    );
    let capacity = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
    ensure!(
        (4096..=65536).contains(&capacity),
        "stderr pipe capacity bound"
    );
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "make fixture pipe fill nonblocking"
    );
    let filler = [0xa5u8; 4096];
    let mut filled = 0;
    loop {
        match writer.write(&filler) {
            Ok(count) => {
                ensure!(count > 0, "stderr pipe accepted no bytes");
                filled += count;
                ensure!(
                    filled <= capacity as usize,
                    "stderr fill exceeded observed capacity"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(filled == capacity as usize, "stderr pipe is not full");
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == 0,
        "restore blocking stderr for actual worker"
    );

    let started = Instant::now();
    // The temporary Command drops its copy of the writer after spawn. The
    // reader remains undrained and control stdin stays open until after exit.
    let mut worker = Owned(
        Command::new(worker_binary)
            .args(["memory-mcp", "--config"])
            .arg(&config)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(writer))
            .spawn()?,
    );
    let status = wait(&mut worker.0, 5)?;
    assert_eq!(
        status.code(),
        Some(70),
        "watchdog must terminate the actual CLI"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut stdout = Vec::new();
    worker
        .0
        .stdout
        .take()
        .unwrap()
        .take(33)
        .read_to_end(&mut stdout)?;
    assert!(
        stdout.is_empty(),
        "blocked preflight must not announce readiness"
    );
    assert!(!socket.exists());
    let mut stderr = Vec::new();
    reader.take(capacity as u64 + 1).read_to_end(&mut stderr)?;
    assert_eq!(stderr, vec![0xa5; capacity as usize]);
    // Closing this sole fixture descriptor releases the lock only after all
    // exit/output assertions; error unwinding also closes it through File Drop.
    drop(locked);
    Ok(())
}
