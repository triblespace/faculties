//! The follower: one short-lived process per open alarm that runs the rest
//! of its ladder, and exits when the alarm is acked or resolved, or when the
//! ladder is over.
//!
//! It is not a daemon. `alarm raise` starts it outside the agent harness
//! (`systemd-run --user`, [`SystemdRun`]; a detached child on a Mac,
//! [`Detached`]) right after R0. Each turn it reads the alarm and the
//! Discord facts tolerantly, records an ack JP left on Discord and stops,
//! or runs what [`next_action`] says is due, or sleeps until the next
//! occurrence (at most [`POLL`]), pulling the alarm's Discord channel every
//! [`PULL_EVERY`] so an ack does not wait for intake.
//!
//! One follower per alarm and host: an exclusive flock on a lock file in the
//! runtime directory, held for the follower's life ([`Lock`]). `alarm follow
//! --resume` adopts every open, real alarm whose newest activity is older
//! than [`STALE_AFTER`] and whose lock no process holds ([`resume`]): a
//! follower that died with its host. Across hosts nothing serialises
//! followers: two hosts adopting the same alarm both run its whole ladder,
//! sound, voice and lights included, not only the pings.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use triblespace::prelude::*;

use super::ack::Detected;
use super::ladder::{next_action, Due, Next, SOUND_REPEAT};
use super::operations::{Alarm, Clock, FollowState, StepReport};
use super::rungs::{RungRunner, Settings, Subject};
use super::StepRow;
use super::SECOND;

/// The longest the follower sleeps before it reads the pile again.
pub const POLL: i128 = 5 * SECOND;
/// How often the follower pulls the alarm's Discord channel.
pub const PULL_EVERY: i128 = 20 * SECOND;
/// An open alarm with no step this recent has lost its follower.
pub const STALE_AFTER: i128 = 2 * SOUND_REPEAT;
/// The longest a follower runs, whatever happens: above the 60-minute
/// ladder, so it only ends a follower that is stuck.
pub const WALL_CAP: i128 = 2 * 60 * 60 * SECOND;
/// How long a follower waits on incomplete observations before it goes on
/// escalating from the last complete one. For an alarm, silence is the
/// dangerous failure and a repeated rung only noise.
pub const GRACE: i128 = 2 * 60 * SECOND;
/// The transient unit's own backstop, above [`WALL_CAP`].
pub const UNIT_RUNTIME_MAX: &str = "RuntimeMaxSec=7800";

/// Why a follower stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It found and recorded JP's ack on Discord.
    Acked { via: Id },
    /// Something closed the alarm: an ack or a resolve.
    Closed,
    /// The ladder is over (a test after R0, or the final notice sent).
    Finished,
    /// The follower ran for [`WALL_CAP`] without the alarm closing or its
    /// ladder ending (for instance, it could not observe it completely).
    TimedOut,
}

/// What the follower reads and writes: [`Alarm`] over a pile, or a stand-in
/// under test.
pub trait Ledger {
    fn subject(&self, alarm: Id) -> Result<Subject>;
    fn state(&self, alarm: Id, jp_user: Option<&str>) -> Result<FollowState>;
    fn record_detected(&self, alarm: Id, detected: Detected) -> Result<Option<Id>>;
    /// Record a note on `alarm` by `by` at `at`.
    fn note(&self, alarm: Id, by: Id, text: &str, at: i128) -> Result<Id>;
    fn run_rungs(
        &self,
        subject: &Subject,
        due: &[Due],
        runner: &mut dyn RungRunner,
        clock: &mut dyn Clock,
    ) -> Result<Vec<StepReport>>;
}

impl Ledger for Alarm {
    fn subject(&self, alarm: Id) -> Result<Subject> {
        Alarm::subject(self, alarm)
    }

    fn state(&self, alarm: Id, jp_user: Option<&str>) -> Result<FollowState> {
        Alarm::state(self, alarm, jp_user)
    }

    fn record_detected(&self, alarm: Id, detected: Detected) -> Result<Option<Id>> {
        Alarm::record_detected(self, alarm, detected)
    }

    fn note(&self, alarm: Id, by: Id, text: &str, at: i128) -> Result<Id> {
        Alarm::note_as(self, alarm, by, text, at)
    }

    fn run_rungs(
        &self,
        subject: &Subject,
        due: &[Due],
        runner: &mut dyn RungRunner,
        clock: &mut dyn Clock,
    ) -> Result<Vec<StepReport>> {
        Alarm::run_rungs(self, subject, due, runner, clock)
    }
}

/// Run `alarm`'s ladder to its end. `pull` refreshes the Discord facts (the
/// alarm's channel); its failures are reported and the ladder goes on.
///
/// Every observation is tolerant. An ack or resolve it can see, in any
/// observation, complete or not, ends the follower at once; it ends as
/// [`Ended::Closed`] only on such a record, never because the raise is
/// missing. While observations are incomplete (unread commits, or the raise
/// not visible) it pulls Discord and reads again every [`POLL`], running no
/// rung for up to [`GRACE`]; after that it escalates from the last complete
/// observation it saw, with the alarm's own raise time and test flag,
/// remembering every rung it runs at the start its step recorded, so each
/// due occurrence fires once, and notes once that it does so. A follower that
/// never had a complete observation does not act. It stops after
/// [`WALL_CAP`] as [`Ended::TimedOut`].
pub fn follow(
    operations: &dyn Ledger,
    alarm: Id,
    runner: &mut dyn RungRunner,
    clock: &mut dyn Clock,
    jp_user: Option<&str>,
    pull: &mut dyn FnMut() -> Result<()>,
) -> Result<Ended> {
    let subject = operations.subject(alarm)?;
    // Probed once, when the first rung could be due: a follower started on
    // an alarm that is already closed touches nothing.
    let mut capabilities = None;
    let mut pulled: Option<i128> = None;
    let started = clock.now()?;
    // The last complete observation (when, and its steps), with every rung
    // this follower ran since appended, so a rung it attempted is never
    // replayed from the same stale observation.
    let mut basis: Option<(i128, Vec<StepRow>)> = None;
    let mut incomplete_since: Option<i128> = None;
    let mut noted = false;
    loop {
        let now = clock.now()?;
        if now - started >= WALL_CAP {
            return Ok(Ended::TimedOut);
        }
        let state = operations.state(alarm, jp_user)?;
        // Positive evidence wins in any observation, complete or not.
        if state.closed {
            return Ok(Ended::Closed);
        }
        if let Some(detected) = state.detected {
            operations.record_detected(alarm, detected)?;
            return Ok(Ended::Acked { via: detected.via });
        }
        let complete = state.unread == 0 && state.raised;
        if complete {
            basis = Some((now, state.steps.clone()));
            incomplete_since = None;
            noted = false;
        } else {
            let since = *incomplete_since.get_or_insert(now);
            let acting = match &basis {
                None => {
                    eprintln!(
                        "alarm follow: {} alarm commit(s) unread and no complete observation \
                         of this alarm yet; it cannot act without knowing its raise",
                        state.unread
                    );
                    false
                }
                Some(_) if now - since < GRACE => {
                    eprintln!(
                        "alarm follow: {} alarm commit(s) unread; no rung this turn (grace)",
                        state.unread
                    );
                    false
                }
                Some((observed, _)) => {
                    if !noted {
                        let text = format!(
                            "the follower acts on the observation from {} with {} commit(s) \
                             unread: it could not read the alarm completely for {} s",
                            super::render::utc(*observed),
                            state.unread,
                            (now - since) / SECOND
                        );
                        if let Err(error) = operations.note(alarm, subject.raiser, &text, now) {
                            eprintln!("alarm follow: could not record that note: {error:#}");
                        }
                        noted = true;
                    }
                    true
                }
            };
            if !acting {
                wait(now, now + POLL, jp_user, &mut pulled, pull, clock)?;
                continue;
            }
        }
        let steps = basis
            .as_ref()
            .expect("a complete observation was seen")
            .1
            .clone();
        let capabilities = capabilities.get_or_insert_with(|| runner.capabilities(&subject));
        capabilities.jp_in_voice = state.jp_in_voice;
        // The alarm's own identity throughout: its raise time and whether it
        // is a test come from the raise, never from a later observation.
        match next_action(
            subject.raised_at,
            now,
            &steps,
            true,
            subject.test,
            capabilities,
        ) {
            Next::Run(due) => {
                let reports = operations.run_rungs(&subject, &due, runner, clock)?;
                if let Some((_, steps)) = &mut basis {
                    steps.extend(reports.iter().map(|report| StepRow {
                        step: report.step,
                        rung: report.rung,
                        // The start the committed step records, never the
                        // turn's time: coverage compares exactly this.
                        started: report.started,
                        delivered: report.outcome == super::Outcome::Delivered,
                        failed: report.outcome != super::Outcome::Delivered,
                    }));
                }
            }
            Next::Stop => return Ok(Ended::Finished),
            Next::Wait(at) => wait(now, at.min(now + POLL), jp_user, &mut pulled, pull, clock)?,
        }
    }
}

/// Pull the Discord channel when due, then sleep until `until`.
fn wait(
    now: i128,
    until: i128,
    jp_user: Option<&str>,
    pulled: &mut Option<i128>,
    pull: &mut dyn FnMut() -> Result<()>,
    clock: &mut dyn Clock,
) -> Result<()> {
    if jp_user.is_some() && pulled.is_none_or(|last| now - last >= PULL_EVERY) {
        if let Err(error) = pull() {
            eprintln!("alarm follow: Discord pull failed, going on: {error:#}");
        }
        *pulled = Some(now);
    }
    clock.sleep_until(until)
}

/// The lock that keeps one follower per alarm on a host: an exclusive
/// `flock` on `alarm-<id>.lock`, held for the follower's whole life by the
/// open file this value owns, and released by the kernel when the process
/// ends, however it ends. Liveness is exactly "the lock is held": a holder
/// that died leaves a free lock, with no staleness rule and no pid check.
/// The file is never unlinked (unlinking a locked path is what lets two
/// processes each lock a different inode of "the same" lock). The pid is
/// written into it for people to read, and never used to decide anything.
#[derive(Debug)]
pub struct Lock {
    /// The open file whose flock is the lock; dropping it releases it.
    _held: std::fs::File,
}

impl Lock {
    pub fn path(directory: &Path, alarm: Id) -> PathBuf {
        directory.join(format!("alarm-{alarm:x}.lock"))
    }

    fn open(path: &Path) -> Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open {}", path.display()))
    }

    /// Take the lock without waiting: `Ok(false)` when another holds it.
    fn try_lock(file: &std::fs::File, path: &Path) -> Result<bool> {
        use std::os::unix::io::AsRawFd;
        // SAFETY: flock on a descriptor `file` owns for as long as it lives.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(false)
        } else {
            Err(error).with_context(|| format!("lock {}", path.display()))
        }
    }

    /// Whether a live process holds `alarm`'s lock on this host. Taken and
    /// released at once when it is free.
    pub fn held(directory: &Path, alarm: Id) -> Result<bool> {
        let path = Self::path(directory, alarm);
        let file = Self::open(&path)?;
        Ok(!Self::try_lock(&file, &path)?)
    }

    /// The pid the holder wrote, for display only.
    pub fn recorded_pid(directory: &Path, alarm: Id) -> Option<u32> {
        std::fs::read_to_string(Self::path(directory, alarm))
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
    }

    pub fn acquire(directory: &Path, alarm: Id) -> Result<Self> {
        let path = Self::path(directory, alarm);
        let file = Self::open(&path)?;
        if !Self::try_lock(&file, &path)? {
            let shown = Self::recorded_pid(directory, alarm)
                .map(|pid| format!(" (it wrote pid {pid})"))
                .unwrap_or_default();
            bail!(
                "a follower already runs alarm {alarm:x} on this host: {} is locked{shown}",
                path.display()
            );
        }
        {
            use std::io::{Seek, Write};
            let mut writer = &file;
            file.set_len(0)
                .and_then(|()| writer.seek(std::io::SeekFrom::Start(0)).map(drop))
                .and_then(|()| writeln!(writer, "{}", std::process::id()))
                .with_context(|| format!("write {}", path.display()))?;
        }
        Ok(Self { _held: file })
    }
}

/// The runtime directory lock files live in.
pub fn runtime_dir() -> Result<PathBuf> {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(directory) if !directory.is_empty() => Ok(PathBuf::from(directory)),
        _ => Ok(std::env::temp_dir()),
    }
}

/// How a follower for one alarm is started: a program with arguments and
/// an explicit environment, every path absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowerCommand {
    /// The transient unit's name: `alarm-<id8>`.
    pub unit: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl FollowerCommand {
    /// `systemd-run --user` arguments for this follower, with its own
    /// memory ceiling and an OOM score that makes it the one to go first.
    pub fn systemd_run_args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "--user",
            "--unit",
            self.unit.as_str(),
            "--collect",
            "-p",
            "MemoryMax=1G",
            "-p",
            "OOMScoreAdjust=500",
            "-p",
            UNIT_RUNTIME_MAX,
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        for (name, value) in &self.env {
            args.push("-E".to_owned());
            args.push(format!("{name}={value}"));
        }
        args.push("--".to_owned());
        args.push(self.program.display().to_string());
        args.extend(self.args.iter().cloned());
        args
    }
}

/// The environment a follower is given: the settings as `ALARM_*`
/// variables, and the collection routes and peers of `ambient` (a transient
/// unit gets the user manager's environment, not the shell's, so the routes
/// must be passed). Nothing else is passed: no key path (the key is an
/// argument), no persona (the follower acts as nobody), and no workspace
/// selector: the workspace the raise used travels as an explicit flag
/// ([`command_for`]), never as whatever the environment happens to name.
pub fn follower_env(
    settings: &Settings,
    ambient: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ambient
        .into_iter()
        .filter(|(name, _)| {
            name.starts_with(crate::collection_names::COLLECTION_OVERRIDE_PREFIX)
                || name == "TRIBLESPACE_PEERS"
        })
        .collect();
    let mut setting = |name: &str, value: &Option<String>| {
        if let Some(value) = value {
            env.push((name.to_owned(), value.clone()));
        }
    };
    setting("ALARM_JP_DISCORD_USER", &settings.jp_discord_user);
    setting("ALARM_DISCORD_CHANNEL", &settings.discord_channel);
    setting("ALARM_TEST_DISCORD_CHANNEL", &settings.test_channel);
    setting("ALARM_VOICE_CHANNEL", &settings.voice_channel);
    setting("ALARM_VOICE_GUILD", &settings.voice_guild);
    setting("ALARM_COLONY_GROUP", &settings.colony_group);
    setting("ALARM_MAC_SSH", &settings.mac_ssh);
    setting("ALARM_HUE_TARGET", &settings.hue_target);
    env.sort();
    env
}

/// The command that follows `alarm`, refusing any path that is not absolute.
pub fn follower_command(
    program: &Path,
    pile: &Path,
    key: &Path,
    alarm: Id,
    env: Vec<(String, String)>,
) -> Result<FollowerCommand> {
    for (what, path) in [("program", program), ("pile", pile), ("key", key)] {
        if !path.is_absolute() {
            bail!(
                "the follower's {what} path {} is not absolute",
                path.display()
            );
        }
    }
    Ok(FollowerCommand {
        unit: format!("alarm-{}", &format!("{alarm:x}")[..8]),
        program: program.to_owned(),
        args: vec![
            "--pile".to_owned(),
            pile.display().to_string(),
            "--key".to_owned(),
            key.display().to_string(),
            "follow".to_owned(),
            format!("{alarm:x}"),
        ],
        env,
    })
}

/// How the follower reaches the same collections the raise did, from the
/// raise's own storage, never from ambient selection:
/// - a selected workspace: `--workspace <its exact revision>`;
/// - explicit routes without a workspace (an MCP launcher's routed
///   storage): the whole map as `--route NAME=DESCRIPTOR` flags, which the
///   follower opens with the strict explicit-routes constructor (a role not
///   in the map is an error there, not a fallback);
/// - neither (legacy CLI routing, which reads the collection variables
///   itself): the ambient collection variables, as the raise read them.
/// With either explicit selector no collection variable is passed, and the
/// follower ignores any its unit inherits ([`super::cli::select`]).
/// The extra follower arguments and the environment, in that order.
pub fn routing(
    storage: &crate::storage::Storage,
    settings: &Settings,
    ambient: &[(String, String)],
) -> Result<(Vec<String>, Vec<(String, String)>)> {
    let not_routes = |(name, _): &(String, String)| {
        !name.starts_with(crate::collection_names::COLLECTION_OVERRIDE_PREFIX)
    };
    match (storage.selected_workspace(), storage.collection_routes()) {
        (Some(workspace), _) => Ok((
            vec!["--workspace".to_owned(), format!("{:X}", workspace.id)],
            follower_env(
                settings,
                ambient.iter().filter(|var| not_routes(var)).cloned(),
            ),
        )),
        (None, Some(routes)) => {
            let mut args = Vec::new();
            for (scope, handle) in routes {
                let Some(name) = crate::collection_names::name_for(*scope) else {
                    bail!("route for scope {scope:x} has no collection name to pass on");
                };
                args.push("--route".to_owned());
                args.push(format!("{name}={}", hex::encode(handle.raw)));
            }
            Ok((
                args,
                follower_env(
                    settings,
                    ambient.iter().filter(|var| not_routes(var)).cloned(),
                ),
            ))
        }
        (None, None) => Ok((Vec::new(), follower_env(settings, ambient.iter().cloned()))),
    }
}

/// The command that follows `alarm` with `program`, on the raise's pile,
/// key and routing ([`routing`]).
pub fn command_for(
    program: &Path,
    storage: &crate::storage::Storage,
    settings: &Settings,
    ambient: &[(String, String)],
    alarm: Id,
) -> Result<FollowerCommand> {
    let pile = std::fs::canonicalize(storage.path())
        .with_context(|| format!("resolve the pile {}", storage.path().display()))?;
    let key = crate::storage::signer_path(&pile, storage.key_path());
    let key = std::fs::canonicalize(&key)
        .with_context(|| format!("resolve the signing key {}", key.display()))?;
    let (extra, env) = routing(storage, settings, ambient)?;
    let mut command = follower_command(program, &pile, &key, alarm, env)?;
    // Global arguments go before the subcommand.
    let at = command.args.len() - 2;
    command.args.splice(at..at, extra);
    Ok(command)
}

/// The `alarm` binary a follower runs: this process when it is `alarm`,
/// else the `alarm` beside it (an MCP server's cohort).
pub fn alarm_program() -> Result<PathBuf> {
    let current = std::env::current_exe().context("find this program")?;
    if current.file_stem().is_some_and(|stem| stem == "alarm") {
        return Ok(current);
    }
    let sibling = current
        .parent()
        .ok_or_else(|| anyhow!("{} has no directory", current.display()))?
        .join("alarm");
    if sibling.is_file() {
        Ok(sibling)
    } else {
        bail!("no alarm program beside {}", current.display())
    }
}

/// Starts a follower.
pub trait Spawner {
    fn spawn(&mut self, command: &FollowerCommand) -> Result<()>;
}

/// A transient user unit, outside the agent harness that started the raise.
pub struct SystemdRun;

impl Spawner for SystemdRun {
    fn spawn(&mut self, command: &FollowerCommand) -> Result<()> {
        super::rungs::run_with_timeout(
            "systemd-run",
            &command.systemd_run_args(),
            std::time::Duration::from_secs(30),
        )
        .map(drop)
        .with_context(|| format!("start the follower unit {}", command.unit))
    }
}

/// A detached child in its own process group, its output in `log`: the
/// follower where there is no user systemd (a Mac).
pub struct Detached {
    pub log: PathBuf,
}

impl Spawner for Detached {
    fn spawn(&mut self, command: &FollowerCommand) -> Result<()> {
        use std::os::unix::process::CommandExt;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .with_context(|| format!("open {}", self.log.display()))?;
        std::process::Command::new(&command.program)
            .args(&command.args)
            .env_clear()
            .envs(command.env.iter().cloned())
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()
            .with_context(|| format!("start the follower {}", command.program.display()))
            .map(drop)
    }
}

/// The variables of this process a follower must inherit: the collection
/// routes, the peers and the workspace. Read by a CLI or a launcher, never
/// by an MCP adapter.
pub fn ambient_routes() -> Vec<(String, String)> {
    follower_env(&Settings::default(), std::env::vars())
}

/// The spawner for this platform: a transient user unit on Linux, a
/// detached child (logging to `log_directory`) on a Mac.
pub fn default_spawner(log_directory: &Path) -> Box<dyn Spawner> {
    if cfg!(target_os = "macos") {
        Box::new(Detached {
            log: log_directory.join("alarm-follow.log"),
        })
    } else {
        let _ = log_directory;
        Box::new(SystemdRun)
    }
}

/// Start the follower of `alarm` on this pile through `spawner`, with the
/// settings and `routes` it needs; the unit (or child) name.
pub fn start(
    storage: &crate::storage::Storage,
    settings: &Settings,
    routes: &[(String, String)],
    alarm: Id,
    spawner: &mut dyn Spawner,
) -> Result<String> {
    let command = command_for(&alarm_program()?, storage, settings, routes, alarm)?;
    spawner.spawn(&command)?;
    Ok(command.unit)
}

/// Adopt every open, real alarm whose newest activity is older than
/// [`STALE_AFTER`], whose ladder is not over, and whose lock in
/// `lock_directory` no live process holds: start a follower for each
/// through `spawner`, built by `command`. The adopted alarms.
pub fn resume(
    operations: &Alarm,
    clock: &mut dyn Clock,
    lock_directory: &Path,
    spawner: &mut dyn Spawner,
    command: &dyn Fn(Id) -> Result<FollowerCommand>,
) -> Result<Vec<Id>> {
    let now = clock.now()?;
    let mut adopted = Vec::new();
    for orphan in operations.orphans(now, STALE_AFTER, clock)? {
        if Lock::held(lock_directory, orphan)? {
            continue;
        }
        spawner.spawn(&command(orphan)?)?;
        adopted.push(orphan);
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests;
