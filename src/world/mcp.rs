//! World MCP tools: the CLI's verbs as typed calls, observations as text.
//!
//! The world URL and an optional session file are launcher configuration,
//! never tool arguments. Without a session file the body's session lives in
//! this server's memory, so one server is one body until it restarts.
//! `watch` is not offered: it streams until killed, and a tool call must end.

use super::operations::{Session, Target, World, DEFAULT_URL};
use crate::mcp::{decode_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

const TOOLS: &[Tool] = &[
    Tool {
        name: "world_join",
        description: "Enter the configured world (or re-enter with this server's session) and return what the body perceives, as text.",
        input_schema: r#"{"type":"object","properties":{"name":{"type":"string","minLength":1,"default":"agent"}},"additionalProperties":false}"#,
    },
    Tool {
        name: "world_look",
        description: "Describe what the body perceives right now: status, position, facing, the nearest things with distance and offsets, recent events. Plain text.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    Tool {
        name: "world_move_to",
        description: "Walk toward an entity (by name) or a ground point (x, z), polling until arrived, stuck, or timeout seconds (default 5). Long walks are repeated short moves. Returns the verdict and the observation.",
        input_schema: r#"{"type":"object","properties":{"entity":{"type":"string","minLength":1,"description":"Nearest entity with this name or base name (Block matches Block_7)."},"x":{"type":"number"},"z":{"type":"number"},"y":{"type":"number","description":"Height for a point target; default the body's own."},"timeout":{"type":"number","exclusiveMinimum":0,"default":5}},"additionalProperties":false}"#,
    },
    Tool {
        name: "world_pickup",
        description: "Pick up the nearest entity with a name, or the thing at a point (x, z). Reports the world's own result (e.g. PickupSuccess, PickupFailed:OutOfRange) and the observation.",
        input_schema: r#"{"type":"object","properties":{"entity":{"type":"string","minLength":1,"description":"Nearest entity with this name or base name (Block matches Block_7)."},"x":{"type":"number"},"z":{"type":"number"},"y":{"type":"number","description":"Height for a point target; default the body's own."}},"additionalProperties":false}"#,
    },
    Tool {
        name: "world_place",
        description: "Put down what the body holds at a point (x, z). Reports the world's own result and the observation.",
        input_schema: r#"{"type":"object","properties":{"x":{"type":"number"},"z":{"type":"number"},"y":{"type":"number"}},"required":["x","z"],"additionalProperties":false}"#,
    },
    Tool {
        name: "world_throw",
        description: "Throw what the body holds along the horizontal direction (dx, dz), with upward part dy (default 0.3) and speed (default 12). Reports the world's own result and the observation.",
        input_schema: r#"{"type":"object","properties":{"dx":{"type":"number"},"dz":{"type":"number"},"dy":{"type":"number","default":0.3},"speed":{"type":"number","exclusiveMinimum":0,"default":12}},"required":["dx","dz"],"additionalProperties":false}"#,
    },
    Tool {
        name: "world_jump",
        description: "Jump once, then return the observation.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    Tool {
        name: "world_stop",
        description: "Stop moving, then return the observation.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    Tool {
        name: "world_say",
        description: "Say something in the world's chat.",
        input_schema: r#"{"type":"object","properties":{"text":{"type":"string","minLength":1}},"required":["text"],"additionalProperties":false}"#,
    },
    Tool {
        name: "world_act",
        description: "Send any input this world defines (type plus a JSON data object) and report the world's own result, for actions the other tools do not name.",
        input_schema: r#"{"type":"object","properties":{"type":{"type":"string","minLength":1},"data":{"type":"object","default":{}}},"required":["type"],"additionalProperties":false}"#,
    },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinArguments {
    #[serde(default = "default_name")]
    name: String,
}

fn default_name() -> String {
    "agent".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArguments {
    entity: Option<String>,
    x: Option<f64>,
    z: Option<f64>,
    y: Option<f64>,
    #[serde(default = "default_timeout")]
    timeout: f64,
}

fn default_timeout() -> f64 {
    5.0
}

impl TargetArguments {
    fn target(&self) -> Result<Target> {
        match (&self.entity, self.x, self.z) {
            (Some(name), None, None) => Ok(Target::Entity(name.clone())),
            (None, Some(x), Some(z)) => Ok(Target::Point { x, z, y: self.y }),
            _ => bail!("give either entity, or both x and z"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaceArguments {
    x: f64,
    z: f64,
    y: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThrowArguments {
    dx: f64,
    dz: f64,
    #[serde(default = "default_dy")]
    dy: f64,
    #[serde(default = "default_speed")]
    speed: f64,
}

fn default_dy() -> f64 {
    0.3
}

fn default_speed() -> f64 {
    12.0
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SayArguments {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActArguments {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default = "empty_object")]
    data: Value,
}

fn empty_object() -> Value {
    json!({})
}

pub struct WorldTools {
    url: String,
    session: Session,
}

impl WorldTools {
    /// Construction stores configuration only; nothing is contacted or created.
    pub fn new(url: Option<String>, session_file: Option<PathBuf>) -> Self {
        Self {
            url: url.unwrap_or_else(|| DEFAULT_URL.into()),
            session: match session_file {
                Some(path) => Session::File(path),
                None => Session::memory(),
            },
        }
    }
}

impl Faculty for WorldTools {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }

    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        // Decode before contacting the world.
        match name {
            "world_join" => {
                let args: JoinArguments = decode_arguments(arguments)?;
                World::new(&self.url, &self.session)?.join(&args.name, out)
            }
            "world_look" | "world_jump" | "world_stop" => {
                let NoArguments {} = decode_arguments(arguments)?;
                let world = World::new(&self.url, &self.session)?;
                match name {
                    "world_look" => world.look(out),
                    "world_jump" => world.jump(out),
                    _ => world.stop(out),
                }
            }
            "world_move_to" => {
                let args: TargetArguments = decode_arguments(arguments)?;
                let target = args.target()?;
                if !(args.timeout > 0.0) {
                    bail!("timeout must be positive");
                }
                World::new(&self.url, &self.session)?.move_to(&target, args.timeout, out)
            }
            "world_pickup" => {
                let args: TargetArguments = decode_arguments(arguments)?;
                let target = args.target()?;
                World::new(&self.url, &self.session)?.pickup(&target, out)
            }
            "world_place" => {
                let args: PlaceArguments = decode_arguments(arguments)?;
                let target = Target::Point {
                    x: args.x,
                    z: args.z,
                    y: args.y,
                };
                World::new(&self.url, &self.session)?.place(&target, out)
            }
            "world_throw" => {
                let args: ThrowArguments = decode_arguments(arguments)?;
                World::new(&self.url, &self.session)?
                    .throw(args.dx, args.dz, args.dy, args.speed, out)
            }
            "world_say" => {
                let args: SayArguments = decode_arguments(arguments)?;
                World::new(&self.url, &self.session)?.say(&args.text, out)
            }
            "world_act" => {
                let args: ActArguments = decode_arguments(arguments)?;
                if !args.data.is_object() {
                    bail!("data must be a JSON object");
                }
                World::new(&self.url, &self.session)?.act(&args.kind, args.data, &args.kind, out)
            }
            other => bail!("World MCP has no tool {other:?}"),
        }
    }
}
