//! Literal finite Home operations; the bridge comes from the pile's binding.
use super::{presentation, Home as Operations, Request};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

macro_rules! schema {
    ($properties:expr, $required:expr) => {
        concat!(
            r#"{"type":"object","properties":{"#,
            $properties,
            r#"},"required":["#,
            $required,
            r#"],"additionalProperties":false}"#
        )
    };
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "home_rooms",
        description: "List the Hue bridge's rooms and zones with their live on/off state and brightness. Reads the bridge; records nothing.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    Tool {
        name: "home_lights",
        description: "List lights with their live state, optionally only those of one room or zone. Reads the bridge; records nothing.",
        input_schema: schema!(
            r#""room":{"type":"string","description":"Hue room or zone name (case-insensitive) or group id"}"#,
            ""
        ),
    },
    Tool {
        name: "home_on",
        description: "Turn a room or zone on, optionally at a brightness (bri 1-254 or percent 1-100, not both). Changes real lights. Records the action unless record=false.",
        input_schema: schema!(
            r#""room":{"type":"string","description":"Hue room or zone name (case-insensitive) or group id"},"bri":{"type":"integer","minimum":1,"maximum":254},"percent":{"type":"integer","minimum":1,"maximum":100},"record":{"type":"boolean","default":true}"#,
            r#""room""#
        ),
    },
    Tool {
        name: "home_off",
        description: "Turn a room or zone off. Changes real lights. Records the action unless record=false.",
        input_schema: schema!(
            r#""room":{"type":"string","description":"Hue room or zone name (case-insensitive) or group id"},"record":{"type":"boolean","default":true}"#,
            r#""room""#
        ),
    },
    Tool {
        name: "home_alert",
        description: "Make a room's or zone's lights breathe for about fifteen seconds, then return to their state (the Hue group alert). Signals real lights. Records the action unless record=false.",
        input_schema: schema!(
            r#""room":{"type":"string","description":"Hue room or zone name (case-insensitive) or group id"},"record":{"type":"boolean","default":true}"#,
            r#""room""#
        ),
    },
    Tool {
        name: "home_dim",
        description: "Set a room or zone to a brightness percentage (1-100), turning it on. Changes real lights. Records the action unless record=false.",
        input_schema: schema!(
            r#""room":{"type":"string","description":"Hue room or zone name (case-insensitive) or group id"},"percent":{"type":"integer","minimum":1,"maximum":100},"record":{"type":"boolean","default":true}"#,
            r#""room","percent""#
        ),
    },
    Tool {
        name: "home_scenes",
        description: "List Hue scenes, optionally only those of one room or zone. Reads the bridge; records nothing.",
        input_schema: schema!(
            r#""room":{"type":"string","description":"Hue room or zone name (case-insensitive) or group id"}"#,
            ""
        ),
    },
    Tool {
        name: "home_scene",
        description: "Recall a scene of a room or zone by scene name (case-insensitive) or scene id. Changes real lights. Records the action unless record=false.",
        input_schema: schema!(
            r#""room":{"type":"string","description":"Hue room or zone name (case-insensitive) or group id"},"scene":{"type":"string"},"record":{"type":"boolean","default":true}"#,
            r#""room","scene""#
        ),
    },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Optional {
    #[serde(default)]
    room: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct On {
    room: String,
    #[serde(default)]
    bri: Option<u8>,
    #[serde(default)]
    percent: Option<u8>,
    #[serde(default = "record_default")]
    record: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Off {
    room: String,
    #[serde(default = "record_default")]
    record: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Dim {
    room: String,
    percent: u8,
    #[serde(default = "record_default")]
    record: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneArgs {
    room: String,
    scene: String,
    #[serde(default = "record_default")]
    record: bool,
}

fn record_default() -> bool {
    true
}

pub struct Home {
    operations: Operations,
}

impl Home {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: Operations::with_storage(storage),
        }
    }
    /// Trusted Rust/launcher injection, not request-controlled routing.
    pub fn from_operations(operations: Operations) -> Self {
        Self { operations }
    }

    fn change(&self, room: &str, request: Request, record: bool, out: &mut Out<'_>) -> Result<()> {
        request.validate().map_err(invalid_arguments)?;
        let report = self.operations.change(room, &request)?;
        presentation::action(&report, out)?;
        if record {
            let action = self.operations.record(&report)?;
            out.line(format!("recorded [{action:x}]"))?;
        }
        if report.refused() {
            bail!("the bridge refused the change");
        }
        Ok(())
    }
}

impl Faculty for Home {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }

    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "home_rooms" => {
                let Empty {} = decode_arguments(arguments)?;
                presentation::rooms(&self.operations.rooms()?, out)
            }
            "home_lights" => {
                let args: Optional = decode_arguments(arguments)?;
                presentation::lights(&self.operations.lights(args.room.as_deref())?, out)
            }
            "home_scenes" => {
                let args: Optional = decode_arguments(arguments)?;
                presentation::scenes(&self.operations.scenes(args.room.as_deref())?, out)
            }
            "home_on" => {
                let args: On = decode_arguments(arguments)?;
                let bri = match (args.bri, args.percent) {
                    (Some(_), Some(_)) => {
                        return Err(invalid_arguments("give bri or percent, not both"))
                    }
                    (Some(bri), None) => Some(bri),
                    (None, Some(percent)) if (1..=100).contains(&percent) => {
                        Some(super::percent_to_bri(percent))
                    }
                    (None, Some(_)) => return Err(invalid_arguments("percent must be 1..=100")),
                    (None, None) => None,
                };
                self.change(&args.room, Request::On { bri }, args.record, out)
            }
            "home_off" => {
                let args: Off = decode_arguments(arguments)?;
                self.change(&args.room, Request::Off, args.record, out)
            }
            "home_alert" => {
                let args: Off = decode_arguments(arguments)?;
                self.change(&args.room, Request::Alert, args.record, out)
            }
            "home_dim" => {
                let args: Dim = decode_arguments(arguments)?;
                self.change(
                    &args.room,
                    Request::Dim {
                        percent: args.percent,
                    },
                    args.record,
                    out,
                )
            }
            "home_scene" => {
                let args: SceneArgs = decode_arguments(arguments)?;
                self.change(
                    &args.room,
                    Request::Scene { name: args.scene },
                    args.record,
                    out,
                )
            }
            _ => bail!("Home MCP has no tool {name:?}"),
        }
    }
}
