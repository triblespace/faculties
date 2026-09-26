//! The gateway session of `discord live`: one Discord session carried across
//! as many websocket connections as it takes.
//!
//! Discord ends connections routinely: it asks for a reconnect (op 7),
//! invalidates the session (op 9), closes with a code, or stops acknowledging
//! heartbeats. None of that has to end the session. A connection that ends
//! resumable is followed by RESUME (op 6) on the session's resume URL, and
//! Discord replays what was missed; one that does not is followed by a fresh
//! IDENTIFY. Only the close codes the gateway documentation marks as not
//! reconnectable (4004 and 4010-4014) end the session, and 4013 or 4014 only
//! once the optional intents have been given up.
//!
//! A network that is gone never gives a session up: a connection that cannot
//! even open says nothing about the session, and Discord says itself when a
//! session is over (op 9 with d=false, 4007, 4009). So an outage of any length
//! ends in a RESUME, and the events it missed are replayed.
//!
//! The protocol decisions live in [`SessionState`], [`Heart`] and [`Retry`],
//! which see payloads, close codes and instants and nothing else, so they are
//! tested without a socket; [`start`] runs them over real connections.

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::num::NonZeroU64;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

/// The main gateway, where new sessions identify.
pub const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
/// GUILDS | GUILD_VOICE_STATES: the two intents a voice join needs.
pub const VOICE_INTENTS: u64 = 1 | (1 << 7);
/// Messages in server channels.
pub const GUILD_MESSAGES: u64 = 1 << 9;
/// Messages in DMs to the bot, which carry their content without
/// [`MESSAGE_CONTENT`].
pub const DIRECT_MESSAGES: u64 = 1 << 12;
/// The content of messages in server channels. Privileged: unless it is
/// enabled for the bot in the developer portal, Discord refuses a session
/// that asks for it with close code 4014.
pub const MESSAGE_CONTENT: u64 = 1 << 15;
/// The first wait after a connection that did not last; it doubles from there.
pub const BACKOFF: Duration = Duration::from_secs(1);
/// Connections in a row that did not last after which every other RESUME goes
/// to the main gateway, in case the resume host is gone; and RESUMEs in a row
/// the gateway answered with neither RESUMED nor op 9 after which the session
/// is given up.
const RESUME_ATTEMPTS: u32 = 3;
/// How long a connection may take to open and say HELLO.
const OPENING: Duration = Duration::from_secs(30);
/// The close code this side ends a connection with when it means to resume:
/// closing with 1000 or 1001 would invalidate the session.
const RECONNECTING: u16 = 4000;

/// How the session connects and what it asks for.
pub struct Config {
    pub token: String,
    /// The intents every session needs.
    pub intents: u64,
    /// Further intents, given up if Discord refuses them (4013, 4014) so that
    /// the session goes on with `intents` alone.
    pub optional: u64,
    /// Where new sessions identify: [`GATEWAY`], or a stand-in under test.
    pub gateway: String,
    /// The first wait after a failed connection: [`BACKOFF`].
    pub backoff: Duration,
}

/// What the session hands on.
#[derive(Debug)]
pub enum Event {
    /// A dispatch (op 0), in order, every READY and RESUMED included.
    Dispatch(Value),
    /// Discord refused the optional intents with this close code; the session
    /// goes on without them.
    Refused(u16),
}

/// The running session: its events, a sender for commands such as voice
/// state updates, and the task, which ends only when the session cannot
/// continue. Commands are held until a connection is READY or RESUMED, and go
/// out on it in order; a voice state update held meanwhile replaces every
/// earlier one for its guild, so an outage never ends in a burst of joins.
pub struct Gateway {
    pub events: mpsc::UnboundedReceiver<Event>,
    pub commands: mpsc::UnboundedSender<Value>,
    pub task: tokio::task::JoinHandle<Result<()>>,
}

pub fn start(config: Config) -> Gateway {
    let (event, events) = mpsc::unbounded_channel();
    let (commands, inbox) = mpsc::unbounded_channel();
    let task = tokio::spawn(session(config, event, inbox));
    Gateway {
        events,
        commands,
        task,
    }
}

/// Op 4, voice state update: join `channel` in `guild`, or leave it with None.
pub fn voice_state(guild: NonZeroU64, channel: Option<NonZeroU64>) -> Value {
    json!({
        "op": 4,
        "d": {"guild_id": guild.to_string(), "channel_id": channel.map(|id| id.to_string()),
              "self_mute": false, "self_deaf": false}
    })
}

/// How the next connection introduces itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// RESUME on the resume URL: the same session, missed events replayed.
    Resume,
    /// IDENTIFY on the main gateway: a new session.
    Identify,
}

/// How one connection ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// Connect again, introduced as `next`, after `pause`.
    Reconnect { next: Next, pause: Duration },
    /// Discord refused the optional intents with this close code; they are
    /// given up, and the next connection identifies without them.
    Refused(u16),
    /// The session cannot continue.
    Stop(String),
}

/// How far one connection got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reached {
    /// It never opened with HELLO: the network, DNS, TLS or the host failed,
    /// which says nothing about the session.
    Nothing,
    /// It sent IDENTIFY or RESUME and heard nothing after it: the connection
    /// failed or closed without a code, which says nothing about the session
    /// either.
    Introduced,
    /// The gateway answered the IDENTIFY or RESUME (a payload, or a close
    /// code) with neither READY nor RESUMED.
    Heard,
    /// READY or RESUMED arrived, but no heartbeat was acknowledged.
    Established,
    /// It was established and a heartbeat was acknowledged: it worked.
    Lasted,
}

/// What one payload asks of the connection that read it.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Continue,
    /// A dispatch (op 0), to hand on.
    Dispatch,
    /// Discord asked for a heartbeat now (op 1).
    Beat,
    /// Discord acknowledged a heartbeat (op 11).
    Acked,
    /// This connection is over.
    End(Ended),
}

/// What the session keeps across connections: the last sequence number seen,
/// the session id and resume URL that READY named, and the intents a new
/// session asks for.
#[derive(Debug, Default)]
pub struct SessionState {
    pub sequence: Option<u64>,
    pub resumable: Option<(String, String)>,
    pub intents: u64,
    /// The part of `intents` that is given up when Discord refuses it.
    pub optional: u64,
}

impl SessionState {
    pub fn new(intents: u64, optional: u64) -> Self {
        Self {
            intents: intents | optional,
            optional,
            ..Self::default()
        }
    }

    /// Record what one payload says about the session, and say what it asks
    /// of the connection.
    pub fn observe(&mut self, payload: &Value) -> Step {
        if let Some(sequence) = payload["s"].as_u64() {
            self.sequence = Some(sequence);
        }
        match payload["op"].as_u64() {
            Some(0) => {
                let d = &payload["d"];
                if payload["t"] == "READY" {
                    if let (Some(id), Some(url)) =
                        (d["session_id"].as_str(), d["resume_gateway_url"].as_str())
                    {
                        self.resumable = Some((id.to_owned(), url.to_owned()));
                    }
                }
                Step::Dispatch
            }
            Some(1) => Step::Beat,
            Some(7) => Step::End(self.again(Duration::ZERO)),
            // d says whether the session may still be resumed. When it may
            // not, the documentation asks for a random 1-5 s wait before the
            // new IDENTIFY.
            Some(9) if payload["d"] == true => Step::End(self.again(Duration::ZERO)),
            Some(9) => {
                self.forget();
                let pause = Duration::from_millis(1000 + (jitter() * 4000.0) as u64);
                Step::End(Ended::Reconnect {
                    next: Next::Identify,
                    pause,
                })
            }
            Some(11) => Step::Acked,
            _ => Step::Continue,
        }
    }

    /// How to go on after the connection closed with `code`, or failed
    /// without one (None).
    pub fn closed(&mut self, code: Option<u16>) -> Ended {
        match code {
            Some(4004) => Ended::Stop("Discord refused the bot token (close code 4004)".into()),
            // Invalid or disallowed intents. Only the optional ones can be at
            // fault while they are asked for (the privileged Message Content
            // intent above all): give them up and go on without them.
            Some(code @ (4013 | 4014)) if self.intents & self.optional != 0 => {
                self.intents &= !self.optional;
                self.forget();
                Ended::Refused(code)
            }
            // Asked for and refused, the privileged intent is the likely
            // cause, and the one thing to fix in the developer portal.
            Some(4014) if self.intents & MESSAGE_CONTENT != 0 => Ended::Stop(
                "Discord refused the Message Content intent (close code 4014); enable it for \
                 the bot in the developer portal"
                    .into(),
            ),
            Some(code @ 4010..=4014) => Ended::Stop(format!(
                "Discord closed the session for good (close code {code})"
            )),
            // An invalid sequence on resume, or a session that timed out:
            // the documentation asks for a new session.
            Some(4007 | 4009) => {
                self.forget();
                self.again(Duration::ZERO)
            }
            _ => self.again(Duration::ZERO),
        }
    }

    fn again(&self, pause: Duration) -> Ended {
        let next = match self.resumable {
            Some(_) => Next::Resume,
            None => Next::Identify,
        };
        Ended::Reconnect { next, pause }
    }

    /// Give the session up: the next connection starts a new one.
    pub fn forget(&mut self) {
        self.resumable = None;
        self.sequence = None;
    }

    /// Where a connection introduced as `next` goes.
    pub fn url(&self, next: Next, gateway: &str) -> String {
        match (next, &self.resumable) {
            (Next::Resume, Some((_, url))) => {
                format!("{}/?v=10&encoding=json", url.trim_end_matches('/'))
            }
            _ => gateway.to_owned(),
        }
    }

    /// The payload a connection introduced as `next` opens with.
    pub fn introduction(&self, next: Next, token: &str) -> Value {
        match (next, &self.resumable) {
            (Next::Resume, Some((session_id, _))) => json!({
                "op": 6,
                "d": {"token": token, "session_id": session_id, "seq": self.sequence}
            }),
            _ => json!({
                "op": 2,
                "d": {
                    "token": token,
                    "intents": self.intents,
                    "properties": {
                        "os": std::env::consts::OS,
                        "browser": "faculties-discord",
                        "device": "faculties-discord"
                    }
                }
            }),
        }
    }

    pub fn heartbeat(&self) -> Value {
        json!({"op": 1, "d": self.sequence})
    }
}

/// Heartbeat bookkeeping for one connection. Every beat is owed an ACK
/// (op 11); a beat still owed a full interval after it was sent means the
/// connection is a zombie, and it is replaced by a resumed one.
#[derive(Debug, Default)]
pub struct Heart {
    /// When the oldest beat still owed an ACK was sent.
    owed: Option<Instant>,
}

/// What a scheduled heartbeat tick comes to.
#[derive(Debug, PartialEq, Eq)]
pub enum Beat {
    Send,
    /// A beat went out less than an interval ago, answering op 1; its ACK may
    /// still be on the way.
    Wait,
    Zombie,
}

impl Heart {
    /// The tick of an interval of `period`, falling at `now`.
    pub fn due(&mut self, now: Instant, period: Duration) -> Beat {
        match self.owed {
            None => {
                self.owed = Some(now);
                Beat::Send
            }
            Some(sent) if now.saturating_duration_since(sent) >= period => Beat::Zombie,
            Some(_) => Beat::Wait,
        }
    }

    /// Discord asked for a beat (op 1) at `now`; it is sent at once and owed
    /// an ACK.
    pub fn asked(&mut self, now: Instant) {
        self.owed.get_or_insert(now);
    }

    pub fn acked(&mut self) {
        self.owed = None;
    }
}

/// When the session connects again, and where.
///
/// Connections that did not last back off from the configured unit to sixty
/// times it. A session is given up only when Discord says so, or after
/// [`RESUME_ATTEMPTS`] RESUMEs in a row that the gateway answered without
/// accepting or refusing them; a connection lost before any answer does not
/// count. Once connections keep failing, every other RESUME goes to the main
/// gateway, which resumes the session or says it cannot.
/// IDENTIFYs that did not last are spaced further apart, up to ten minutes,
/// because each one spends one of the bot's daily session starts.
#[derive(Debug)]
pub struct Retry {
    unit: Duration,
    /// Connections in a row that did not last.
    failures: u32,
    /// RESUMEs in a row the gateway answered and neither accepted nor refused.
    unanswered: u32,
    /// IDENTIFYs in a row that were sent and did not last.
    identified: u32,
}

impl Retry {
    pub fn new(unit: Duration) -> Self {
        Self {
            unit,
            failures: 0,
            unanswered: 0,
            identified: 0,
        }
    }

    /// Where the next connection, introduced as `next`, goes.
    pub fn url(&self, state: &SessionState, next: Next, gateway: &str) -> String {
        if next == Next::Resume && self.failures >= RESUME_ATTEMPTS && self.failures % 2 == 1 {
            return gateway.to_owned();
        }
        state.url(next, gateway)
    }

    /// A connection introduced as `tried` got as far as `reached` and asked
    /// to go on as `next` after `pause`: how the following connection is
    /// introduced, and the wait before it.
    pub fn after(
        &mut self,
        state: &mut SessionState,
        tried: Next,
        reached: Reached,
        next: Next,
        pause: Duration,
    ) -> (Next, Duration) {
        let lasted = reached == Reached::Lasted;
        self.failures = if lasted {
            0
        } else {
            self.failures.saturating_add(1)
        };
        if reached >= Reached::Established {
            self.unanswered = 0;
        } else if reached == Reached::Heard && tried == Next::Resume {
            self.unanswered += 1;
        }
        if lasted {
            self.identified = 0;
        } else if reached >= Reached::Introduced && tried == Next::Identify {
            self.identified = self.identified.saturating_add(1);
        }
        let mut next = next;
        if next == Next::Resume && self.unanswered >= RESUME_ATTEMPTS {
            eprintln!(
                "[discord] the gateway answered {} resumes in a row and took none; starting a new session",
                self.unanswered
            );
            state.forget();
            self.unanswered = 0;
            next = Next::Identify;
        }
        let mut wait = pause.max(self.backoff());
        if next == Next::Identify {
            wait = wait.max(self.spacing());
        }
        (next, wait)
    }

    fn backoff(&self) -> Duration {
        match self.failures {
            0 => Duration::ZERO,
            n => self.unit * (1 << (n - 1).min(6)).min(60),
        }
    }

    fn spacing(&self) -> Duration {
        match self.identified {
            0 => Duration::ZERO,
            n => self.unit * (5 << (n - 1).min(7)).min(600),
        }
    }
}

/// A fraction in [0, 1), different on every call: std's randomly keyed
/// hasher is random enough to spread heartbeats and pauses.
fn jitter() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let bits = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// The session: connection after connection until one ends it for good.
async fn session(
    config: Config,
    events: mpsc::UnboundedSender<Event>,
    mut commands: mpsc::UnboundedReceiver<Value>,
) -> Result<()> {
    let mut state = SessionState::new(config.intents, config.optional);
    let mut retry = Retry::new(config.backoff);
    let mut next = Next::Identify;
    loop {
        let url = retry.url(&state, next, &config.gateway);
        let mut reached = Reached::Nothing;
        let ended = match connection(
            &config,
            &url,
            &mut state,
            next,
            &mut reached,
            &events,
            &mut commands,
        )
        .await
        {
            Ok(ended) => ended,
            Err(error) => {
                eprintln!("[discord] gateway connection failed: {error:#}");
                state.closed(None)
            }
        };
        let (following, pause) = match ended {
            Ended::Stop(reason) => bail!(reason),
            Ended::Refused(code) => {
                eprintln!(
                    "[discord] Discord refused the message intents (close code {code}); \
                     going on with voice only. Reading messages needs the Message \
                     Content intent enabled for the bot in the developer portal"
                );
                let _ = events.send(Event::Refused(code));
                (Next::Identify, Duration::ZERO)
            }
            Ended::Reconnect { next, pause } => (next, pause),
        };
        let (following, wait) = retry.after(&mut state, next, reached, following, pause);
        next = following;
        eprintln!(
            "[discord] gateway: {} in {:.1} s",
            match next {
                Next::Resume => "resuming",
                Next::Identify => "identifying",
            },
            wait.as_secs_f64()
        );
        tokio::time::sleep(wait).await;
    }
}

/// One websocket connection of the session, to `url`. `Ok` says how it
/// ended; an error is a connection that failed, and the session goes on as
/// after a close without a code. `reached` records how far it got.
async fn connection(
    config: &Config,
    url: &str,
    state: &mut SessionState,
    next: Next,
    reached: &mut Reached,
    events: &mpsc::UnboundedSender<Event>,
    commands: &mut mpsc::UnboundedReceiver<Value>,
) -> Result<Ended> {
    let opening = async {
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("connect to {url}"))?;
        let (sink, mut stream) = socket.split();
        let hello = read(&mut stream).await?;
        anyhow::Ok((sink, stream, hello))
    };
    let (mut sink, mut stream, hello) = tokio::time::timeout(OPENING, opening)
        .await
        .with_context(|| format!("{url} did not open with HELLO within {OPENING:?}"))??;
    let interval = match hello {
        Incoming::Payload(hello) if hello["op"] == 10 => hello["d"]["heartbeat_interval"]
            .as_u64()
            .context("HELLO carries a heartbeat interval")?,
        Incoming::Payload(other) => bail!("the gateway opened without HELLO: {other}"),
        Incoming::Closed(code) => return Ok(state.closed(code)),
    };
    send(&mut sink, &state.introduction(next, &config.token)).await?;
    *reached = Reached::Introduced;
    // The first beat falls at a random point of the first interval, as the
    // documentation asks, so clients reconnecting together do not beat in step.
    let period = Duration::from_millis(interval.max(1));
    let mut beats = tokio::time::interval_at(Instant::now() + period.mul_f64(jitter()), period);
    beats.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut heart = Heart::default();
    loop {
        let established = *reached >= Reached::Established;
        tokio::select! {
            incoming = read(&mut stream) => {
                let payload = match incoming? {
                    Incoming::Payload(payload) => payload,
                    Incoming::Closed(code) => {
                        eprintln!("[discord] the gateway closed the connection (code {code:?})");
                        if code.is_some() {
                            *reached = (*reached).max(Reached::Heard);
                        }
                        return Ok(state.closed(code));
                    }
                };
                *reached = (*reached).max(Reached::Heard);
                match state.observe(&payload) {
                    Step::Continue => {}
                    Step::Dispatch => {
                        if matches!(payload["t"].as_str(), Some("READY" | "RESUMED")) {
                            *reached = (*reached).max(Reached::Established);
                        }
                        let _ = events.send(Event::Dispatch(payload));
                    }
                    Step::Beat => {
                        heart.asked(Instant::now());
                        send(&mut sink, &state.heartbeat()).await?;
                    }
                    Step::Acked => {
                        heart.acked();
                        if *reached == Reached::Established {
                            *reached = Reached::Lasted;
                        }
                    }
                    Step::End(ended) => {
                        eprintln!("[discord] the gateway ended the connection with op {}", payload["op"]);
                        close(&mut sink).await;
                        return Ok(ended);
                    }
                }
            }
            _ = beats.tick() => match heart.due(Instant::now(), period) {
                Beat::Send => send(&mut sink, &state.heartbeat()).await?,
                Beat::Wait => {}
                Beat::Zombie => {
                    eprintln!("[discord] the gateway did not acknowledge a heartbeat; reconnecting");
                    close(&mut sink).await;
                    return Ok(state.closed(None));
                }
            },
            Some(command) = commands.recv(), if established => {
                // Everything held while no connection was up is here at once.
                let mut held = vec![command];
                while let Ok(command) = commands.try_recv() {
                    held.push(command);
                }
                for command in coalesce(held) {
                    send(&mut sink, &command).await?;
                }
            }
        }
    }
}

/// Commands in the order they go out, each voice state update (op 4) in
/// place of every earlier one for its guild: only the newest says where the
/// bot is to be, and Discord closes a connection that sends more than 120
/// commands a minute.
fn coalesce(commands: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(commands.len());
    for command in commands {
        if command["op"] == 4 {
            let guild = &command["d"]["guild_id"];
            out.retain(|earlier| !(earlier["op"] == 4 && earlier["d"]["guild_id"] == *guild));
        }
        out.push(command);
    }
    out
}

enum Incoming {
    Payload(Value),
    /// The connection closed, with the close code if Discord sent one.
    Closed(Option<u16>),
}

/// The next JSON payload from the gateway, or how the connection closed.
async fn read<S>(stream: &mut S) -> Result<Incoming>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            None => return Ok(Incoming::Closed(None)),
            Some(Err(error)) => return Err(error).context("read from the gateway"),
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(text.as_str())
                    .map(Incoming::Payload)
                    .context("parse a gateway payload");
            }
            Some(Ok(Message::Close(frame))) => {
                return Ok(Incoming::Closed(frame.map(|frame| u16::from(frame.code))));
            }
            Some(Ok(_)) => {}
        }
    }
}

async fn send<S>(sink: &mut S, payload: &Value) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    sink.send(Message::text(payload.to_string()))
        .await
        .context("write to the gateway")
}

/// End a connection so that its session stays resumable.
async fn close<S>(sink: &mut S)
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let frame = CloseFrame {
        code: CloseCode::from(RECONNECTING),
        reason: "reconnecting".into(),
    };
    let _ = sink.send(Message::Close(Some(frame))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;

    const OPTIONAL: u64 = GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT;

    fn ready(session_id: &str, url: &str, sequence: u64) -> Value {
        json!({"op": 0, "s": sequence, "t": "READY", "d": {
            "session_id": session_id, "resume_gateway_url": url, "user": {"id": "42"}
        }})
    }

    fn resumable() -> SessionState {
        let mut state = SessionState::new(VOICE_INTENTS, OPTIONAL);
        assert_eq!(
            state.observe(&ready("abc", "wss://resume.example", 1)),
            Step::Dispatch
        );
        state
    }

    #[test]
    fn a_reconnect_request_resumes_where_the_session_was() {
        let mut state = resumable();
        state.observe(&json!({"op": 0, "s": 5, "t": "MESSAGE_CREATE", "d": {}}));
        assert_eq!(
            state.observe(&json!({"op": 7, "d": null})),
            Step::End(Ended::Reconnect {
                next: Next::Resume,
                pause: Duration::ZERO
            })
        );
        assert_eq!(
            state.url(Next::Resume, GATEWAY),
            "wss://resume.example/?v=10&encoding=json"
        );
        assert_eq!(
            state.introduction(Next::Resume, "token"),
            json!({"op": 6, "d": {"token": "token", "session_id": "abc", "seq": 5}})
        );
    }

    #[test]
    fn a_resumable_invalid_session_resumes() {
        let mut state = resumable();
        assert_eq!(
            state.observe(&json!({"op": 9, "d": true})),
            Step::End(Ended::Reconnect {
                next: Next::Resume,
                pause: Duration::ZERO
            })
        );
        assert!(state.resumable.is_some());
    }

    #[test]
    fn an_unresumable_invalid_session_identifies_after_a_pause() {
        let mut state = resumable();
        let Step::End(Ended::Reconnect { next, pause }) =
            state.observe(&json!({"op": 9, "d": false}))
        else {
            panic!("op 9 ends the connection");
        };
        assert_eq!(next, Next::Identify);
        assert!(pause >= Duration::from_secs(1) && pause <= Duration::from_secs(5));
        assert_eq!(state.resumable, None);
        assert_eq!(state.sequence, None);
        assert_eq!(state.url(Next::Resume, GATEWAY), GATEWAY);
        let identify = state.introduction(next, "token");
        assert_eq!(identify["op"], 2);
        assert_eq!(identify["d"]["intents"], VOICE_INTENTS | OPTIONAL);
    }

    #[test]
    fn close_codes_resume_identify_give_up_intents_or_stop() {
        let resume = Ended::Reconnect {
            next: Next::Resume,
            pause: Duration::ZERO,
        };
        let identify = Ended::Reconnect {
            next: Next::Identify,
            pause: Duration::ZERO,
        };
        for code in [
            None,
            Some(1000),
            Some(1006),
            Some(4000),
            Some(4001),
            Some(4002),
        ] {
            assert_eq!(resumable().closed(code), resume, "{code:?}");
        }
        for code in [4003, 4005, 4008] {
            assert_eq!(resumable().closed(Some(code)), resume, "{code}");
        }
        for code in [4007, 4009] {
            let mut state = resumable();
            assert_eq!(state.closed(Some(code)), identify, "{code}");
            assert_eq!(state.resumable, None);
        }
        // Refused intents are given up once; refused again, voice alone is at
        // fault and the session cannot go on.
        for code in [4013, 4014] {
            let mut state = resumable();
            assert_eq!(state.closed(Some(code)), Ended::Refused(code));
            assert_eq!(state.intents, VOICE_INTENTS);
            assert_eq!(state.resumable, None);
            assert_eq!(
                state.introduction(Next::Identify, "token")["d"]["intents"],
                VOICE_INTENTS
            );
            assert!(matches!(state.closed(Some(code)), Ended::Stop(_)));
        }
        for code in [4004, 4010, 4011, 4012] {
            assert!(
                matches!(resumable().closed(Some(code)), Ended::Stop(_)),
                "{code} ends the session"
            );
        }
        let mut voice_only = SessionState::new(VOICE_INTENTS, 0);
        assert!(matches!(voice_only.closed(Some(4014)), Ended::Stop(_)));
        // Intake alone has nothing to give up: a refusal ends the session and
        // says which intent to enable.
        let mut intake_only = SessionState::new(GUILD_MESSAGES | MESSAGE_CONTENT, 0);
        let Ended::Stop(reason) = intake_only.closed(Some(4014)) else {
            panic!("a refused intake-only session ends");
        };
        assert!(reason.contains("Message Content intent"), "{reason}");
        // Without a session to resume, every reconnect identifies.
        assert_eq!(voice_only.closed(Some(4000)), identify);
    }

    #[test]
    fn the_sequence_follows_dispatches_into_heartbeats() {
        let mut state = SessionState::default();
        assert_eq!(state.heartbeat(), json!({"op": 1, "d": null}));
        state.observe(&ready("abc", "wss://resume.example", 1));
        state.observe(&json!({"op": 0, "s": 2, "t": "GUILD_CREATE", "d": {}}));
        // Payloads without a sequence leave it alone.
        assert_eq!(state.observe(&json!({"op": 11, "s": null})), Step::Acked);
        assert_eq!(state.observe(&json!({"op": 1, "d": null})), Step::Beat);
        assert_eq!(state.sequence, Some(2));
        assert_eq!(state.heartbeat(), json!({"op": 1, "d": 2}));
    }

    #[test]
    fn only_a_beat_owed_for_a_whole_interval_is_a_zombie() {
        let period = Duration::from_millis(1000);
        let start = Instant::now();
        let mut heart = Heart::default();
        assert_eq!(heart.due(start, period), Beat::Send);
        heart.acked();
        assert_eq!(heart.due(start + period, period), Beat::Send);
        assert_eq!(
            heart.due(start + period * 2, period),
            Beat::Zombie,
            "a beat still owed an ACK a whole interval later is a zombie connection"
        );

        // An op 1 answered just before the scheduled tick: its ACK may still
        // be on the way, so the tick neither beats again nor gives up.
        let mut heart = Heart::default();
        heart.asked(start + Duration::from_millis(980));
        assert_eq!(heart.due(start + period, period), Beat::Wait);
        heart.acked();
        assert_eq!(heart.due(start + period * 2, period), Beat::Send);
        // Unacknowledged for a whole interval, it is a zombie after all.
        let mut heart = Heart::default();
        heart.asked(start);
        assert_eq!(heart.due(start + period, period), Beat::Zombie);
    }

    #[test]
    fn retries_back_off_space_identifies_and_keep_the_session() {
        let unit = Duration::from_secs(1);
        let mut state = resumable();
        let mut retry = Retry::new(unit);
        // A network that is gone: nothing opens, the session is kept however
        // long it lasts, and the waits double to a minute.
        let mut waits = Vec::new();
        for _ in 0..9 {
            let (next, wait) = retry.after(
                &mut state,
                Next::Resume,
                Reached::Nothing,
                Next::Resume,
                Duration::ZERO,
            );
            assert_eq!(next, Next::Resume);
            waits.push(wait.as_secs());
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert!(state.resumable.is_some());
        // Every other RESUME goes to the main gateway once they keep failing.
        assert_eq!(retry.url(&state, Next::Resume, GATEWAY), GATEWAY);
        retry.after(
            &mut state,
            Next::Resume,
            Reached::Nothing,
            Next::Resume,
            Duration::ZERO,
        );
        assert_eq!(
            retry.url(&state, Next::Resume, GATEWAY),
            "wss://resume.example/?v=10&encoding=json"
        );

        // A connection that worked resets the backoff: a reconnect request is
        // followed at once.
        assert_eq!(
            retry.after(
                &mut state,
                Next::Resume,
                Reached::Lasted,
                Next::Resume,
                Duration::ZERO
            ),
            (Next::Resume, Duration::ZERO)
        );
        // One that is closed straight after READY or RESUMED did not last:
        // it backs off like any other failure.
        assert_eq!(
            retry.after(
                &mut state,
                Next::Resume,
                Reached::Established,
                Next::Resume,
                Duration::ZERO
            ),
            (Next::Resume, unit)
        );

        // Connections lost after RESUME went out, before any answer, say
        // nothing about the session: however many, it is kept.
        let mut retry = Retry::new(unit);
        for _ in 0..RESUME_ATTEMPTS * 3 {
            let (next, _) = retry.after(
                &mut state,
                Next::Resume,
                Reached::Introduced,
                Next::Resume,
                Duration::ZERO,
            );
            assert_eq!(next, Next::Resume);
        }
        assert!(state.resumable.is_some());

        // RESUMEs the gateway answers and never takes give the session up.
        let mut retry = Retry::new(unit);
        for _ in 0..RESUME_ATTEMPTS - 1 {
            let (next, _) = retry.after(
                &mut state,
                Next::Resume,
                Reached::Heard,
                Next::Resume,
                Duration::ZERO,
            );
            assert_eq!(next, Next::Resume);
        }
        let (next, wait) = retry.after(
            &mut state,
            Next::Resume,
            Reached::Heard,
            Next::Resume,
            Duration::ZERO,
        );
        assert_eq!(next, Next::Identify);
        assert!(state.resumable.is_none());
        assert_eq!(wait, unit * 4);

        // IDENTIFYs that do not last are spaced out to ten minutes.
        let mut retry = Retry::new(unit);
        let mut waits = Vec::new();
        for _ in 0..9 {
            let (_, wait) = retry.after(
                &mut state,
                Next::Identify,
                Reached::Established,
                Next::Identify,
                Duration::ZERO,
            );
            waits.push(wait.as_secs());
        }
        assert_eq!(waits, [5, 10, 20, 40, 80, 160, 320, 600, 600]);
    }

    async fn accept(listener: &TcpListener) -> WebSocketStream<TcpStream> {
        let (socket, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(socket).await.unwrap()
    }

    async fn put(socket: &mut WebSocketStream<TcpStream>, payload: Value) {
        socket
            .send(Message::text(payload.to_string()))
            .await
            .unwrap();
    }

    /// The next payload with `op`, passing over heartbeats.
    async fn take(socket: &mut WebSocketStream<TcpStream>, op: u64) -> Value {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => {
                    let payload: Value = serde_json::from_str(text.as_str()).unwrap();
                    if payload["op"] == op {
                        return payload;
                    }
                    assert_eq!(payload["op"], 1, "unexpected payload {payload}");
                }
                other => panic!("expected op {op}, got {other:?}"),
            }
        }
    }

    async fn close_with(socket: &mut WebSocketStream<TcpStream>, code: u16) {
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        };
        socket.send(Message::Close(Some(frame))).await.unwrap();
    }

    /// The close code the client ends a connection with.
    async fn closed_by_client(socket: &mut WebSocketStream<TcpStream>) -> Option<u16> {
        while let Some(message) = socket.next().await {
            if let Ok(Message::Close(frame)) = message {
                return frame.map(|frame| u16::from(frame.code));
            }
        }
        None
    }

    fn hello(interval: u64) -> Value {
        json!({"op": 10, "d": {"heartbeat_interval": interval}})
    }

    fn start_against(listener: &TcpListener) -> Gateway {
        let address = listener.local_addr().unwrap();
        start(Config {
            token: "token".into(),
            intents: VOICE_INTENTS,
            optional: OPTIONAL,
            gateway: format!("ws://{address}/?v=10&encoding=json"),
            backoff: Duration::from_millis(50),
        })
    }

    async fn next_dispatch(gateway: &mut Gateway) -> Value {
        match tokio::time::timeout(Duration::from_secs(5), gateway.events.recv())
            .await
            .expect("an event is handed on")
            .expect("the session is still running")
        {
            Event::Dispatch(dispatch) => dispatch,
            other => panic!("expected a dispatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_fake_gateway_sees_resume_and_a_new_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let mut gateway = start_against(&listener);
        let script = async {
            // A new session identifies with every intent.
            let mut first = accept(&listener).await;
            put(&mut first, hello(45_000)).await;
            let identify = take(&mut first, 2).await;
            assert_eq!(identify["d"]["token"], "token");
            assert_eq!(identify["d"]["intents"], VOICE_INTENTS | OPTIONAL);
            put(&mut first, ready("first", &resume_url, 1)).await;
            put(
                &mut first,
                json!({"op": 0, "s": 2, "t": "MESSAGE_CREATE", "d": {}}),
            )
            .await;
            // A reconnect request: the client leaves the session resumable.
            put(&mut first, json!({"op": 7, "d": null})).await;
            assert_eq!(closed_by_client(&mut first).await, Some(RECONNECTING));

            // It comes back on the resume URL and resumes from sequence 2.
            let mut second = accept(&listener).await;
            put(&mut second, hello(45_000)).await;
            let resume = take(&mut second, 6).await;
            assert_eq!(resume["d"]["session_id"], "first");
            assert_eq!(resume["d"]["seq"], 2);
            put(
                &mut second,
                json!({"op": 0, "s": 3, "t": "RESUMED", "d": {}}),
            )
            .await;
            // A timed-out session cannot be resumed: a new one.
            close_with(&mut second, 4009).await;
            let mut third = accept(&listener).await;
            put(&mut third, hello(45_000)).await;
            take(&mut third, 2).await;
            put(&mut third, ready("second", &resume_url, 1)).await;
        };
        tokio::time::timeout(Duration::from_secs(30), script)
            .await
            .expect("the fake gateway script finishes");
        for expected in ["READY", "MESSAGE_CREATE", "RESUMED", "READY"] {
            assert_eq!(next_dispatch(&mut gateway).await["t"], expected);
        }
        assert!(!gateway.task.is_finished());
        gateway.task.abort();
    }

    #[tokio::test]
    async fn commands_wait_until_the_connection_is_ready() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let gateway = start_against(&listener);
        let join = voice_state(NonZeroU64::new(7).unwrap(), NonZeroU64::new(8));
        gateway.commands.send(join.clone()).unwrap();
        let script = async {
            let mut socket = accept(&listener).await;
            put(&mut socket, hello(45_000)).await;
            // IDENTIFY comes first, and nothing but heartbeats until READY.
            take(&mut socket, 2).await;
            let quiet = Instant::now() + Duration::from_millis(300);
            while let Ok(Some(frame)) = tokio::time::timeout_at(quiet, socket.next()).await {
                let Message::Text(text) = frame.unwrap() else {
                    panic!("the connection ended before READY");
                };
                let payload: Value = serde_json::from_str(text.as_str()).unwrap();
                assert_eq!(payload["op"], 1, "sent before READY: {payload}");
            }
            put(&mut socket, ready("s", &resume_url, 1)).await;
            assert_eq!(take(&mut socket, 4).await, join);
        };
        tokio::time::timeout(Duration::from_secs(30), script)
            .await
            .expect("the fake gateway script finishes");
        gateway.task.abort();
    }

    #[test]
    fn a_voice_state_update_replaces_the_earlier_ones_for_its_guild() {
        let guild = NonZeroU64::new(7).unwrap();
        let other = NonZeroU64::new(9).unwrap();
        let join = voice_state(guild, NonZeroU64::new(8));
        let leave = voice_state(guild, None);
        let elsewhere = voice_state(other, NonZeroU64::new(10));
        let presence = json!({"op": 3, "d": {"status": "online"}});
        assert_eq!(
            coalesce(vec![
                join.clone(),
                elsewhere.clone(),
                join.clone(),
                presence.clone(),
                join.clone(),
                leave.clone(),
            ]),
            [elsewhere, presence, leave]
        );
        assert_eq!(coalesce(vec![join.clone()]), [join]);
    }

    /// Joins asked for while no connection is up (a long outage, say) go out
    /// once, the newest, when one is.
    #[tokio::test]
    async fn joins_held_through_an_outage_go_out_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let gateway = start_against(&listener);
        let guild = NonZeroU64::new(7).unwrap();
        for _ in 0..200 {
            gateway
                .commands
                .send(voice_state(guild, NonZeroU64::new(8)))
                .unwrap();
        }
        let newest = voice_state(guild, NonZeroU64::new(11));
        gateway.commands.send(newest.clone()).unwrap();
        let script = async {
            let mut socket = accept(&listener).await;
            put(&mut socket, hello(45_000)).await;
            take(&mut socket, 2).await;
            put(&mut socket, ready("s", &resume_url, 1)).await;
            assert_eq!(take(&mut socket, 4).await, newest);
            // Nothing else follows but heartbeats.
            let quiet = Instant::now() + Duration::from_millis(300);
            while let Ok(Some(frame)) = tokio::time::timeout_at(quiet, socket.next()).await {
                let Message::Text(text) = frame.unwrap() else {
                    panic!("the connection ended");
                };
                let payload: Value = serde_json::from_str(text.as_str()).unwrap();
                assert_eq!(payload["op"], 1, "sent after the newest join: {payload}");
            }
        };
        tokio::time::timeout(Duration::from_secs(30), script)
            .await
            .expect("the fake gateway script finishes");
        gateway.task.abort();
    }

    #[tokio::test]
    async fn a_fake_gateway_that_stops_acknowledging_is_resumed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let gateway = start_against(&listener);
        let script = async {
            let mut first = accept(&listener).await;
            put(&mut first, hello(100)).await;
            take(&mut first, 2).await;
            put(&mut first, ready("quiet", &resume_url, 1)).await;
            // Beats arrive and are never acknowledged, until the client
            // gives the connection up, leaving the session resumable.
            let mut beats = 0;
            let code = loop {
                match first.next().await.unwrap().unwrap() {
                    Message::Text(text) => {
                        let payload: Value = serde_json::from_str(text.as_str()).unwrap();
                        beats += usize::from(payload["op"] == 1);
                    }
                    Message::Close(frame) => break frame.map(|frame| u16::from(frame.code)),
                    _ => {}
                }
            };
            assert!(beats >= 1);
            assert_eq!(code, Some(RECONNECTING));
            let mut second = accept(&listener).await;
            put(&mut second, hello(45_000)).await;
            let resume = take(&mut second, 6).await;
            assert_eq!(resume["d"]["session_id"], "quiet");
            assert_eq!(resume["d"]["seq"], 1);
        };
        tokio::time::timeout(Duration::from_secs(30), script)
            .await
            .expect("the fake gateway script finishes");
        gateway.task.abort();
    }

    /// Discord asks for beats often, and acknowledges each a little late: a
    /// connection that answers every request is never taken for a zombie.
    #[tokio::test]
    async fn answering_heartbeat_requests_is_not_a_zombie() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let gateway = start_against(&listener);
        let script = async {
            let mut socket = accept(&listener).await;
            put(&mut socket, hello(400)).await;
            take(&mut socket, 2).await;
            put(&mut socket, ready("busy", &resume_url, 1)).await;
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until {
                put(&mut socket, json!({"op": 1, "d": null})).await;
                // Answered at once; acknowledged 150 ms later, while every
                // scheduled beat that falls meanwhile passes it by.
                let beat = tokio::time::timeout(Duration::from_secs(1), take(&mut socket, 1))
                    .await
                    .expect("op 1 is answered");
                assert_eq!(beat["op"], 1);
                tokio::time::sleep(Duration::from_millis(150)).await;
                put(&mut socket, json!({"op": 11})).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            // The client never closed: the next frame is whatever it sends
            // next, and it is not a close.
            let quiet = tokio::time::timeout(Duration::from_millis(200), socket.next()).await;
            assert!(
                !matches!(quiet, Ok(Some(Ok(Message::Close(_))))),
                "the connection was taken for a zombie"
            );
        };
        tokio::time::timeout(Duration::from_secs(30), script)
            .await
            .expect("the fake gateway script finishes");
        assert!(!gateway.task.is_finished());
        gateway.task.abort();
    }

    /// Connections that fail before they open, however many, keep the
    /// session: the next one that opens RESUMEs it.
    #[tokio::test]
    async fn transport_failures_keep_the_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let mut gateway = start_against(&listener);
        let script = async {
            let mut first = accept(&listener).await;
            put(&mut first, hello(45_000)).await;
            take(&mut first, 2).await;
            put(&mut first, ready("kept", &resume_url, 1)).await;
            put(
                &mut first,
                json!({"op": 0, "s": 2, "t": "MESSAGE_CREATE", "d": {}}),
            )
            .await;
            drop(first);
            // Five connections that never get as far as a websocket.
            for _ in 0..5 {
                let (socket, _) = listener.accept().await.unwrap();
                drop(socket);
            }
            let mut back = accept(&listener).await;
            put(&mut back, hello(45_000)).await;
            let resume = take(&mut back, 6).await;
            assert_eq!(resume["d"]["session_id"], "kept");
            assert_eq!(resume["d"]["seq"], 2);
        };
        tokio::time::timeout(Duration::from_secs(30), script)
            .await
            .expect("the fake gateway script finishes");
        assert_eq!(next_dispatch(&mut gateway).await["t"], "READY");
        assert_eq!(next_dispatch(&mut gateway).await["t"], "MESSAGE_CREATE");
        gateway.task.abort();
    }

    /// Connections lost after RESUME went out, before any answer (a flapping
    /// uplink, an edge resetting connections), keep the session too.
    #[tokio::test]
    async fn connections_lost_after_resume_keep_the_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let gateway = start_against(&listener);
        let script = async {
            let mut first = accept(&listener).await;
            put(&mut first, hello(45_000)).await;
            take(&mut first, 2).await;
            put(&mut first, ready("kept", &resume_url, 1)).await;
            drop(first);
            for _ in 0..RESUME_ATTEMPTS + 2 {
                let mut flapping = accept(&listener).await;
                put(&mut flapping, hello(45_000)).await;
                assert_eq!(take(&mut flapping, 6).await["d"]["session_id"], "kept");
                drop(flapping);
            }
            let mut back = accept(&listener).await;
            put(&mut back, hello(45_000)).await;
            assert_eq!(take(&mut back, 6).await["d"]["session_id"], "kept");
        };
        tokio::time::timeout(Duration::from_secs(60), script)
            .await
            .expect("the fake gateway script finishes");
        gateway.task.abort();
    }

    /// A connection closed right after READY did not last, so the next one
    /// waits; one that lasted is followed at once.
    #[tokio::test]
    async fn backoff_applies_after_ready() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let gateway = start_against(&listener);
        let script = async {
            let mut first = accept(&listener).await;
            put(&mut first, hello(45_000)).await;
            take(&mut first, 2).await;
            put(&mut first, ready("brief", &resume_url, 1)).await;
            close_with(&mut first, 4000).await;
            let closed = Instant::now();
            let mut second = accept(&listener).await;
            assert!(closed.elapsed() >= Duration::from_millis(40));
            put(&mut second, hello(45_000)).await;
            take(&mut second, 6).await;
            put(
                &mut second,
                json!({"op": 0, "s": 2, "t": "RESUMED", "d": {}}),
            )
            .await;
            close_with(&mut second, 4000).await;
            let closed = Instant::now();
            let mut third = accept(&listener).await;
            assert!(closed.elapsed() >= Duration::from_millis(90));
            // This one works: RESUMED, and a heartbeat is acknowledged.
            put(&mut third, hello(200)).await;
            take(&mut third, 6).await;
            put(
                &mut third,
                json!({"op": 0, "s": 3, "t": "RESUMED", "d": {}}),
            )
            .await;
            take(&mut third, 1).await;
            put(&mut third, json!({"op": 11})).await;
            put(&mut third, json!({"op": 7, "d": null})).await;
            assert_eq!(closed_by_client(&mut third).await, Some(RECONNECTING));
            let closed = Instant::now();
            // Three failures in a row would have waited 200 ms.
            let mut fourth = accept(&listener).await;
            assert!(closed.elapsed() < Duration::from_millis(150));
            put(&mut fourth, hello(45_000)).await;
            take(&mut fourth, 6).await;
        };
        tokio::time::timeout(Duration::from_secs(30), script)
            .await
            .expect("the fake gateway script finishes");
        gateway.task.abort();
    }

    #[tokio::test]
    async fn refused_intents_leave_a_voice_only_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let mut gateway = start_against(&listener);
        let script = async {
            let mut first = accept(&listener).await;
            put(&mut first, hello(45_000)).await;
            let identify = take(&mut first, 2).await;
            assert_eq!(identify["d"]["intents"], VOICE_INTENTS | OPTIONAL);
            close_with(&mut first, 4014).await;
            let mut second = accept(&listener).await;
            put(&mut second, hello(45_000)).await;
            let identify = take(&mut second, 2).await;
            assert_eq!(identify["d"]["intents"], VOICE_INTENTS);
            put(&mut second, ready("voice", &resume_url, 1)).await;
        };
        tokio::time::timeout(Duration::from_secs(30), script)
            .await
            .expect("the fake gateway script finishes");
        let refused = tokio::time::timeout(Duration::from_secs(5), gateway.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(refused, Event::Refused(4014)), "{refused:?}");
        assert_eq!(next_dispatch(&mut gateway).await["t"], "READY");
        assert!(!gateway.task.is_finished());
        gateway.task.abort();
    }

    #[tokio::test]
    async fn an_unresumable_session_identifies_again() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resume_url = format!("ws://{}", listener.local_addr().unwrap());
        let gateway = start_against(&listener);
        let script = async {
            let mut first = accept(&listener).await;
            put(&mut first, hello(45_000)).await;
            take(&mut first, 2).await;
            put(&mut first, ready("old", &resume_url, 1)).await;
            put(&mut first, json!({"op": 9, "d": false})).await;
            assert_eq!(closed_by_client(&mut first).await, Some(RECONNECTING));
            let closed = Instant::now();
            let mut second = accept(&listener).await;
            assert!(closed.elapsed() >= Duration::from_millis(900));
            put(&mut second, hello(45_000)).await;
            take(&mut second, 2).await;
        };
        tokio::time::timeout(Duration::from_secs(30), script)
            .await
            .expect("the fake gateway script finishes");
        gateway.task.abort();
    }

    #[tokio::test]
    async fn a_refused_token_ends_the_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = start_against(&listener);
        let mut socket = accept(&listener).await;
        put(&mut socket, hello(45_000)).await;
        take(&mut socket, 2).await;
        close_with(&mut socket, 4004).await;
        let ended = tokio::time::timeout(Duration::from_secs(30), gateway.task)
            .await
            .expect("the session ends")
            .unwrap();
        assert!(format!("{:#}", ended.unwrap_err()).contains("4004"));
    }
}
