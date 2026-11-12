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
//! One follower per alarm and host: a lock file in the runtime directory
//! holds its pid ([`Lock`]). `alarm follow --resume` adopts every open, real
//! alarm whose newest activity is older than [`STALE_AFTER`] and whose lock
//! no live process holds ([`resume`]): a follower that died with its host.
//! Across hosts nothing serialises followers; two hosts adopting the same
//! alarm both run its ladder, which repeats a ping and nothing worse.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use triblespace::prelude::*;

use super::ladder::{next_action, Next, SOUND_REPEAT};
use super::operations::{Alarm, Clock};
use super::rungs::{RungRunner, Settings};
use super::SECOND;

/// The longest the follower sleeps before it reads the pile again.
pub const POLL: i128 = 5 * SECOND;
/// How often the follower pulls the alarm's Discord channel.
pub const PULL_EVERY: i128 = 20 * SECOND;
/// An open alarm with no step this recent has lost its follower.
pub const STALE_AFTER: i128 = 2 * SOUND_REPEAT;

/// Why a follower stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It found and recorded JP's ack on Discord.
    Acked { via: Id },
    /// Something closed the alarm: an ack or a resolve.
    Closed,
    /// The ladder is over (a test after R0, or the final notice sent).
    Finished,
}

/// Run `alarm`'s ladder to its end. `pull` refreshes the Discord facts (the
/// alarm's channel); its failures are reported and the ladder goes on.
pub fn follow(
    operations: &Alarm,
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
    loop {
        let now = clock.now()?;
        let state = operations.state(alarm, jp_user)?;
        if !state.open {
            return Ok(Ended::Closed);
        }
        if let Some(detected) = state.detected {
            operations.record_detected(alarm, detected)?;
            return Ok(Ended::Acked { via: detected.via });
        }
        let capabilities = capabilities.get_or_insert_with(|| runner.capabilities(&subject));
        capabilities.jp_in_voice = state.jp_in_voice;
        match next_action(
            subject.raised_at,
            now,
            &state.steps,
            true,
            subject.test,
            capabilities,
        ) {
            Next::Run(due) => {
                operations.run_rungs(&subject, &due, runner, clock)?;
            }
            Next::Stop => return Ok(Ended::Finished),
            Next::Wait(at) => {
                if jp_user.is_some() && pulled.is_none_or(|last| now - last >= PULL_EVERY) {
                    if let Err(error) = pull() {
                        eprintln!("alarm follow: Discord pull failed, going on: {error:#}");
                    }
                    pulled = Some(now);
                }
                clock.sleep_until(at.min(now + POLL))?;
            }
        }
    }
}

/// The lock that keeps one follower per alarm on a host: a file holding the
/// follower's pid, removed when it ends. A lock whose pid is not alive is
/// stale and is taken over.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    pid: u32,
}

impl Lock {
    pub fn path(directory: &Path, alarm: Id) -> PathBuf {
        directory.join(format!("alarm-{alarm:x}.lock"))
    }

    /// The pid holding `alarm`'s lock, when a live process does.
    pub fn holder(directory: &Path, alarm: Id) -> Option<u32> {
        std::fs::read_to_string(Self::path(directory, alarm))
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            .filter(|pid| pid_alive(*pid))
    }

    pub fn acquire(directory: &Path, alarm: Id) -> Result<Self> {
        let path = Self::path(directory, alarm);
        let pid = std::process::id();
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    writeln!(file, "{pid}").with_context(|| format!("write {}", path.display()))?;
                    return Ok(Self { path, pid });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(holder) = Self::holder(directory, alarm) {
                        bail!(
                            "a follower (pid {holder}) already runs alarm {alarm:x} on this \
                             host ({})",
                            path.display()
                        );
                    }
                    // Stale: its process is gone. Take it over.
                    std::fs::remove_file(&path)
                        .with_context(|| format!("remove stale {}", path.display()))?;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("create {}", path.display()))
                }
            }
        }
        bail!("could not take {}", path.display())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let ours = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            == Some(self.pid);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether a process with this pid exists (on this host).
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
/// variables, and the collection routes and workspace of `ambient` (a
/// transient unit gets the user manager's environment, not the shell's, so
/// the routes must be passed). Nothing else is passed: no key path (the key
/// is an argument) and no persona (the follower acts as nobody).
pub fn follower_env(
    settings: &Settings,
    ambient: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ambient
        .into_iter()
        .filter(|(name, _)| {
            name.starts_with(crate::collection_names::COLLECTION_OVERRIDE_PREFIX)
                || name == "TRIBLESPACE_PEERS"
                || name == "FACULTIES_WORKSPACE"
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
    let pile = std::fs::canonicalize(storage.path())
        .with_context(|| format!("resolve the pile {}", storage.path().display()))?;
    let key = crate::storage::signer_path(&pile, storage.key_path());
    let key = std::fs::canonicalize(&key)
        .with_context(|| format!("resolve the signing key {}", key.display()))?;
    let command = follower_command(
        &alarm_program()?,
        &pile,
        &key,
        alarm,
        follower_env(settings, routes.iter().cloned()),
    )?;
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
    for orphan in operations.orphans(now, STALE_AFTER)? {
        if Lock::holder(lock_directory, orphan).is_some() {
            continue;
        }
        spawner.spawn(&command(orphan)?)?;
        adopted.push(orphan);
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests;
