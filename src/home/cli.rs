//! Explicit shell grammar for Home.
use super::{presentation, Home, Request};
use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;
use triblespace::prelude::Id;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "home",
    about = "Kim's lights through the Hue bridge: rooms, lights, scenes, on/off/dim"
)]
pub(crate) struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    /// Existing pile file. Reads and writes never create it.
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable collection signer. Ordinary commands never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    /// Reach the bridge at this address instead of the bound one.
    #[arg(long, value_name = "ADDRESS")]
    bridge: Option<String>,
    /// Open this exact Secrets version for the bridge username instead of the
    /// bound one.
    #[arg(long, value_name = "VERSION")]
    username_secret: Option<String>,
    /// Do not record a light change in the pile; only send it.
    #[arg(long)]
    no_store: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List rooms and zones with their on/off state.
    Rooms,
    /// List lights, or the lights of one room or zone.
    Lights { room: Option<String> },
    /// Turn a room or zone on, optionally at a brightness.
    On {
        room: String,
        /// Hue brightness, 1..=254.
        #[arg(long, conflicts_with = "percent")]
        bri: Option<u8>,
        /// Brightness in percent, 1..=100.
        #[arg(long)]
        percent: Option<u8>,
    },
    /// Turn a room or zone off.
    Off { room: String },
    /// Set a room or zone to a brightness percentage (turning it on).
    Dim { room: String, percent: u8 },
    /// List scenes, or the scenes of one room or zone.
    Scenes { room: Option<String> },
    /// Recall a scene of a room or zone by its name (or scene id).
    Scene { room: String, name: String },
    /// Find Hue bridges: mDNS on the local network, or Signify's cloud
    /// discovery with --cloud.
    Discover {
        #[arg(long)]
        cloud: bool,
        /// How long to listen for mDNS answers.
        #[arg(long, default_value_t = 2000)]
        wait_ms: u64,
    },
    /// Show the bridge binding, or bind one.
    Bridge {
        #[command(subcommand)]
        action: Option<BridgeAction>,
    },
}

#[derive(Subcommand)]
enum BridgeAction {
    /// Bind the bridge at ADDRESS, its username held in Secrets version
    /// SECRET. Reads the bridge's id and proves the username with GETs only.
    Set { address: String, secret: String },
}

fn parse_id(raw: &str, what: &str) -> Result<Id> {
    match Id::from_hex(raw.trim()) {
        Some(id) => Ok(id),
        None => bail!("{what} must be a 32-digit hex id"),
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    if let Command::Discover { cloud, wait_ms } = command {
        let found = Home::discover(cloud, std::time::Duration::from_millis(wait_ms))?;
        return crate::cli::with_output("home", |out| presentation::discovered(&found, out));
    }
    let request = match &command {
        Command::On { bri, percent, .. } => Some(Request::On {
            bri: match (bri, percent) {
                (Some(bri), _) => Some(*bri),
                (None, Some(percent)) => {
                    if !(1..=100).contains(percent) {
                        bail!("percent must be 1..=100");
                    }
                    Some(super::percent_to_bri(*percent))
                }
                (None, None) => None,
            },
        }),
        Command::Off { .. } => Some(Request::Off),
        Command::Dim { percent, .. } => Some(Request::Dim { percent: *percent }),
        Command::Scene { name, .. } => Some(Request::Scene { name: name.clone() }),
        _ => None,
    };
    if let Some(request) = &request {
        request.validate()?;
    }
    let username_secret = cli
        .username_secret
        .as_deref()
        .map(|raw| parse_id(raw, "--username-secret"))
        .transpose()?;
    let storage = cli.workspace.storage(cli.pile, cli.key)?;
    storage.scope_to_exit(|storage| {
        let mut home = Home::with_storage(storage.clone());
        if let Some(address) = &cli.bridge {
            home = home.with_address(address.clone());
        }
        if let Some(secret) = username_secret {
            home = home.with_secret(secret);
        }
        crate::cli::with_output("home", |out| match command {
            Command::Rooms => presentation::rooms(&home.rooms()?, out),
            Command::Lights { room } => presentation::lights(&home.lights(room.as_deref())?, out),
            Command::Scenes { room } => presentation::scenes(&home.scenes(room.as_deref())?, out),
            Command::On { room, .. }
            | Command::Off { room }
            | Command::Dim { room, .. }
            | Command::Scene { room, .. } => {
                let request = request.as_ref().expect("a light change");
                let report = home.change(&room, request)?;
                presentation::action(&report, out)?;
                if !cli.no_store {
                    let action = home.record(&report)?;
                    out.line(format!("recorded [{action:x}]"))?;
                }
                if report.refused() {
                    bail!("the bridge refused the change");
                }
                Ok(())
            }
            Command::Bridge { action: None } => presentation::status(&home.bridge_status()?, out),
            Command::Bridge {
                action: Some(BridgeAction::Set { address, secret }),
            } => {
                let secret = parse_id(&secret, "SECRET")?;
                presentation::binding(&home.bridge_set(&address, secret)?, out)
            }
            Command::Discover { .. } => unreachable!("handled before the pile"),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bri_and_percent_conflict() {
        let parsed = Cli::try_parse_from([
            "home",
            "--pile",
            "x.pile",
            "on",
            "Living room",
            "--bri",
            "3",
            "--percent",
            "4",
        ]);
        assert!(parsed.is_err());
    }
}
