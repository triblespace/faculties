//! `world` CLI: verbs become inputs, observations become text.

use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use super::render;
use crate::out::Out;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "world",
    about = "Live in a Clawblox-protocol world: perceive as text, act through verbs"
)]
pub struct Cli {
    /// World server base URL.
    #[arg(long, env = "WORLD_URL", default_value = "http://localhost:8080")]
    url: String,
    /// File holding this body's session token, written by `join`.
    #[arg(long, env = "WORLD_SESSION_FILE")]
    session_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enter the world (or re-enter with the saved session) and look around.
    Join {
        /// Name other players see.
        #[arg(long, default_value = "agent")]
        name: String,
    },
    /// Describe what the body perceives right now.
    Look,
    /// Walk toward a point (x z, optional --y) or the nearest entity with a
    /// name; `Block` also matches `Block_7`.
    MoveTo {
        /// Either `<x> <z>` or one entity name.
        #[arg(allow_negative_numbers = true, required = true, num_args = 1..=2)]
        target: Vec<String>,
        /// Height for a coordinate target. Default: the body's current height.
        #[arg(long)]
        y: Option<f64>,
        /// Give up after this many seconds. Long walks are repeated short moves.
        #[arg(long, default_value_t = 5.0)]
        timeout: f64,
    },
    /// Pick up the nearest entity with a name (or the thing at `<x> <z>`).
    Pickup {
        #[arg(allow_negative_numbers = true, required = true, num_args = 1..=2)]
        target: Vec<String>,
        /// Height for a coordinate target. Default: the body's current height.
        #[arg(long)]
        y: Option<f64>,
    },
    /// Put down what the body holds at `<x> <z>`.
    Place {
        #[arg(allow_negative_numbers = true, required = true, num_args = 2)]
        target: Vec<String>,
        /// Height. Default: the body's current height.
        #[arg(long)]
        y: Option<f64>,
    },
    /// Throw what the body holds along the horizontal direction `<dx> <dz>`.
    Throw {
        #[arg(allow_negative_numbers = true, required = true, num_args = 2)]
        direction: Vec<f64>,
        /// Upward part of the direction.
        #[arg(long, default_value_t = 0.3, allow_negative_numbers = true)]
        dy: f64,
        #[arg(long, default_value_t = 12.0)]
        speed: f64,
    },
    /// Send any input this world defines: `act <Type> [json-data]`.
    Act {
        kind: String,
        /// The input's data as JSON. Default: `{}`.
        data: Option<String>,
    },
    /// Jump once.
    Jump,
    /// Stop moving.
    Stop,
    /// Say something in the world's chat.
    Say { text: Vec<String> },
    /// The sense: emit the observation whenever it changes, until killed.
    Watch {
        /// Seconds between looks; also the shortest gap between two emissions.
        #[arg(long, default_value_t = 2.0)]
        every: f64,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    crate::cli::with_output("world", |out| execute(cli, out))
}

struct World {
    url: String,
    session_file: PathBuf,
    http: reqwest::blocking::Client,
}

impl World {
    fn new(url: String, session_file: Option<PathBuf>) -> Result<Self> {
        let session_file = match session_file {
            Some(path) => path,
            None => {
                let base = std::env::var_os("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state"))
                    })
                    .context("no XDG_STATE_HOME or HOME for the session file")?;
                base.join("world").join("session")
            }
        };
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            url: url.trim_end_matches('/').to_owned(),
            session_file,
            http,
        })
    }

    fn session(&self) -> Result<String> {
        let token = std::fs::read_to_string(&self.session_file).with_context(|| {
            format!(
                "no session at {}; run `world join` first",
                self.session_file.display()
            )
        })?;
        Ok(token.trim().to_owned())
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

    fn join(&self, name: &str) -> Result<Value> {
        let mut request = self
            .http
            .post(format!("{}/join", self.url))
            .query(&[("name", name)]);
        if let Ok(token) = self.session() {
            request = request.header("X-Session", token);
        }
        let reply: Value = Self::check(request.send()?, "join")?.json()?;
        let token = reply
            .get("session")
            .and_then(Value::as_str)
            .context("join reply carried no session")?;
        if let Some(dir) = self.session_file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.session_file, token)?;
        Ok(reply)
    }

    fn observe(&self) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}/observe", self.url))
            .header("X-Session", self.session()?)
            .send()?;
        Ok(Self::check(response, "observe")?.json()?)
    }

    fn input(&self, kind: &str, data: Value) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/input", self.url))
            .header("X-Session", self.session()?)
            .json(&json!({"type": kind, "data": data}))
            .send()?;
        Self::check(response, kind)?;
        Ok(())
    }

    fn chat(&self, text: &str) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/chat", self.url))
            .header("X-Session", self.session()?)
            .json(&json!({"content": text}))
            .send()?;
        Self::check(response, "chat")?;
        Ok(())
    }
}

fn execute(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let world = World::new(cli.url, cli.session_file)?;
    match cli.command {
        Command::Join { name } => {
            let reply = world.join(&name)?;
            let id = reply
                .get("agent_id")
                .map(|v| v.to_string())
                .unwrap_or_default();
            out.line(format!("joined as {name} {id}").trim_end().to_owned())?;
            out.text(render::observation(&world.observe()?))
        }
        Command::Look => out.text(render::observation(&world.observe()?)),
        Command::MoveTo { target, y, timeout } => move_to(&world, out, &target, y, timeout),
        Command::Pickup { target, y } => {
            let (label, at) = resolve_target(&world, &target, y)?;
            act(
                &world,
                out,
                "Pickup",
                json!({"position": at}),
                &format!("pickup {label}"),
            )
        }
        Command::Place { target, y } => {
            let (label, at) = resolve_target(&world, &target, y)?;
            act(
                &world,
                out,
                "Place",
                json!({"position": at}),
                &format!("place at {label}"),
            )
        }
        Command::Throw {
            direction,
            dy,
            speed,
        } => act(
            &world,
            out,
            "Throw",
            json!({"direction": [direction[0], dy, direction[1]], "speed": speed}),
            "throw",
        ),
        Command::Act { kind, data } => {
            let data: Value = match data {
                Some(text) => serde_json::from_str(&text).context("act data is not JSON")?,
                None => json!({}),
            };
            act(&world, out, &kind, data, &kind)
        }
        Command::Jump => {
            world.input("Jump", json!({}))?;
            out.line("jumped")?;
            out.text(render::observation(&world.observe()?))
        }
        Command::Stop => {
            world.input("Stop", json!({}))?;
            out.line("stopped")?;
            out.text(render::observation(&world.observe()?))
        }
        Command::Say { text } => {
            let text = text.join(" ");
            if text.trim().is_empty() {
                bail!("say what?");
            }
            world.chat(&text)?;
            out.line("said it")
        }
        Command::Watch { every } => watch(&world, out, every),
    }
}

/// Send one input, then report what the world says became of it.
fn act(world: &World, out: &mut Out<'_>, kind: &str, data: Value, label: &str) -> Result<()> {
    world.input(kind, data)?;
    let obs = world.observe()?;
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

fn resolve_target(world: &World, target: &[String], y: Option<f64>) -> Result<(String, [f64; 3])> {
    let obs = world.observe()?;
    let me = render::own_position(&obs).context("the observation has no own position")?;
    match target {
        [x, z] if x.parse::<f64>().is_ok() && z.parse::<f64>().is_ok() => {
            let p = [x.parse()?, y.unwrap_or(me[1]), z.parse()?];
            Ok((format!("({:.1}, {:.1})", p[0], p[2]), p))
        }
        [name] => {
            let entities = obs
                .get("world")
                .and_then(|w| w.get("entities"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let best = entities
                .iter()
                .filter(|e| {
                    e.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n == name || render::base_name(n) == name)
                })
                .filter_map(|e| render::vec3(e.get("position")?))
                .min_by(|a, b| render::distance(me, *a).total_cmp(&render::distance(me, *b)));
            match best {
                Some(p) => Ok((name.clone(), p)),
                None => bail!("no {name} in sight"),
            }
        }
        _ => bail!("move-to takes `<x> <z>` or one entity name"),
    }
}

fn move_to(
    world: &World,
    out: &mut Out<'_>,
    target: &[String],
    y: Option<f64>,
    timeout: f64,
) -> Result<()> {
    let (label, goal) = resolve_target(world, target, y)?;
    world.input("MoveTo", json!({"position": goal}))?;
    let started = Instant::now();
    let mut last = None;
    let mut still_since = Instant::now();
    let outcome = loop {
        sleep(Duration::from_millis(250));
        let obs = world.observe()?;
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
    let (verdict, obs) = outcome;
    out.line(format!("{verdict} {label}"))?;
    out.text(render::observation(&obs))
}

fn watch(world: &World, out: &mut Out<'_>, every: f64) -> Result<()> {
    let every = Duration::from_secs_f64(every.max(0.5));
    let mut previous: Option<String> = None;
    loop {
        let obs = world.observe()?;
        let body = render::body(&obs);
        if previous.as_deref() != Some(body.as_str()) {
            out.text(render::observation(&obs))?;
            previous = Some(body);
        }
        sleep(every);
    }
}
