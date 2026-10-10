//! Explicit finite Discord MCP operations; authentication resolves through Secrets.
use super::{operations::*, render};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

const TOOLS: &[Tool] = &[
    Tool { name: "discord_auth_status", description: "Show the workspace's Discord authentication revision and exact encrypted Secrets references. Never opens or returns credential plaintext.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"# },
    Tool { name: "discord_auth_set", description: "Bind an existing exact encrypted Secrets version as this workspace's Discord bot credential, reconciling observed auth revisions. Requires access to that version and Discord collection WRITE; never imports plaintext or grants authority.",
        input_schema: r#"{"type":"object","properties":{"secret_version":{"type":"string","description":"Exact 32-character immutable Secrets version id; never a token or secret name."}},"required":["secret_version"],"additionalProperties":false}"# },
    Tool { name: "discord_read", description: "Read resident Discord observations, including divergent latest versions. No token or network access required; call discord_pull explicitly for fresh data.",
        input_schema: r#"{"type":"object","properties":{"channel_id":{"type":"string"},"since":{"type":"string","description":"RFC3339 timestamp."},"limit":{"type":"integer","minimum":0,"default":20,"description":"Newest N messages; 0 means unlimited."},"descending":{"type":"boolean","default":false}},"additionalProperties":false}"# },
    Tool { name: "discord_pull", description: "Pull a complete forward interval and bounded recent edits into the archive using the workspace's Secrets-backed bot credential. Omit channel_id to visit all visible text-capable channels; partial channel failures are reported explicitly.",
        input_schema: r#"{"type":"object","properties":{"channel_id":{"type":"string"},"fetch_limit":{"type":"integer","minimum":1,"maximum":100,"default":100},"reconcile_limit":{"type":"integer","minimum":1,"maximum":100,"default":50}},"additionalProperties":false}"# },
    Tool { name: "discord_send", description: "Send literal text as the workspace's Secrets-backed Discord bot, after checking collection WRITE admission, then store the returned observation. A post-send storage failure does not undo the external send; do not retry automatically.",
        input_schema: r#"{"type":"object","properties":{"channel_id":{"type":"string"},"text":{"type":"string","description":"Literal message body. @file and @- never expand to host input."}},"required":["channel_id","text"],"additionalProperties":false}"# },
    Tool { name: "discord_channels_list", description: "List the guilds and channels visible to the workspace's Secrets-backed Discord bot through a finite REST query.",
        input_schema: r#"{"type":"object","properties":{"guild":{"type":"string","description":"Optional exact canonical Discord guild snowflake."}},"additionalProperties":false}"# },
];
fn limit() -> usize {
    20
}
fn fetch_limit() -> u32 {
    100
}
fn reconcile_limit() -> u32 {
    50
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    channel_id: Option<String>,
    since: Option<String>,
    #[serde(default = "limit")]
    limit: usize,
    #[serde(default)]
    descending: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pull {
    channel_id: Option<String>,
    #[serde(default = "fetch_limit")]
    fetch_limit: u32,
    #[serde(default = "reconcile_limit")]
    reconcile_limit: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    channel_id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Channels {
    guild: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthSet {
    secret_version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Clone, Debug)]
pub struct Discord {
    operations: super::operations::Discord,
}
impl Discord {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: super::operations::Discord::with_storage(storage),
        }
    }
}
impl Faculty for Discord {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "discord_auth_status" => {
                let _: Empty = decode_arguments(arguments)?;
                render::auth(&self.operations.auth_status()?, out)
            }
            "discord_auth_set" => {
                let args: AuthSet = decode_arguments(arguments)?;
                let secret =
                    triblespace::prelude::Id::from_hex(&args.secret_version).ok_or_else(|| {
                        invalid_arguments("expected an exact 32-character Secrets version id")
                    })?;
                let revision = self.operations.auth_set(secret)?;
                out.line(format!(
                    "Discord auth revision {revision:X}; Secrets version {secret:X}"
                ))
            }
            "discord_read" => {
                let args: Read = decode_arguments(arguments)?;
                let options = ReadOptions {
                    channel_id: args.channel_id,
                    since: args.since,
                    limit: args.limit,
                    descending: args.descending,
                };
                options.validate().map_err(invalid_arguments)?;
                render::history(&self.operations.read(options)?, out)
            }
            "discord_pull" => {
                let args: Pull = decode_arguments(arguments)?;
                let options = PullOptions {
                    channel_id: args.channel_id,
                    fetch_limit: args.fetch_limit,
                    reconcile_limit: args.reconcile_limit,
                };
                options.validate().map_err(invalid_arguments)?;
                render::pull(&self.operations.pull(options)?, out)
            }
            "discord_send" => {
                let args: Send = decode_arguments(arguments)?;
                super::validate_snowflake(&args.channel_id).map_err(invalid_arguments)?;
                if args.text.trim().is_empty() {
                    return Err(invalid_arguments("message body must not be empty"));
                }
                render::sent(&self.operations.send(&args.channel_id, &args.text)?, out)
            }
            "discord_channels_list" => {
                let args: Channels = decode_arguments(arguments)?;
                if let Some(guild) = &args.guild {
                    super::validate_snowflake(guild).map_err(invalid_arguments)?;
                }
                render::channels(&self.operations.channels_list(args.guild.as_deref())?, out)
            }
            _ => bail!("unknown Discord MCP tool {name:?}"),
        }
    }
}
