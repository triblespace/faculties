//! World operations shared by the CLI and MCP frontends: one HTTP client of a
//! Clawblox-protocol world, every verb writing its observation as text.

use std::path::PathBuf;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::render;
use crate::out::Out;

/// The world a launcher configures when it names none.
pub const DEFAULT_URL: &str = "http://localhost:8080";

/// Where this body's session token lives. The CLI keeps it in a file so
/// separate invocations are one body; a long-lived MCP server may keep it in
/// memory, or share the CLI's file when its launcher names one.
pub enum Session {
    File(PathBuf),
    Memory(Mutex<Option<String>>),
}

impl Session {
    pub fn memory() -> Self {
        Self::Memory(Mutex::new(None))
    }

    fn get(&self) -> Result<String> {
        match self {
            Self::File(path) => Ok(std::fs::read_to_string(path)
                .with_context(|| format!("no session at {}; join the world first", path.display()))?
                .trim()
                .to_owned()),
            Self::Memory(token) => token
                .lock()
                .expect("world session mutex")
                .clone()
                .context("no session yet; join the world first"),
        }
    }

    fn set(&self, token: &str) -> Result<()> {
        match self {
            Self::File(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(path, token)?;
            }
            Self::Memory(slot) => *slot.lock().expect("world session mutex") = Some(token.into()),
        }
        Ok(())
    }
}

/// Where to go: a ground point (height defaults to the body's own) or the
/// nearest entity whose name or base name matches (`Block` finds `Block_7`).
pub enum Target {
    Point { x: f64, z: f64, y: Option<f64> },
    Entity(String),
}

pub struct World<'a> {
    url: String,
    session: &'a Session,
    http: reqwest::blocking::Client,
}

impl<'a> World<'a> {
    pub fn new(url: &str, session: &'a Session) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            url: url.trim_end_matches('/').to_owned(),
            session,
            http,
        })
    }

    fn check(
        response: reqwest::blocking::Response,
        what: &str,
    ) -> Result<reqwest::blocking::Response> {
        let status = response.status();
        if !status.is_success() {
            let body: String = response
                .text()
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            bail!("{what} failed: HTTP {status}: {body}");
        }
        Ok(response)
    }

    fn observe(&self) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}/observe", self.url))
            .header("X-Session", self.session.get()?)
            .send()?;
        Ok(Self::check(response, "observe")?.json()?)
    }

    fn input(&self, kind: &str, data: Value) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/input", self.url))
            .header("X-Session", self.session.get()?)
            .json(&json!({"type": kind, "data": data}))
            .send()?;
        Self::check(response, kind)?;
        Ok(())
    }

    /// Enter the world, or re-enter with the kept session, and look around.
    pub fn join(&self, name: &str, out: &mut Out<'_>) -> Result<()> {
        let mut request = self
            .http
            .post(format!("{}/join", self.url))
            .query(&[("name", name)]);
        if let Ok(token) = self.session.get() {
            request = request.header("X-Session", token);
        }
        let reply: Value = Self::check(request.send()?, "join")?.json()?;
        let token = reply
            .get("session")
            .and_then(Value::as_str)
            .context("join reply carried no session")?;
        self.session.set(token)?;
        let id = reply
            .get("agent_id")
            .map(|v| v.to_string())
            .unwrap_or_default();
        out.line(format!("joined as {name} {id}").trim_end().to_owned())?;
        out.text(render::observation(&self.observe()?))
    }

    pub fn look(&self, out: &mut Out<'_>) -> Result<()> {
        out.text(render::observation(&self.observe()?))
    }

    fn resolve(&self, target: &Target) -> Result<(String, [f64; 3])> {
        let obs = self.observe()?;
        let me = render::own_position(&obs).context("the observation has no own position")?;
        match target {
            Target::Point { x, z, y } => {
                Ok((format!("({x:.1}, {z:.1})"), [*x, y.unwrap_or(me[1]), *z]))
            }
            Target::Entity(name) => {
                let entities = obs
                    .get("world")
                    .and_then(|w| w.get("entities"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                entities
                    .iter()
                    .filter(|e| {
                        e.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|n| n == name || render::base_name(n) == name)
                    })
                    .filter_map(|e| render::vec3(e.get("position")?))
                    .min_by(|a, b| render::distance(me, *a).total_cmp(&render::distance(me, *b)))
                    .map(|p| (name.clone(), p))
                    .with_context(|| format!("no {name} in sight"))
            }
        }
    }

    /// Walk toward a target, polling until arrived, stuck, or `timeout`
    /// seconds. Long walks are repeated short moves.
    pub fn move_to(&self, target: &Target, timeout: f64, out: &mut Out<'_>) -> Result<()> {
        let (label, goal) = self.resolve(target)?;
        self.input("MoveTo", json!({"position": goal}))?;
        let started = Instant::now();
        let mut last = None;
        let mut still_since = Instant::now();
        let (verdict, obs) = loop {
            sleep(Duration::from_millis(250));
            let obs = self.observe()?;
            let Some(here) = render::own_position(&obs) else {
                break ("lost track of myself", obs);
            };
            if render::ground_distance(here, goal) < 2.0 {
                break ("arrived at", obs);
            }
            if last.is_some_and(|p| render::distance(p, here) > 0.05) {
                still_since = Instant::now();
            }
            last = Some(here);
            if still_since.elapsed() > Duration::from_secs(1)
                && started.elapsed() > Duration::from_millis(750)
            {
                break ("stuck on the way to", obs);
            }
            if started.elapsed().as_secs_f64() > timeout {
                break ("still walking toward", obs);
            }
        };
        out.line(format!("{verdict} {label}"))?;
        out.text(render::observation(&obs))
    }

    /// Send one input, then report what the world made of it, read back from
    /// the body's LastActionResult when the world keeps one.
    pub fn act(&self, kind: &str, data: Value, label: &str, out: &mut Out<'_>) -> Result<()> {
        self.input(kind, data)?;
        let obs = self.observe()?;
        let result = obs
            .get("player")
            .and_then(|p| p.get("attributes"))
            .and_then(|a| a.get("LastActionResult"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        match result {
            Some(result) => out.line(format!("{label}: {result}"))?,
            None => out.line(format!("{label}: sent"))?,
        }
        out.text(render::observation(&obs))
    }

    pub fn pickup(&self, target: &Target, out: &mut Out<'_>) -> Result<()> {
        let (label, at) = self.resolve(target)?;
        self.act(
            "Pickup",
            json!({"position": at}),
            &format!("pickup {label}"),
            out,
        )
    }

    pub fn place(&self, target: &Target, out: &mut Out<'_>) -> Result<()> {
        let (label, at) = self.resolve(target)?;
        self.act(
            "Place",
            json!({"position": at}),
            &format!("place at {label}"),
            out,
        )
    }

    pub fn throw(&self, dx: f64, dz: f64, dy: f64, speed: f64, out: &mut Out<'_>) -> Result<()> {
        self.act(
            "Throw",
            json!({"direction": [dx, dy, dz], "speed": speed}),
            "throw",
            out,
        )
    }

    pub fn jump(&self, out: &mut Out<'_>) -> Result<()> {
        self.input("Jump", json!({}))?;
        out.line("jumped")?;
        self.look(out)
    }

    pub fn stop(&self, out: &mut Out<'_>) -> Result<()> {
        self.input("Stop", json!({}))?;
        out.line("stopped")?;
        self.look(out)
    }

    pub fn say(&self, text: &str, out: &mut Out<'_>) -> Result<()> {
        if text.trim().is_empty() {
            bail!("say what?");
        }
        let response = self
            .http
            .post(format!("{}/chat", self.url))
            .header("X-Session", self.session.get()?)
            .json(&json!({"content": text}))
            .send()?;
        Self::check(response, "chat")?;
        out.line("said it")
    }

    /// The sense: emit the observation whenever it changes, until killed.
    /// Never ends on its own, so no request/response frontend offers it.
    pub fn watch(&self, every: f64, out: &mut Out<'_>) -> Result<()> {
        let every = Duration::from_secs_f64(every.max(0.5));
        let mut previous: Option<String> = None;
        loop {
            let obs = self.observe()?;
            let body = render::body(&obs);
            if previous.as_deref() != Some(body.as_str()) {
                out.text(render::observation(&obs))?;
                previous = Some(body);
            }
            sleep(every);
        }
    }
}
