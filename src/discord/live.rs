//! `discord live`: the faculty's resident process, holding Discord's one
//! gateway session.
//!
//! The session ([`super::gateway`]) identifies, heartbeats and resumes across
//! reconnects, and everything the faculty keeps open rides on it. With intake
//! configured ([`super::intake`]) it reads text channels, and DMs to the bot,
//! into the discord collection, where orient finds them. With a voice channel
//! configured, a voice connection hangs off the same session (`voice`, built
//! with the `discord-voice` feature): the bot joins the channel through the
//! session and speaks what `discord say` queues in the state directory; with
//! the `discord-hearing` feature and a model to hear with, it also hears the
//! channel, and what is said there goes into the collection through intake.
//! Who comes into and leaves the voice channel goes there too, with intake
//! running: the session holds every member's voice state, and
//! [`super::presence`] tells a join and a leave from the rest. Discord
//! refusing the message intents stops only messages: what is heard and who
//! comes and goes need none of them.
//!
//! The process runs until the gateway session ends for good, the speech model
//! cannot load or breaks, or it is asked to stop. Nothing else ends it:
//! gateway reconnects, lost voice connections, a line that could not be
//! spoken and intake failures are all recovered from.

#[cfg(feature = "discord-voice")]
use super::voice;
use super::{gateway, intake, presence};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long queued intake work may take to finish at shutdown.
const INTAKE_DRAIN: Duration = Duration::from_secs(20);

/// The state directory `discord live` and `discord say` share: `say/` holds
/// queued lines, `said/` spoken ones and `failed/` the ones that could not be
/// spoken; `intake/` is intake's, utterances that could not be stored
/// included.
#[derive(Clone, Debug)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The given directory, or else the default one per user:
    /// `$XDG_DATA_HOME/faculties/discord`, or
    /// `~/.local/share/faculties/discord` without XDG_DATA_HOME.
    pub fn resolve(given: Option<PathBuf>) -> Result<Self> {
        match given {
            Some(root) => Ok(Self::new(root)),
            None => default_root(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
                .map(Self::new),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where intake keeps the floor each channel's intake begins at.
    pub fn intake(&self) -> PathBuf {
        self.root.join("intake")
    }
}

/// The default state directory under the XDG data home. A relative
/// XDG_DATA_HOME is ignored, as the XDG base directory specification asks;
/// with neither variable usable there is no default, rather than one relative
/// to wherever the process happens to run.
fn default_root(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    let absolute = |value: Option<OsString>| value.map(PathBuf::from).filter(|p| p.is_absolute());
    let data = match (absolute(xdg_data_home), absolute(home)) {
        (Some(data), _) => data,
        (None, Some(home)) => home.join(".local/share"),
        (None, None) => bail!(
            "no default state directory: neither XDG_DATA_HOME nor HOME is an absolute \
             path; pass --state-dir or set DISCORD_STATE_DIR"
        ),
    };
    Ok(data.join("faculties/discord"))
}

/// What the resident process holds on the gateway session.
pub struct Live {
    pub token: String,
    /// Text channels and DMs read into the discord collection, when configured.
    pub intake: Option<intake::Intake>,
    /// The voice connection, when a voice channel is configured.
    #[cfg(feature = "discord-voice")]
    pub voice: Option<voice::Config>,
}

/// A part of the process that runs beside the gateway session as a task of
/// its own (the voice connection): it is handed every dispatch, in order, and
/// winds down once they stop coming.
pub struct Beside {
    pub dispatches: mpsc::UnboundedSender<Value>,
    pub task: tokio::task::JoinHandle<Result<()>>,
}

/// Run the process until the gateway session ends for good, the voice
/// connection's task ends, or the process is asked to stop.
pub async fn run(live: Live) -> Result<()> {
    let wanted = live.intake.as_ref().map_or(0, intake::Intake::intents);
    #[cfg(feature = "discord-voice")]
    let voiced = live.voice.is_some();
    #[cfg(not(feature = "discord-voice"))]
    let voiced = false;
    // A voice connection needs voice's intents and goes on without intake's
    // if Discord refuses them; without one, intake's are all the session is
    // for.
    let (intents, optional) = if voiced {
        (gateway::VOICE_INTENTS, wanted)
    } else {
        (wanted, 0)
    };
    anyhow::ensure!(
        intents != 0,
        "nothing to run: no voice channel and no intake"
    );
    // Intake's first backfill follows the session's first READY, after the
    // bot's own account is recorded, so that its earlier messages are known
    // as its own the moment they are stored.
    let mut intake = live.intake.map(intake::start);
    let mut discord = gateway::start(gateway::Config {
        token: live.token.clone(),
        intents,
        optional,
        gateway: gateway::GATEWAY.to_owned(),
        backoff: gateway::BACKOFF,
    });
    // Who is in the voice channel, to tell joins and leaves apart.
    #[cfg(feature = "discord-voice")]
    let mut members = live
        .voice
        .as_ref()
        .map(|config| presence::Members::new(config.guild, config.channel));
    #[cfg(not(feature = "discord-voice"))]
    let mut members: Option<presence::Members> = None;
    #[cfg(feature = "discord-voice")]
    let mut voice_task = live.voice.map(|config| {
        // What is heard is stored through intake.
        #[cfg(feature = "discord-hearing")]
        let hears = config.hearing.is_some();
        #[cfg(not(feature = "discord-hearing"))]
        let hears = false;
        let heard = hears
            .then(|| intake.as_ref().and_then(intake::Worker::inbox))
            .flatten();
        voice::start(config, live.token.clone(), discord.commands.clone(), heard)
    });
    #[cfg(not(feature = "discord-voice"))]
    let mut voice_task: Option<Beside> = None;

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("listen for SIGTERM")?;
    let outcome = loop {
        tokio::select! {
            ended = &mut discord.task => {
                break match ended {
                    Ok(Ok(())) => Err(anyhow!("the gateway session ended")),
                    Ok(Err(error)) => Err(error.context("the gateway session failed")),
                    Err(error) => Err(anyhow!("the gateway task stopped: {error}")),
                };
            }
            ended = finished(&mut voice_task) => {
                // Its task is over: nothing is left to wait for at shutdown.
                voice_task = None;
                break match ended {
                    Ok(Ok(())) => Err(anyhow!("the voice connection stopped")),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(anyhow!("the voice connection's task stopped: {error}")),
                };
            }
            Some(event) = discord.events.recv() => {
                if let Some(worker) = &mut intake {
                    let now_ms = unix_ms(std::time::SystemTime::now());
                    to_intake(worker, members.as_mut(), &event, now_ms);
                }
                if let (gateway::Event::Dispatch(dispatch), Some(beside)) = (event, &voice_task) {
                    let _ = beside.dispatches.send(dispatch);
                }
            }
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
        }
    };
    // The bot leaves the voice channel once its dispatches stop coming.
    if let Some(beside) = voice_task.take() {
        drop(beside.dispatches);
        let _ = beside.task.await;
    }
    if let Some(worker) = intake {
        worker.stop(INTAKE_DRAIN).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    outcome
}

/// Resolves when the task beside the session ends; never while there is none.
async fn finished(
    beside: &mut Option<Beside>,
) -> std::result::Result<Result<()>, tokio::task::JoinError> {
    match beside {
        Some(beside) => (&mut beside.task).await,
        None => std::future::pending().await,
    }
}

/// Milliseconds since the Unix epoch at `time`.
fn unix_ms(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Hand intake its share of a gateway event: what [`route`] takes of a
/// dispatch, and who comes into or leaves the voice channel by it, as
/// `members` tells (seen at `now_ms`). Discord refusing the message intents
/// stops messages and nothing else. It refuses them only to a session that
/// also holds a voice connection, the one place they are optional, and
/// everything intake stores of the voice channel, who comes and goes and
/// what is heard, needs none of them.
fn to_intake(
    worker: &mut intake::Worker,
    members: Option<&mut presence::Members>,
    event: &gateway::Event,
    now_ms: u64,
) {
    match event {
        gateway::Event::Refused(_) => {
            eprintln!(
                "[discord] message intake is off: Discord refused the message intents; who \
                 comes into and leaves the voice channel, and what is heard there, is still \
                 stored"
            );
            worker.send(intake::Work::NoMessages);
        }
        gateway::Event::Dispatch(dispatch) => {
            route(worker, dispatch);
            if let Some(members) = members {
                for change in members.observe(dispatch, now_ms) {
                    worker.send(intake::Work::Presence(change));
                }
            }
        }
    }
}

/// Hand what intake needs of a dispatch to it: messages, the bot's own user
/// id, and a backfill after every READY (the first one is the backfill on
/// start) and RESUMED.
fn route(worker: &mut intake::Worker, dispatch: &Value) {
    match dispatch["t"].as_str() {
        Some("MESSAGE_CREATE") => worker.send(intake::Work::Message(dispatch["d"].clone())),
        Some("MESSAGE_UPDATE") => worker.send(intake::Work::Update(dispatch["d"].clone())),
        Some("READY") => {
            if let Some(user) = dispatch["d"]["user"]["id"].as_str() {
                worker.send(intake::Work::Account(user.to_owned()));
            }
            worker.send(intake::Work::Backfill);
        }
        Some("RESUMED") => worker.send(intake::Work::Backfill),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn intake_hears_messages_the_bot_account_and_a_backfill_per_session() {
        let (sender, inbox) = std::sync::mpsc::channel();
        let mut worker = intake::Worker::from_sender(sender);
        for dispatch in [
            json!({"t": "READY", "d": {"user": {"id": "42"}}}),
            json!({"t": "MESSAGE_CREATE", "d": {"id": "1"}}),
            json!({"t": "MESSAGE_UPDATE", "d": {"id": "1"}}),
            json!({"t": "MESSAGE_DELETE", "d": {"id": "1"}}),
            json!({"t": "VOICE_STATE_UPDATE", "d": {}}),
            json!({"t": "RESUMED", "d": {}}),
        ] {
            route(&mut worker, &dispatch);
        }
        let routed: Vec<String> = inbox.try_iter().map(described).collect();
        assert_eq!(
            routed,
            [
                "account 42",
                "backfill",
                "message \"1\"",
                "update \"1\"",
                "backfill"
            ]
        );
    }

    /// Work handed to intake, as a test compares it.
    fn described(work: intake::Work) -> String {
        match work {
            intake::Work::Message(message) => format!("message {}", message["id"]),
            intake::Work::Update(message) => format!("update {}", message["id"]),
            intake::Work::Account(user) => format!("account {user}"),
            intake::Work::Backfill => "backfill".to_owned(),
            intake::Work::Utterance(_) => "utterance".to_owned(),
            intake::Work::Sentence(_) => "sentence".to_owned(),
            intake::Work::Presence(change) => {
                format!(
                    "{} {} {}",
                    change.user,
                    change.presence.verb(),
                    change.channel
                )
            }
            intake::Work::NoMessages => "no messages".to_owned(),
        }
    }

    /// Discord refusing the message intents stops messages, and never who
    /// comes into and leaves the voice channel, which needs none of them:
    /// intake stays for that, whether or not the channel is heard.
    #[test]
    fn who_comes_and_goes_is_still_stored_once_the_message_intents_are_refused() {
        use std::num::NonZeroU64;
        let (sender, inbox) = std::sync::mpsc::channel();
        let mut worker = intake::Worker::from_sender(sender);
        let mut members =
            presence::Members::new(NonZeroU64::new(300).unwrap(), NonZeroU64::new(201).unwrap());
        for event in [
            gateway::Event::Refused(4014),
            gateway::Event::Dispatch(json!({"t": "READY", "d": {"user": {"id": "900"}}})),
            gateway::Event::Dispatch(json!({"t": "GUILD_CREATE", "d": {
                "id": "300",
                "voice_states": [{"user_id": "900", "channel_id": "201", "session_id": "b"}],
            }})),
            gateway::Event::Dispatch(json!({"t": "VOICE_STATE_UPDATE", "d": {
                "guild_id": "300", "channel_id": "201", "user_id": "400", "session_id": "s",
            }})),
        ] {
            to_intake(&mut worker, Some(&mut members), &event, 7);
        }
        let routed: Vec<String> = inbox.try_iter().map(described).collect();
        assert_eq!(
            routed,
            ["no messages", "account 900", "backfill", "400 joined 201"]
        );
    }

    #[test]
    fn the_default_state_directory_follows_xdg_and_is_never_relative() {
        let os = |value: &str| Some(OsString::from(value));
        let under = |root: &str| PathBuf::from(root).join("faculties/discord");
        assert_eq!(
            default_root(os("/data"), os("/home/u")).unwrap(),
            under("/data")
        );
        assert_eq!(
            default_root(None, os("/home/u")).unwrap(),
            under("/home/u/.local/share")
        );
        // A relative XDG_DATA_HOME is ignored; a relative or missing HOME
        // leaves no default.
        assert_eq!(
            default_root(os("data"), os("/home/u")).unwrap(),
            under("/home/u/.local/share")
        );
        assert!(default_root(None, None).is_err());
        assert!(default_root(os(""), os("home")).is_err());
        // A given directory is taken as it is.
        let given = StateDir::resolve(Some(PathBuf::from("/s"))).unwrap();
        assert_eq!(given.intake(), PathBuf::from("/s/intake"));
    }
}
