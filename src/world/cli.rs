//! `world` CLI: verbs become inputs, observations become text.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use super::operations::{Session, Target, World, DEFAULT_URL};
use crate::out::Out;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "world",
    about = "Live in a Clawblox-protocol world: perceive as text, act through verbs"
)]
pub struct Cli {
    /// World server base URL.
    #[arg(long, env = "WORLD_URL", default_value = DEFAULT_URL)]
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

fn session_file(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .context("no XDG_STATE_HOME or HOME for the session file")?;
    Ok(base.join("world").join("session"))
}

fn target(words: &[String], y: Option<f64>) -> Result<Target> {
    match words {
        [x, z] => Ok(Target::Point {
            x: x.parse()
                .with_context(|| format!("{x:?} is not a coordinate"))?,
            z: z.parse()
                .with_context(|| format!("{z:?} is not a coordinate"))?,
            y,
        }),
        [name] => Ok(Target::Entity(name.clone())),
        _ => bail!("a target is `<x> <z>` or one entity name"),
    }
}

fn execute(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let session = Session::File(session_file(cli.session_file)?);
    let world = World::new(&cli.url, &session)?;
    match cli.command {
        Command::Join { name } => world.join(&name, out),
        Command::Look => world.look(out),
        Command::MoveTo {
            target: t,
            y,
            timeout,
        } => world.move_to(&target(&t, y)?, timeout, out),
        Command::Pickup { target: t, y } => world.pickup(&target(&t, y)?, out),
        Command::Place { target: t, y } => world.place(&target(&t, y)?, out),
        Command::Throw {
            direction,
            dy,
            speed,
        } => world.throw(direction[0], direction[1], dy, speed, out),
        Command::Act { kind, data } => {
            let data: Value = match data {
                Some(text) => serde_json::from_str(&text).context("act data is not JSON")?,
                None => json!({}),
            };
            world.act(&kind, data, &kind, out)
        }
        Command::Jump => world.jump(out),
        Command::Stop => world.stop(out),
        Command::Say { text } => world.say(&text.join(" "), out),
        Command::Watch { every } => world.watch(every, out),
    }
}
