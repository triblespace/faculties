//! The pile as a file, for a process that outlives one command: the identity
//! it observed when it bound, and waiting until the file there changes.
//!
//! A compaction writes a new file and renames it over the path. A process
//! that mapped the old file keeps that inode alive for as long as it runs,
//! so a long-lived process must notice the rename and end, and start again
//! from the new file. A shortened file is a different matter: it is the one
//! change an append-only pile never makes, and it is an error.

use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

/// An observed backing file identity: diagnostic evidence, not proof that
/// another process cannot change mapped pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackingIdentity {
    device: u64,
    inode: u64,
    bytes: u64,
}

impl BackingIdentity {
    /// The file at `path` as it stands now.
    pub fn of(path: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        anyhow::ensure!(meta.is_file(), "the pile must be an ordinary file");
        Ok(Self {
            device: meta.dev(),
            inode: meta.ino(),
            bytes: meta.len(),
        })
    }

    /// The length of the file at `path` if it is still this file, `None` if
    /// another file has replaced it. Appends are expected (the working pile
    /// grows); a shortened file is an error.
    pub fn observe(&self, path: &Path) -> Result<Option<u64>> {
        let now = Self::of(path)?;
        if (now.device, now.inode) != (self.device, self.inode) {
            return Ok(None);
        }
        anyhow::ensure!(
            now.bytes >= self.bytes,
            "the pile {} shrank under a bound process ({} -> {} bytes)",
            path.display(),
            self.bytes,
            now.bytes
        );
        Ok(Some(now.bytes))
    }

    /// The pile at `path` is still this file, at least as long as it was.
    pub fn require_still(&self, path: &Path) -> Result<()> {
        anyhow::ensure!(
            self.observe(path)?.is_some(),
            "the pile {} was replaced under the bound process; refusing further use",
            path.display()
        );
        Ok(())
    }
}

/// Run `pass` now, and again whenever the pile at `path` has grown since the
/// last pass began, looking every `poll`. Returns once another file has
/// replaced the one `bound` observed; an error from `pass` or a shortened
/// file ends it with that error.
///
/// The length is taken before a pass, so a pass's own writes cause one more
/// pass, which finds nothing to do, and a write that lands during a pass is
/// never missed.
pub fn run_until_replaced(
    path: &Path,
    bound: &BackingIdentity,
    poll: Duration,
    mut pass: impl FnMut() -> Result<()>,
) -> Result<()> {
    run_until_replaced_or_due(path, bound, poll, || pass().map(|()| None))
}

/// [`run_until_replaced`] for a pass that may also name when it is next due
/// whatever the pile does (a retry it deferred): the next pass runs when the
/// pile has grown or that moment has come, whichever is first. The wait
/// sleeps until the earlier of the next look and the deadline, so a deadline
/// costs no polling. A deadline already past when the pass returns (it came
/// while the pass was finishing) runs the next pass at once: it is pending
/// work, not a stale one. Not spinning is the pass's part: it must name a
/// past deadline only for work it will then do, and push the next one
/// forward when it does.
pub fn run_until_replaced_or_due(
    path: &Path,
    bound: &BackingIdentity,
    poll: Duration,
    mut pass: impl FnMut() -> Result<Option<Instant>>,
) -> Result<()> {
    loop {
        let Some(seen) = bound.observe(path)? else {
            return Ok(());
        };
        let due = pass()?;
        loop {
            let wait = due.map_or(poll, |due| {
                poll.min(due.saturating_duration_since(Instant::now()))
            });
            std::thread::sleep(wait);
            match bound.observe(path)? {
                None => return Ok(()),
                Some(bytes) if bytes != seen => break,
                Some(_) => {}
            }
            if due.is_some_and(|due| Instant::now() >= due) {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn append(path: &Path, bytes: &[u8]) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn a_grown_pile_is_still_bound_and_a_replaced_or_shortened_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("self.pile");
        std::fs::write(&pile, b"prefix").unwrap();
        let bound = BackingIdentity::of(&pile).unwrap();
        append(&pile, b" appended");
        bound.require_still(&pile).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&pile)
            .unwrap()
            .set_len(3)
            .unwrap();
        assert!(bound.require_still(&pile).is_err(), "shortened");
        let other = dir.path().join("compacted.pile");
        std::fs::write(&other, b"prefix appended and more").unwrap();
        std::fs::rename(&other, &pile).unwrap();
        assert!(bound.require_still(&pile).is_err(), "replaced");
    }

    /// A pass that appends causes exactly one more pass; a compaction that
    /// renames a new file over the path ends the loop cleanly.
    #[test]
    fn the_loop_passes_again_after_growth_and_ends_when_the_pile_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("self.pile");
        std::fs::write(&pile, b"prefix").unwrap();
        let bound = BackingIdentity::of(&pile).unwrap();
        let mut passes = 0;
        run_until_replaced(&pile, &bound, Duration::from_millis(1), || {
            passes += 1;
            match passes {
                1 => append(&pile, b" a derived row"),
                2 => {
                    let compacted = dir.path().join("compacted.pile");
                    std::fs::write(&compacted, b"compacted").unwrap();
                    std::fs::rename(&compacted, &pile).unwrap();
                }
                _ => panic!("a pass after the pile was replaced"),
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(passes, 2);
    }

    /// A deadline a pass names runs the next pass when it comes, on a pile
    /// that never grows, even when the look interval is an hour.
    #[test]
    fn a_pass_runs_at_the_deadline_the_last_one_named_on_a_quiet_pile() {
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("self.pile");
        std::fs::write(&pile, b"prefix").unwrap();
        let bound = BackingIdentity::of(&pile).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut passes = 0;
            let result =
                run_until_replaced_or_due(&pile, &bound, Duration::from_secs(3600), || {
                    passes += 1;
                    match passes {
                        1 => Ok(Some(Instant::now() + Duration::from_millis(100))),
                        _ => anyhow::bail!("the second pass"),
                    }
                });
            let _ = done.send((passes, started.elapsed(), result.is_err()));
        });
        let (passes, elapsed, ended) = finished
            .recv_timeout(Duration::from_secs(30))
            .expect("no pass ran at the deadline on a quiet pile");
        assert_eq!((passes, ended), (2, true));
        assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
    }

    /// A deadline that comes while the pass is still returning is not lost:
    /// the next pass runs at once, though the pile never grows and the look
    /// interval is an hour.
    #[test]
    fn a_deadline_that_passes_during_the_callback_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("self.pile");
        std::fs::write(&pile, b"prefix").unwrap();
        let bound = BackingIdentity::of(&pile).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut passes = 0;
            let result =
                run_until_replaced_or_due(&pile, &bound, Duration::from_secs(3600), || {
                    passes += 1;
                    match passes {
                        1 => {
                            let due = Instant::now() + Duration::from_millis(10);
                            // The pass is still logging when its deadline comes.
                            std::thread::sleep(Duration::from_millis(50));
                            Ok(Some(due))
                        }
                        _ => anyhow::bail!("the second pass"),
                    }
                });
            let _ = done.send((passes, result.is_err()));
        });
        let (passes, ended) = finished
            .recv_timeout(Duration::from_secs(30))
            .expect("a deadline that passed during the callback was dropped");
        assert_eq!((passes, ended), (2, true));
    }

    #[test]
    fn the_loop_ends_with_the_error_of_a_pass_or_of_a_shortened_pile() {
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("self.pile");
        std::fs::write(&pile, b"prefix").unwrap();
        let bound = BackingIdentity::of(&pile).unwrap();
        let error = run_until_replaced(&pile, &bound, Duration::from_millis(1), || {
            anyhow::bail!("derive failed")
        })
        .unwrap_err();
        assert!(error.to_string().contains("derive failed"), "{error:#}");
        let error = run_until_replaced(&pile, &bound, Duration::from_millis(1), || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&pile)
                .unwrap()
                .set_len(3)
                .unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("shrank"), "{error:#}");
    }
}
