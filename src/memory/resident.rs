//! One provider-owned native Memory process. No catalog, network acquisition,
//! endpoint discovery, child command, or automatic restart lives here.
//! Stdin EOF stops admission; an admitted operation finishes and flushes before
//! exit. A deadline exits unsuccessfully: the provider must retain recovery
//! evidence, and a disconnected writer must reconcile through ordinary reads.

use std::fs::{self, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use serde_json::json;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::inline::encodings::hash::{Blake3, Handle, Hash};

use crate::mcp::local::{write_frame, TimedRead, DIAGNOSTIC_BYTES, FRAME_BYTES};
use crate::mcp::{self, Limits, Server};
use crate::storage::{CollectionRoutes, Storage};

/// Host-only configuration. The descriptor and signer are exact identities,
/// not collection names, ambient role configuration, or guest-selected paths.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub socket: PathBuf,
    pub pile: PathBuf,
    pub key: PathBuf,
    pub expected_signer: String,
    pub memory_collection: String,
    pub min_free_bytes: u64,
    pub request_timeout_ms: u64,
}

fn regular(path: &Path) -> Result<fs::File> {
    ensure!(
        path.is_absolute() && fs::canonicalize(path)? == path,
        "native path must be absolute and canonical"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "native path must be a regular file"
    );
    Ok(file)
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        let file = regular(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.nlink() == 1,
            "native configuration must be private and provider-owned"
        );
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 16 * 1024,
            "native configuration exceeds budget"
        );
        let config: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            (100..=60_000).contains(&config.request_timeout_ms),
            "native operation deadline must be 100..60000 ms"
        );
        ensure!(
            config.min_free_bytes > 0,
            "native Memory requires an explicit disk-pressure threshold"
        );
        regular(&config.pile)?;
        regular(&config.key)?;
        let parent = config.socket.parent().context("socket parent missing")?;
        ensure!(
            config.socket.is_absolute() && fs::canonicalize(parent)? == parent,
            "socket parent must be canonical"
        );
        let metadata = fs::symlink_metadata(parent)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "socket parent must be private and provider-owned"
        );
        ensure!(
            config.socket.as_os_str().as_bytes().len() < 108,
            "native socket path exceeds Linux limit"
        );
        Ok(config)
    }

    fn storage(&self) -> Result<Storage> {
        let bytes: [u8; 32] = hex::decode(&self.expected_signer)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected signer must contain 32 bytes"))?;
        let signer = VerifyingKey::from_bytes(&bytes)?;
        let handle = Handle::<SimpleArchive>::from_hash(Hash::<Blake3>::from_hex(&self.memory_collection)?);
        let routes = CollectionRoutes::from([(crate::schemas::memory::DEFAULT_SCOPE_ID, handle)]);
        Ok(
            Storage::shared_local(self.pile.clone(), Some(self.key.clone()))
                .with_collections(routes)
                .with_expected_signer(signer),
        )
    }
}

/// Pressure admission only: other host writers can consume space after this
/// observation. It is deliberately not a quota or an aggregate append bound.
fn check_space(path: &Path, minimum: u64) -> Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), &mut stats) } == 0,
        "observe native pile capacity: {}",
        std::io::Error::last_os_error()
    );
    let available = (stats.f_bavail as u128) * (stats.f_frsize as u128);
    ensure!(
        available >= minimum as u128 && stats.f_favail > 0,
        "native Memory stopped by host disk pressure"
    );
    Ok(())
}

fn check_pile(path: &Path, identity: (u64, u64)) -> Result<()> {
    let current = fs::symlink_metadata(path)?;
    ensure!(current.is_file() && (current.dev(), current.ino()) == identity, "native Memory pile path was replaced; stop before reopening a different view");
    Ok(())
}

/// This process has exactly one dispatch at a time. A blocked filesystem call
/// cannot prevent finite provider shutdown: the watchdog exits nonzero without
/// claiming the admitted write failed or that storage cleanup was proved.
struct Watchdog(mpsc::SyncSender<Option<Instant>>);
impl Watchdog {
    fn start() -> Self {
        let (send, receive) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut deadline: Option<Instant> = None;
            loop {
                let update = match deadline {
                    None => match receive.recv() {
                        Ok(value) => value,
                        Err(_) => return,
                    },
                    Some(end) => match receive
                        .recv_timeout(end.saturating_duration_since(Instant::now()))
                    {
                        Ok(value) => value,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            // No diagnostic delivery, stdio flushing or exit
                            // callback may block this unproved termination.
                            unsafe { libc::_exit(70) };
                        }
                    },
                };
                deadline = update;
            }
        });
        Self(send)
    }
    fn arm(&self, timeout: Duration) -> Result<()> {
        self.0
            .send(Some(Instant::now() + timeout))
            .context("arm native deadline")
    }
    fn disarm(&self) -> Result<()> {
        self.0.send(None).context("disarm native deadline")
    }
}

fn connection(
    stream: UnixStream,
    storage: &Storage,
    config: &Config,
    stopping: &AtomicBool,
    watchdog: &Watchdog,
    failed_flush: Arc<AtomicBool>,
    pile_identity: (u64, u64),
) -> Result<()> {
    let timeout = Duration::from_millis(config.request_timeout_ms);
    let mut input = BufReader::new(TimedRead {
        stream: stream.try_clone()?,
        deadline: Instant::now() + Duration::from_secs(2),
    });
    let output = Arc::new(Mutex::new(stream));
    let level = Arc::new(mcp::local_logging::Level::default());
    let diagnostic_bytes = Arc::new(Mutex::new(0usize));
    let diagnostic = {
        let output = output.clone();
        let level = level.clone();
        let used = diagnostic_bytes.clone();
        Arc::new(move |message: &str| -> Result<()> {
            if !level.warning_enabled() {
                return Ok(());
            }
            let mut used = used
                .lock()
                .map_err(|_| anyhow::anyhow!("native diagnostic counter poisoned"))?;
            *used = used
                .checked_add(message.len())
                .context("diagnostic overflow")?;
            ensure!(
                *used <= DIAGNOSTIC_BYTES,
                "native diagnostics exceed budget"
            );
            let bytes = serde_json::to_vec(
                &json!({"jsonrpc":"2.0", "method":"notifications/message", "params":{"level":"warning", "logger":"memory", "data":message}}),
            )?;
            write_frame(
                &mut *output
                    .lock()
                    .map_err(|_| anyhow::anyhow!("native output poisoned"))?,
                &bytes,
                Instant::now() + Duration::from_secs(2),
            )
        }) as super::mcp::Diagnostic
    };
    let pressure = {
        let path = config.pile.clone();
        let minimum = config.min_free_bytes;
        Arc::new(move || check_space(&path, minimum)) as super::mcp::WriteAdmission
    };
    let memory =
        super::mcp::Memory::resident(storage.clone(), diagnostic, pressure, failed_flush.clone())?;
    let mut server = Server::with_limits(
        &[&memory],
        Limits {
            request_bytes: FRAME_BYTES,
            response_bytes: FRAME_BYTES,
            json_depth: 64,
        },
    )?
    .with_logging(level);
    // The connection and frame count are finite even when a caller only pings.
    for _ in 0..64 {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        input.get_mut().deadline = Instant::now() + Duration::from_secs(2);
        let Some(bytes) = mcp::read_frame(&mut input, FRAME_BYTES)? else {
            break;
        };
        *diagnostic_bytes
            .lock()
            .map_err(|_| anyhow::anyhow!("native diagnostic counter poisoned"))? = 0;
        watchdog.arm(timeout)?;
        if let Err(error) = check_pile(&config.pile, pile_identity) {
            failed_flush.store(true, Ordering::Release);
            watchdog.disarm()?;
            return Err(error);
        }
        let response = server.dispatch(bytes.into());
        if let Err(error) = check_pile(&config.pile, pile_identity) {
            failed_flush.store(true, Ordering::Release);
            watchdog.disarm()?;
            return Err(error);
        }
        watchdog.disarm()?;
        if let Some(response) = response? {
            write_frame(
                &mut *output
                    .lock()
                    .map_err(|_| anyhow::anyhow!("native output poisoned"))?,
                response.as_bytes(),
                Instant::now() + Duration::from_secs(2),
            )?;
        }
        ensure!(
            !failed_flush.load(Ordering::Acquire),
            "native publication flush failed; outcome unknown"
        );
    }
    Ok(())
}

pub fn run(path: &Path) -> Result<()> {
    let config = Config::load(path)?;
    let metadata = fs::symlink_metadata(&config.pile)?;
    let pile_identity = (metadata.dev(), metadata.ino());
    let storage = config.storage()?;
    let watchdog = Watchdog::start();
    watchdog.arm(Duration::from_millis(config.request_timeout_ms))?;
    let preflight = storage.with_store(|store, signer, runtime| {
        let scope = crate::schemas::memory::DEFAULT_SCOPE_ID;
        storage.open_collection_read(store, scope, signer.verifying_key(), runtime)?;
        storage.open_collection_write(store, scope, signer.verifying_key(), runtime)?;
        check_space(&config.pile, config.min_free_bytes)
    });
    watchdog.disarm()?;
    let result = preflight.and_then(|()| {
        check_pile(&config.pile, pile_identity)?;
        serve(&config, &storage, &watchdog, pile_identity)
    });
    watchdog.arm(Duration::from_millis(config.request_timeout_ms))?;
    // Preserve a primary operation/transport failure alongside close failure.
    let result = storage.finish(result);
    watchdog.disarm()?;
    // Failed close/dispatch keeps the socket as recovery evidence. Only a
    // proved clean drain removes the exact endpoint owned by this process.
    let identity = result?;
    let current = fs::symlink_metadata(&config.socket)?;
    ensure!(
        current.dev() == identity.dev() && current.ino() == identity.ino(),
        "native socket identity changed during shutdown"
    );
    fs::remove_file(&config.socket)?;
    Ok(())
}

fn serve(config: &Config, storage: &Storage, watchdog: &Watchdog, pile_identity: (u64, u64)) -> Result<fs::Metadata> {
    // bind never removes/replaces a pre-existing endpoint.
    let listener = UnixListener::bind(&config.socket)?;
    fs::set_permissions(&config.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    ensure!(
        unsafe { libc::listen(listener.as_raw_fd(), 4) } == 0,
        "set finite native socket backlog"
    );
    let identity = fs::symlink_metadata(&config.socket)?;
    let stopping = Arc::new(AtomicBool::new(false));
    {
        let stopping = stopping.clone();
        std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            // Control is EOF-only. Any byte/error also stops admission.
            let _ = std::io::stdin().read(&mut byte);
            stopping.store(true, Ordering::Release);
        });
    }
    let failed_flush = Arc::new(AtomicBool::new(false));
    let result = (|| -> Result<()> {
        println!("{{\"ready\":true}}");
        std::io::stdout().flush()?;
        while !stopping.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    // Malformed/disconnected clients lose only their connection.
                    // Native publication has completed/flush failed before dispatch
                    // returns, including when response delivery subsequently fails.
                    let connection_result = connection(
                        stream,
                        storage,
                        config,
                        &stopping,
                        watchdog,
                        failed_flush.clone(),
                        pile_identity,
                    );
                    if failed_flush.load(Ordering::Acquire) {
                        connection_result.context("native Memory persistence custody failed; outcome unknown")?;
                        anyhow::bail!("native Memory persistence custody failed; outcome unknown");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();
    drop(listener);
    result.map(|()| identity)
}

#[cfg(test)]
#[path = "resident_process_tests.rs"]
mod process_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disk_pressure_is_admission_not_a_quota() {
        let root = tempfile::tempdir().unwrap();
        assert!(check_space(root.path(), u64::MAX)
            .unwrap_err()
            .to_string()
            .contains("disk pressure"));
        assert!(check_space(root.path(), 1).is_ok());
    }

    #[test]
    fn replaced_pile_path_cannot_silently_retarget_a_retained_view() {
        let root = tempfile::tempdir().unwrap();
        let pile = root.path().join("synthetic.pile");
        fs::write(&pile, b"first").unwrap();
        let metadata = fs::metadata(&pile).unwrap();
        let identity = (metadata.dev(), metadata.ino());
        check_pile(&pile, identity).unwrap();
        fs::rename(&pile, root.path().join("retained-old")).unwrap();
        fs::write(&pile, b"replacement").unwrap();
        assert!(check_pile(&pile, identity).unwrap_err().to_string().contains("replaced"));
    }
}
