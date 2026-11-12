//! Running one rung of the ladder.
//!
//! The ladder ([`super::ladder`]) says which rungs are due; a [`RungRunner`]
//! runs one and says how it ended. Operations record the step afterwards,
//! whatever the outcome, so a channel that FAILS never stalls the escalation.
//! One that hangs is bounded only as far as its transport is: a program
//! rung by [`run_with_timeout`], an HTTP rung by its client's per-request
//! timeout.

use triblespace::prelude::*;

use super::ladder::{Capabilities, Due};
use super::Outcome;

/// What a rung says about one alarm, read once from its raise.
#[derive(Clone, Debug, PartialEq)]
pub struct Subject {
    pub alarm: Id,
    pub raiser: Id,
    pub raiser_label: String,
    /// The class's CLI name (data-loss, ...).
    pub class: String,
    /// The host it was raised on.
    pub host: String,
    pub what: String,
    pub decays: String,
    pub evidence: String,
    pub test: bool,
    /// TAI nanoseconds.
    pub raised_at: i128,
}

impl Subject {
    /// The short id people type and the Discord line carries.
    pub fn id8(&self) -> String {
        format!("{:x}", self.alarm)[..8].to_owned()
    }
}

/// How one rung ended, and what it left that a later reader joins on.
#[derive(Clone, Debug, PartialEq)]
pub struct RungResult {
    pub outcome: Outcome,
    /// The snowflake of the Discord message the rung posted.
    pub discord_message_id: Option<String>,
    /// The say request the rung published.
    pub say_request: Option<Id>,
}

impl RungResult {
    pub fn delivered() -> Self {
        Self {
            outcome: Outcome::Delivered,
            discord_message_id: None,
            say_request: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Failed(error.into()),
            discord_message_id: None,
            say_request: None,
        }
    }
}

/// The executor of the ladder's rungs on one host.
pub trait RungRunner {
    /// The host the rungs run on, as each step records it.
    fn host(&self) -> &str;
    /// What this host cannot run. `jp_in_voice` is the caller's to set:
    /// the follower reads it from the Discord facts on every turn.
    fn capabilities(&mut self, subject: &Subject) -> Capabilities;
    /// Run one rung. Never panics on a dead channel: a failure is an outcome.
    fn run(&mut self, subject: &Subject, due: Due) -> RungResult;
}

// ---- Transports and the executor ------------------------------------------

use anyhow::{anyhow, bail, Context, Result};

use crate::discord::AllowedMentions;

use crate::schemas::alarm::{
    RUNG_COLONY, RUNG_DISCORD_MENTION, RUNG_DISCORD_VOICE, RUNG_FINAL_COLONY, RUNG_FINAL_DISCORD,
    RUNG_HUE_ALERT, RUNG_MAC_NOTIFY, RUNG_MAC_SOUND, RUNG_REACHY_SHOUT,
};

/// Where the rungs go. Every field is configuration for one deployment and
/// has no default: a rung whose target is not configured is recorded as
/// unreachable from this host, so the ledger shows the gap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// JP's Discord user snowflake: the mention pings it, and ack detection
    /// reads his lines by it.
    pub jp_discord_user: Option<String>,
    /// The Discord channel a real alarm's mention goes to.
    pub discord_channel: Option<String>,
    /// The Discord channel a test alarm's line goes to (the raiser's own).
    pub test_channel: Option<String>,
    /// The Discord voice channel a say request is made for.
    pub voice_channel: Option<String>,
    /// The guild of the voice channel, when the say request should name it.
    pub voice_guild: Option<String>,
    /// The Relations group every zooid of the colony is in.
    pub colony_group: Option<String>,
    /// The ssh destination of the Mac that plays sound and shows banners.
    /// Unset on a Mac means the rungs run locally.
    pub mac_ssh: Option<String>,
    /// The Hue room or zone whose lights breathe.
    pub hue_target: Option<String>,
}

/// The volume the Mac sound rung plays at; the old volume is restored.
pub const ALARM_VOLUME: u32 = 80;
/// The sound and how often it plays, once per rung.
pub const ALARM_SOUND: &str = "/System/Library/Sounds/Sosumi.aiff";
pub const ALARM_SOUND_REPEATS: usize = 3;
/// Discord's message length limit.
const DISCORD_LIMIT: usize = 2000;

/// The outside effects a rung has. Implemented over the real channels by
/// [`Live`], and by fakes in tests: nothing else in the executor reaches
/// outside the process.
pub trait Transport {
    /// Send literal text from `from` (a Relations person) to a Relations
    /// person or group.
    fn message(&mut self, from: Id, to: &str, text: &str) -> Result<()>;
    /// Post literal text in a Discord channel, pinging only what `mentions`
    /// allows; the posted message's snowflake.
    fn discord_send(
        &mut self,
        channel: &str,
        text: &str,
        mentions: &crate::discord::AllowedMentions,
    ) -> Result<String>;
    /// Whether this host can open the Discord credential.
    fn discord_ready(&mut self) -> Result<()>;
    /// Publish a say request for a voice channel; its id.
    fn say(&mut self, guild: Option<&str>, channel: &str, text: &str) -> Result<Id>;
    /// Run a program to its end; its stdout. A nonzero exit is an error.
    fn run(&mut self, program: &str, args: &[String]) -> Result<String>;
    /// Ask the Hue bridge to breathe the lights of a room or zone.
    fn hue_alert(&mut self, target: &str) -> Result<()>;
}

/// Quote one word for a POSIX shell: ssh hands the remote side one command
/// line, so every word travels single-quoted.
pub fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// An AppleScript string literal.
pub fn applescript_string(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', r"\\")
            .replace('"', "\\\"")
            .replace(['\n', '\r'], " ")
    )
}

fn clip(text: &str) -> String {
    if text.chars().count() <= DISCORD_LIMIT {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(DISCORD_LIMIT - 1).collect();
    clipped.push('…');
    clipped
}

/// The first Discord line of a real alarm: it pings JP.
pub fn mention_line(subject: &Subject, jp: &str) -> String {
    clip(&format!(
        "<@{jp}> ALARM ({}) from {} on {}: {}. Decays: {}. Reply to this message or say \
         'seen' to stop. [alarm {}]",
        subject.class,
        subject.raiser_label,
        subject.host,
        subject.what,
        subject.decays,
        subject.id8()
    ))
}

/// A later mention while the alarm is still open.
pub fn re_mention_line(subject: &Subject, jp: &str, minutes: i128) -> String {
    clip(&format!(
        "<@{jp}> ALARM still unacknowledged after {minutes} min ({}): {}. Decays: {}. Reply \
         to this message or say 'seen' to stop. [alarm {}]",
        subject.class,
        subject.what,
        subject.decays,
        subject.id8()
    ))
}

/// A test alarm's only Discord line: no one is pinged.
pub fn test_line(subject: &Subject) -> String {
    clip(&format!(
        "TEST alarm ({}) from {} on {}: {}. Decays: {}. Nobody is woken. [alarm {}]",
        subject.class,
        subject.raiser_label,
        subject.host,
        subject.what,
        subject.decays,
        subject.id8()
    ))
}

pub fn final_line(subject: &Subject, jp: &str) -> String {
    clip(&format!(
        "<@{jp}> ALARM still unacknowledged after 60 min; stopped sounding; the alarm stays \
         open: {}. [alarm {}]",
        subject.what,
        subject.id8()
    ))
}

/// What the colony is told when an alarm is raised.
pub fn colony_text(subject: &Subject) -> String {
    format!(
        "{}ALARM {} ({}) raised by {} on {}: {}. Decays: {}. Evidence: {}. If JP answers \
         you, `alarm ack {} --by <him> --via relayed --evidence \"<his words, where>\"`; if \
         you fixed it, `alarm resolve {} --reason \"<what>\"`.",
        if subject.test { "TEST " } else { "" },
        subject.id8(),
        subject.class,
        subject.raiser_label,
        subject.host,
        subject.what,
        subject.decays,
        subject.evidence,
        subject.id8(),
        subject.id8()
    )
}

pub fn final_colony_text(subject: &Subject) -> String {
    format!(
        "ALARM {} is still unacknowledged after 60 min; it stopped sounding and stays open: {}.",
        subject.id8(),
        subject.what
    )
}

/// What the voice says.
pub fn spoken(subject: &Subject) -> String {
    format!("Alarm. {}", subject.what)
}

/// Runs rungs over a [`Transport`] with this host's [`Settings`].
pub struct Executor<T> {
    pub transport: T,
    settings: Settings,
    host: String,
    /// Probe the Mac route too (the follower does; R0 has no Mac rung).
    probe_mac: bool,
    /// What the last probe found unreachable, and why.
    unreachable: Vec<(Id, String)>,
}

impl<T: Transport> Executor<T> {
    pub fn new(transport: T, settings: Settings, host: impl Into<String>) -> Self {
        Self {
            transport,
            settings,
            host: host.into(),
            probe_mac: false,
            unreachable: Vec::new(),
        }
    }

    /// Also probe the Mac route when asked for capabilities.
    pub fn probing_the_mac(mut self) -> Self {
        self.probe_mac = true;
        self
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn mac_route(&self) -> Result<Option<&str>> {
        match self.settings.mac_ssh.as_deref() {
            Some(destination) if !destination.trim().is_empty() => Ok(Some(destination)),
            _ if cfg!(target_os = "macos") => Ok(None),
            _ => bail!("no route to the Mac: ALARM_MAC_SSH is not configured"),
        }
    }

    /// Run one program on the Mac: locally on a Mac without a configured
    /// ssh destination, else through `ssh` with every word quoted.
    fn on_mac(&mut self, program: &str, args: &[String]) -> Result<String> {
        match self.mac_route()?.map(str::to_owned) {
            None => self.transport.run(program, args),
            Some(destination) => {
                let line = std::iter::once(program.to_owned())
                    .chain(args.iter().cloned())
                    .map(|word| shell_quote(&word))
                    .collect::<Vec<_>>()
                    .join(" ");
                let ssh: Vec<String> = [
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    destination.as_str(),
                    "--",
                    line.as_str(),
                ]
                .iter()
                .map(|word| word.to_string())
                .collect();
                self.transport.run("ssh", &ssh)
            }
        }
    }

    fn osascript(&mut self, script: &str) -> Result<String> {
        self.on_mac("osascript", &["-e".to_owned(), script.to_owned()])
    }

    /// Save the output volume, play the sound at [`ALARM_VOLUME`], and put
    /// the volume back, whatever the sound did.
    fn sound(&mut self) -> Result<()> {
        let saved = self
            .osascript("output volume of (get volume settings)")
            .and_then(|volume| {
                volume
                    .trim()
                    .parse::<u32>()
                    .with_context(|| format!("read the Mac's output volume from {volume:?}"))
            });
        let raised = match &saved {
            Ok(_) => self
                .osascript(&format!("set volume output volume {ALARM_VOLUME}"))
                .map(drop),
            Err(_) => Ok(()),
        };
        let mut played = Ok(());
        for _ in 0..ALARM_SOUND_REPEATS {
            if let Err(error) = self.on_mac("afplay", &[ALARM_SOUND.to_owned()]) {
                played = Err(error);
                break;
            }
        }
        let restored = match &saved {
            Ok(volume) => self
                .osascript(&format!("set volume output volume {volume}"))
                .map(drop),
            Err(_) => Ok(()),
        };
        let mut errors = Vec::new();
        for (what, result) in [
            ("read the volume", saved.map(drop)),
            ("raise the volume", raised),
            ("play the sound", played),
            ("restore the volume", restored),
        ] {
            if let Err(error) = result {
                errors.push(format!("{what}: {error:#}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("{}", errors.join("; "))
        }
    }

    fn jp(&self) -> Result<&str> {
        self.settings
            .jp_discord_user
            .as_deref()
            .filter(|jp| !jp.trim().is_empty())
            .ok_or_else(|| anyhow!("ALARM_JP_DISCORD_USER is not configured"))
    }

    fn configured<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str> {
        value
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("{name} is not configured"))
    }

    fn execute(&mut self, subject: &Subject, due: Due) -> Result<RungResult> {
        let mut result = RungResult::delivered();
        match due.rung {
            rung if rung == RUNG_COLONY => {
                let text = colony_text(subject);
                if subject.test {
                    // A test wakes nobody: the colony message goes to the
                    // raiser alone, through the same path.
                    let raiser = format!("{:x}", subject.raiser);
                    self.transport.message(subject.raiser, &raiser, &text)?;
                } else {
                    let group =
                        Self::configured(&self.settings.colony_group, "ALARM_COLONY_GROUP")?
                            .to_owned();
                    self.transport.message(subject.raiser, &group, &text)?;
                }
            }
            rung if rung == RUNG_DISCORD_MENTION => {
                // Every line says whom it may ping: a test nobody, a real
                // alarm JP alone, so an @everyone or a quoted <@someone> in
                // what or decays stays text.
                let (channel, text, mentions) = if subject.test {
                    (
                        Self::configured(
                            &self.settings.test_channel,
                            "ALARM_TEST_DISCORD_CHANNEL",
                        )?
                        .to_owned(),
                        test_line(subject),
                        AllowedMentions::nobody(),
                    )
                } else {
                    let jp = self.jp()?.to_owned();
                    let channel =
                        Self::configured(&self.settings.discord_channel, "ALARM_DISCORD_CHANNEL")?
                            .to_owned();
                    let text = if due.at == 0 {
                        mention_line(subject, &jp)
                    } else {
                        re_mention_line(subject, &jp, due.at / super::MINUTE)
                    };
                    (channel, text, AllowedMentions::only([jp]))
                };
                result.discord_message_id =
                    Some(self.transport.discord_send(&channel, &text, &mentions)?);
            }
            rung if rung == RUNG_FINAL_DISCORD => {
                let jp = self.jp()?.to_owned();
                let channel =
                    Self::configured(&self.settings.discord_channel, "ALARM_DISCORD_CHANNEL")?
                        .to_owned();
                result.discord_message_id = Some(self.transport.discord_send(
                    &channel,
                    &final_line(subject, &jp),
                    &AllowedMentions::only([jp.clone()]),
                )?);
            }
            rung if rung == RUNG_FINAL_COLONY => {
                let group =
                    Self::configured(&self.settings.colony_group, "ALARM_COLONY_GROUP")?.to_owned();
                self.transport
                    .message(subject.raiser, &group, &final_colony_text(subject))?;
            }
            rung if rung == RUNG_DISCORD_VOICE => {
                let channel =
                    Self::configured(&self.settings.voice_channel, "ALARM_VOICE_CHANNEL")?
                        .to_owned();
                let guild = self.settings.voice_guild.clone();
                result.say_request = Some(self.transport.say(
                    guild.as_deref(),
                    &channel,
                    &spoken(subject),
                )?);
            }
            rung if rung == RUNG_MAC_NOTIFY => {
                let script = format!(
                    "display notification {} with title {}",
                    applescript_string(&format!("{} [alarm {}]", subject.what, subject.id8())),
                    applescript_string(&format!("ALARM ({})", subject.class)),
                );
                self.osascript(&script)?;
            }
            rung if rung == RUNG_MAC_SOUND => self.sound()?,
            rung if rung == RUNG_REACHY_SHOUT => {
                self.on_mac("voice", &["shout".to_owned(), spoken(subject)])?;
            }
            rung if rung == RUNG_HUE_ALERT => {
                let target =
                    Self::configured(&self.settings.hue_target, "ALARM_HUE_TARGET")?.to_owned();
                self.transport.hue_alert(&target)?;
            }
            other => bail!("rung {other:x} is not one this build runs"),
        }
        Ok(result)
    }

    /// Why each rung cannot run from here, judged from the configuration,
    /// the Discord credential and (when asked) the Mac route.
    fn probe(&mut self, subject: &Subject) -> Vec<(Id, String)> {
        let mut unreachable = Vec::new();
        let discord = self
            .transport
            .discord_ready()
            .map_err(|error| format!("the Discord credential: {error:#}"));
        let mention_channel = if subject.test {
            Self::configured(&self.settings.test_channel, "ALARM_TEST_DISCORD_CHANNEL")
        } else {
            Self::configured(&self.settings.discord_channel, "ALARM_DISCORD_CHANNEL")
        }
        .map(drop)
        .map_err(|error| error.to_string());
        let jp = if subject.test {
            Ok(())
        } else {
            self.jp().map(drop).map_err(|error| error.to_string())
        };
        for rung in [RUNG_DISCORD_MENTION, RUNG_FINAL_DISCORD] {
            for reason in [&discord, &mention_channel, &jp] {
                if let Err(reason) = reason {
                    unreachable.push((rung, reason.clone()));
                    break;
                }
            }
        }
        if !subject.test {
            if let Err(error) = Self::configured(&self.settings.colony_group, "ALARM_COLONY_GROUP")
            {
                unreachable.push((RUNG_COLONY, error.to_string()));
                unreachable.push((RUNG_FINAL_COLONY, error.to_string()));
            }
        }
        if let Err(error) = Self::configured(&self.settings.voice_channel, "ALARM_VOICE_CHANNEL") {
            unreachable.push((RUNG_DISCORD_VOICE, error.to_string()));
        }
        if let Err(error) = Self::configured(&self.settings.hue_target, "ALARM_HUE_TARGET") {
            unreachable.push((RUNG_HUE_ALERT, error.to_string()));
        }
        let mac = match self.mac_route() {
            Err(error) => Err(error.to_string()),
            Ok(_) if !self.probe_mac => Ok(()),
            Ok(None) => Ok(()),
            Ok(Some(_)) => self
                .on_mac("true", &[])
                .map(drop)
                .map_err(|error| format!("the Mac does not answer: {error:#}")),
        };
        if let Err(reason) = mac {
            for rung in [RUNG_MAC_NOTIFY, RUNG_MAC_SOUND, RUNG_REACHY_SHOUT] {
                unreachable.push((rung, reason.clone()));
            }
        }
        unreachable
    }
}

impl<T: Transport> RungRunner for Executor<T> {
    fn host(&self) -> &str {
        &self.host
    }

    fn capabilities(&mut self, subject: &Subject) -> Capabilities {
        self.unreachable = self.probe(subject);
        Capabilities {
            unreachable: self.unreachable.iter().map(|(rung, _)| *rung).collect(),
            jp_in_voice: false,
        }
    }

    fn run(&mut self, subject: &Subject, due: Due) -> RungResult {
        if let Some((_, reason)) = self.unreachable.iter().find(|(rung, _)| *rung == due.rung) {
            return RungResult::failed(format!("unreachable from {}: {reason}", self.host));
        }
        match self.execute(subject, due) {
            Ok(result) => result,
            Err(error) => RungResult::failed(format!("{error:#}")),
        }
    }
}

/// The real channels: the message faculty, Discord through its send seam
/// and say queue, local programs, and the Hue bridge.
pub struct Live {
    storage: crate::storage::Storage,
    /// How long a program may run before it is killed.
    pub timeout: std::time::Duration,
}

impl Live {
    pub fn new(storage: crate::storage::Storage) -> Self {
        Self {
            storage,
            timeout: std::time::Duration::from_secs(60),
        }
    }
}

impl Transport for Live {
    fn message(&mut self, from: Id, to: &str, text: &str) -> Result<()> {
        let from = format!("{from:x}");
        crate::message::Message::with_storage(self.storage.clone())
            .send(&crate::message::SendOptions {
                from: &from,
                to,
                text,
            })
            .map(drop)
    }

    fn discord_send(
        &mut self,
        channel: &str,
        text: &str,
        mentions: &crate::discord::AllowedMentions,
    ) -> Result<String> {
        crate::discord::Discord::with_storage(self.storage.clone())
            .send_mentioning(channel, text, mentions)
            .map(|receipt| receipt.message_id)
    }

    fn discord_ready(&mut self) -> Result<()> {
        crate::discord::Discord::with_storage(self.storage.clone())
            .token()
            .map(drop)
    }

    fn say(&mut self, guild: Option<&str>, channel: &str, text: &str) -> Result<Id> {
        crate::discord::Discord::with_storage(self.storage.clone()).request_speech(
            guild,
            channel,
            text,
            &crate::voice::options::SpeechOptions::default(),
            None,
        )
    }

    fn run(&mut self, program: &str, args: &[String]) -> Result<String> {
        run_with_timeout(program, args, self.timeout)
    }

    fn hue_alert(&mut self, target: &str) -> Result<()> {
        let report = crate::home::Home::with_storage(self.storage.clone())
            .change(target, &crate::home::Request::Alert)?;
        if report.refused() {
            bail!(
                "the Hue bridge refused the alert: {}",
                report.errors.join("; ")
            );
        }
        Ok(())
    }
}

/// The most stdout a program rung keeps; the rest is read and dropped.
pub const OUTPUT_CAP: usize = 64 * 1024;
/// The most stderr a program rung keeps for its error.
const ERROR_CAP: usize = 8 * 1024;
/// How long the output of a program that has exited may stay open (a
/// descendant holding it) before the program's group is killed.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(1);

/// Read a pipe to its end on its own thread, keeping at most `cap` bytes, so
/// a program that prints a lot never blocks on a full pipe.
fn drain<R: std::io::Read + Send + 'static>(
    mut pipe: R,
    cap: usize,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let room = cap.saturating_sub(kept.len());
                    kept.extend_from_slice(&buffer[..read.min(room)]);
                }
            }
        }
        let _ = sender.send(kept);
    });
    receiver
}

/// Kill a whole process group (the program and every descendant it left).
fn kill_group(pgid: u32) {
    if let Ok(pgid) = i32::try_from(pgid) {
        if pgid > 0 {
            // SAFETY: SIGKILL to a process group this function started.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

/// Run a program with no stdin and return its stdout (at most
/// [`OUTPUT_CAP`] bytes), all within `timeout` plus a bounded settle.
///
/// What is bounded: the program runs in its own process group; stdout and
/// stderr are drained on their own threads while it runs, so a large output
/// cannot block it; at the deadline the whole group is killed and the
/// output is not waited for any longer; after the program exits, output a
/// descendant still holds open is waited for at most [`SETTLE`], then the
/// group is killed and the readers are given [`SETTLE`] once more before
/// they are abandoned. So this returns within about `timeout + 2 * SETTLE`.
/// For an `ssh` rung that bounds the ssh CLIENT; whether the remote command
/// stops when the client is killed is up to the remote side.
///
/// What is not bounded here: the rungs that are not programs. Discord
/// sends, pulls and say requests rely on the Discord client's per-request
/// timeout, the Hue alert on the bridge client's, and the message faculty
/// and the pile operations around every step on nothing of their own.
pub fn run_with_timeout(
    program: &str,
    args: &[String],
    timeout: std::time::Duration,
) -> Result<String> {
    use std::os::unix::process::CommandExt;
    let started = std::time::Instant::now();
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let group = child.id();
    let stdout = drain(child.stdout.take().expect("piped stdout"), OUTPUT_CAP);
    let stderr = drain(child.stderr.take().expect("piped stderr"), ERROR_CAP);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("wait for {program}"))?
        {
            break status;
        }
        if started.elapsed() > timeout {
            kill_group(group);
            let _ = child.wait();
            bail!("{program} did not finish within {} s", timeout.as_secs());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let settle = |receiver: &std::sync::mpsc::Receiver<Vec<u8>>| {
        receiver
            .recv_timeout(SETTLE)
            .or_else(|_| {
                // A descendant holds the output open: end the group, and
                // give the reader one more bounded wait.
                kill_group(group);
                receiver.recv_timeout(SETTLE)
            })
            .unwrap_or_default()
    };
    let out = settle(&stdout);
    let err = settle(&stderr);
    if !status.success() {
        bail!(
            "{program} failed ({status}): {}",
            String::from_utf8_lossy(&err).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
pub(crate) mod tests;
