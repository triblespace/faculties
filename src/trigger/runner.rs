//! Bounded execution of one check. Scheduling, persistence and presentation
//! receipts belong to callers; executing a check does not acknowledge its result.

use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Context {
    Timer,
    AdvisoryEvent,
    SynchronousEvent,
}

impl Context {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timer => "timer",
            Self::AdvisoryEvent => "advisory-event",
            Self::SynchronousEvent => "synchronous-event",
        }
    }
}

pub struct Invocation<'a> {
    pub command: &'a str,
    pub directory: &'a Path,
    pub stdin: &'a [u8],
    pub context: Context,
    pub timeout: Duration,
    /// Maximum combined captured stdout and stderr bytes. Exceeding this
    /// budget fails the check rather than silently truncating a successful run.
    pub output_limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Status {
    Success,
    Exit(i32),
    Signal(i32),
    TimedOut,
    OutputLimit,
    SpawnFailed(String),
    IoFailed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub context: Context,
    /// Byte-exact captured streams; an OutputLimit outcome retains prefixes
    /// whose combined length is at most the requested limit.
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Status,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Presentation<'a> {
    Quiet,
    Message(&'a [u8]),
    Failed(&'a Status),
}

impl Outcome {
    pub fn presentation(&self) -> Presentation<'_> {
        match &self.status {
            Status::Success if self.stdout.is_empty() => Presentation::Quiet,
            Status::Success => Presentation::Message(&self.stdout),
            status => Presentation::Failed(status),
        }
    }

    /// Only synchronous event callers receive a blocking verdict. Preserve a
    /// shell's nonzero exit; failures without an exit code fail closed with 1.
    /// An advisory event still records/reports failure, but never vetoes its
    /// caller through this interface.
    pub fn synchronous_verdict(&self) -> Option<i32> {
        (self.context == Context::SynchronousEvent).then(|| match self.status {
            Status::Success => 0,
            Status::Exit(code) => code,
            _ => 1,
        })
    }
}

/// Run `sh -c` with explicit stdin and cwd, and expose the invocation context
/// as FACULTIES_TRIGGER_CONTEXT. Only a successful, byte-empty stdout is quiet;
/// stderr is retained evidence, never automatically promoted to a message.
pub fn execute(invocation: Invocation<'_>) -> Outcome {
    let mut outcome = Outcome {
        context: invocation.context,
        stdout: Vec::new(),
        stderr: Vec::new(),
        status: Status::Success,
    };
    outcome.status = run(&invocation, &mut outcome.stdout, &mut outcome.stderr);
    outcome
}

#[cfg(unix)]
fn run(invocation: &Invocation<'_>, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) -> Status {
    use std::io::{self, ErrorKind, Read, Write};
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    // Do not reap the group leader while a descendant may still hold one of
    // our pipes. Its unreaped PID reserves the numeric process-group identity
    // until timeout/error cleanup has signalled that group. No reader/writer
    // thread can survive a timeout: all three pipes are nonblocking here.
    struct OwnedChild {
        child: Child,
        reaped: bool,
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if !self.reaped {
                // SAFETY: process_group(0) created this child's own group, and
                // we have not reaped its leader or released its numeric PID.
                unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    fn nonblocking(fd: RawFd) -> io::Result<()> {
        // SAFETY: the caller retains the pipe owning fd throughout these calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    // One bounded read per turn keeps a busy stream from starving the other
    // stream, stdin, or the deadline. One byte beyond the remaining budget is
    // enough to distinguish exact-limit success from excessive output.
    fn drain<R: Read>(
        pipe: &mut Option<R>,
        bytes: &mut Vec<u8>,
        remaining: usize,
    ) -> io::Result<bool> {
        let Some(reader) = pipe.as_mut() else {
            return Ok(false);
        };
        let mut buffer = [0; 8192];
        let capacity = buffer.len().min(remaining.saturating_add(1));
        match reader.read(&mut buffer[..capacity]) {
            Ok(0) => {
                pipe.take();
                Ok(false)
            }
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count.min(remaining)]);
                Ok(count > remaining)
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    if invocation.timeout.is_zero() {
        return Status::TimedOut;
    }
    let Some(deadline) = Instant::now().checked_add(invocation.timeout) else {
        return Status::IoFailed("check timeout is out of range".into());
    };
    let child = match Command::new("sh")
        .arg("-c")
        .arg(invocation.command)
        .current_dir(invocation.directory)
        .env("FACULTIES_TRIGGER_CONTEXT", invocation.context.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return Status::SpawnFailed(error.to_string()),
    };
    let mut owned = OwnedChild {
        child,
        reaped: false,
    };
    let mut input = owned.child.stdin.take();
    let mut output = owned.child.stdout.take();
    let mut diagnostic = owned.child.stderr.take();
    for fd in [
        input.as_ref().map(AsRawFd::as_raw_fd),
        output.as_ref().map(AsRawFd::as_raw_fd),
        diagnostic.as_ref().map(AsRawFd::as_raw_fd),
    ]
    .into_iter()
    .flatten()
    {
        if let Err(error) = nonblocking(fd) {
            return Status::IoFailed(format!("make check pipe nonblocking: {error}"));
        }
    }
    let mut written = 0;
    loop {
        if Instant::now() >= deadline {
            return Status::TimedOut;
        }
        let remaining = invocation
            .output_limit
            .saturating_sub(stdout.len() + stderr.len());
        match drain(&mut output, stdout, remaining) {
            Ok(true) => return Status::OutputLimit,
            Ok(false) => {}
            Err(error) => return Status::IoFailed(format!("read check stdout: {error}")),
        }
        let remaining = invocation
            .output_limit
            .saturating_sub(stdout.len() + stderr.len());
        match drain(&mut diagnostic, stderr, remaining) {
            Ok(true) => return Status::OutputLimit,
            Ok(false) => {}
            Err(error) => return Status::IoFailed(format!("read check stderr: {error}")),
        }
        if written == invocation.stdin.len() {
            input.take(); // EOF is explicit, including for an empty input.
        }
        if let Some(writer) = input.as_mut() {
            match writer.write(&invocation.stdin[written..]) {
                Ok(0) => return Status::IoFailed("check stdin made no progress".into()),
                Ok(count) => written += count,
                // A check is allowed not to consume its input. Its own exit
                // status remains authoritative; EPIPE is not its verdict.
                Err(error) if error.kind() == ErrorKind::BrokenPipe => {
                    input.take();
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(error) => return Status::IoFailed(format!("write check stdin: {error}")),
            }
        }
        if output.is_none() && diagnostic.is_none() && input.is_none() {
            match owned.child.try_wait() {
                Ok(Some(status)) => {
                    owned.reaped = true;
                    return match (status.code(), status.signal()) {
                        (Some(0), _) => Status::Success,
                        (Some(code), _) => Status::Exit(code),
                        (_, Some(signal)) => Status::Signal(signal),
                        _ => Status::IoFailed("check exited without a code or signal".into()),
                    };
                }
                Ok(None) => {}
                Err(error) => return Status::IoFailed(format!("wait for check: {error}")),
            }
        }

        let mut poll_fds = [
            libc::pollfd {
                fd: input.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLOUT,
                revents: 0,
            },
            libc::pollfd {
                fd: output.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: diagnostic.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Bound exit observation latency even after the child closes its pipes.
        let wait_ms = remaining.as_millis().min(20) as i32;
        // SAFETY: poll_fds is live mutable storage for exactly three pollfd
        // entries; every nonnegative fd is owned by one of the retained pipes.
        let polled = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, wait_ms) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != ErrorKind::Interrupted {
                return Status::IoFailed(format!("poll check pipes: {error}"));
            }
        }
    }
}

#[cfg(not(unix))]
fn run(_: &Invocation<'_>, _: &mut Vec<u8>, _: &mut Vec<u8>) -> Status {
    Status::SpawnFailed("check execution currently requires a Unix host".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn check(command: &str, directory: &Path, context: Context) -> Outcome {
        execute(Invocation {
            command,
            directory,
            stdin: b"",
            context,
            timeout: Duration::from_secs(3),
            output_limit: 1024 * 1024,
        })
    }

    #[test]
    fn captures_byte_exact_streams_in_the_requested_directory() {
        let directory = tempfile::tempdir().unwrap();
        let result = check(
            "printf 'A\\000B\\377'; printf 'warning\\000' >&2; printf here > location",
            directory.path(),
            Context::Timer,
        );
        assert_eq!(result.status, Status::Success);
        assert_eq!(result.stdout, b"A\0B\xff");
        assert_eq!(result.stderr, b"warning\0");
        assert_eq!(result.presentation(), Presentation::Message(b"A\0B\xff"));
        assert_eq!(
            std::fs::read(directory.path().join("location")).unwrap(),
            b"here"
        );
    }

    #[test]
    fn successful_empty_stdout_is_quiet_even_with_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let result = check("printf diagnostic >&2", directory.path(), Context::Timer);
        assert_eq!(result.status, Status::Success);
        assert_eq!(result.stderr, b"diagnostic");
        assert_eq!(result.presentation(), Presentation::Quiet);
        let whitespace = check("printf '\\n'", directory.path(), Context::Timer);
        assert_eq!(whitespace.presentation(), Presentation::Message(b"\n"));
    }

    #[test]
    fn exit_one_is_failure_in_every_context_and_only_sync_returns_a_verdict() {
        let directory = tempfile::tempdir().unwrap();
        for context in [
            Context::Timer,
            Context::AdvisoryEvent,
            Context::SynchronousEvent,
        ] {
            let result = check("printf rejected; exit 1", directory.path(), context);
            assert_eq!(result.status, Status::Exit(1));
            assert_eq!(
                result.presentation(),
                Presentation::Failed(&Status::Exit(1))
            );
            assert_eq!(
                result.synchronous_verdict(),
                (context == Context::SynchronousEvent).then_some(1)
            );
            let success = check("true", directory.path(), context);
            assert_eq!(
                success.synchronous_verdict(),
                (context == Context::SynchronousEvent).then_some(0)
            );
            let exposed = check(
                "printf %s \"$FACULTIES_TRIGGER_CONTEXT\"",
                directory.path(),
                context,
            );
            assert_eq!(exposed.stdout, context.as_str().as_bytes());
        }
        let denied = check("exit 7", directory.path(), Context::SynchronousEvent);
        assert_eq!(denied.synchronous_verdict(), Some(7));
    }

    #[test]
    fn simultaneous_stream_drain_and_stdin_delivery_do_not_deadlock() {
        let directory = tempfile::tempdir().unwrap();
        let input = vec![b'x'; 256 * 1024];
        let result = execute(Invocation {
            command: "i=0; while [ $i -lt 5000 ]; do printf a; printf b >&2; i=$((i+1)); done; cat",
            directory: directory.path(),
            stdin: &input,
            context: Context::Timer,
            timeout: Duration::from_secs(5),
            output_limit: input.len() + 10_000,
        });
        assert_eq!(result.status, Status::Success);
        assert_eq!(&result.stdout[..5000], vec![b'a'; 5000]);
        assert_eq!(&result.stdout[5000..], input);
        assert_eq!(result.stderr, vec![b'b'; 5000]);
    }

    #[test]
    fn output_limit_is_combined_and_exact_limit_success_is_not_truncated() {
        let directory = tempfile::tempdir().unwrap();
        for (command, limit, expected) in [
            ("printf 12345; printf 67890 >&2", 6, Status::OutputLimit),
            ("printf 12345", 5, Status::Success),
            ("printf x", 0, Status::OutputLimit),
            ("true", 0, Status::Success),
        ] {
            let result = execute(Invocation {
                command,
                directory: directory.path(),
                stdin: b"",
                context: Context::Timer,
                timeout: Duration::from_secs(3),
                output_limit: limit,
            });
            assert_eq!(result.status, expected);
            assert!(result.stdout.len() + result.stderr.len() <= limit);
        }
    }

    #[test]
    fn timeout_bounds_blocked_stdin_closed_streams_and_descendant_held_pipes() {
        let directory = tempfile::tempdir().unwrap();
        for command in [
            "printf %s $$ > pid; exec sleep 20",
            "printf %s $$ > pid; exec 1>&- 2>&-; exec sleep 20",
            "printf %s $$ > pid; sleep 20 & printf ready; exit 0",
        ] {
            let started = Instant::now();
            let result = execute(Invocation {
                command,
                directory: directory.path(),
                stdin: &vec![b'x'; 256 * 1024],
                context: Context::SynchronousEvent,
                timeout: Duration::from_millis(300),
                output_limit: 1024,
            });
            assert_eq!(result.status, Status::TimedOut);
            assert_eq!(result.synchronous_verdict(), Some(1));
            assert!(started.elapsed() < Duration::from_secs(3));
            let pid: i32 = std::fs::read_to_string(directory.path().join("pid"))
                .unwrap()
                .parse()
                .unwrap();
            // The direct child is ours to reap; descendants are killed with
            // its group and reaped by their parent or the host's child reaper.
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }

    #[test]
    fn signals_and_spawn_errors_remain_explicit() {
        let directory = tempfile::tempdir().unwrap();
        let signalled = check("kill -TERM $$", directory.path(), Context::Timer);
        assert_eq!(signalled.status, Status::Signal(libc::SIGTERM));
        let absent = check("true", &directory.path().join("missing"), Context::Timer);
        assert!(matches!(absent.status, Status::SpawnFailed(_)));
        assert!(matches!(absent.presentation(), Presentation::Failed(_)));
    }
}
