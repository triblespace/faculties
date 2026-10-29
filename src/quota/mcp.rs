//! Quota MCP tools: literal readings with an explicit host.

use super::{render, Quota as Operations};
use crate::mcp::{decode_arguments, Faculty, Tool};
use crate::out::Out;
use crate::quota::{self, Observation};
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

const TOOLS: &[Tool] = &[
    Tool {
        name: "quota_record",
        description: "Record one provider-reported reading of one rate-limit window. provider is claude or codex; account is an opaque digest (never a raw account id or email); window_secs is the window length; used is the used percentage as reported; resets_at and observed_at are unix seconds (observed_at defaults to now). host is explicit: the server's own host name is not assumed to be the observer's. bucket is optional: the provider-side allowance name (e.g. a Codex rateLimitsByLimitId key), so two allowances with the same window length stay apart; omit it when the provider has none.",
        input_schema: r#"{"type":"object","properties":{"provider":{"type":"string","enum":["claude","codex"]},"account":{"type":"string"},"window_secs":{"type":"integer","minimum":1},"used":{"type":"number","minimum":0},"resets_at":{"type":"integer"},"observed_at":{"type":"integer"},"host":{"type":"string"},"bucket":{"type":"string","minLength":1}},"required":["provider","account","window_secs","used","resets_at","host"],"additionalProperties":false}"#,
    },
    Tool {
        name: "quota_show",
        description: "Show the latest reading per provider, account, window and bucket: used %, elapsed % of the window, a pace verdict (a planning heuristic with a 5-point margin), the age of the last recorded observation (STALE after 6 h, never dropped), and the reset time in UTC and Europe/Berlin. When any were recorded, also the latest reading of each provider's own public status page (the provider's claim, not a measurement; STALE after 1 h). States how many observations it examined.",
        input_schema: r#"{"type":"object","properties":{"json":{"type":"boolean","default":false}},"additionalProperties":false}"#,
    },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordArguments {
    provider: String,
    account: String,
    window_secs: u64,
    used: f64,
    resets_at: i64,
    observed_at: Option<i64>,
    host: String,
    bucket: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowArguments {
    #[serde(default)]
    json: bool,
}

pub struct Quota {
    operations: Operations,
}

impl Quota {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }

    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: Operations::with_storage(storage),
        }
    }
}

impl Faculty for Quota {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }

    fn call(&self, name: &str, arguments: Bytes, output: &mut Out<'_>) -> Result<()> {
        match name {
            "quota_record" => {
                let args: RecordArguments = decode_arguments(arguments)?;
                let observed_at = match args.observed_at {
                    Some(seconds) => seconds,
                    None => quota::tai_ns_to_unix(crate::clock::tai_nanoseconds_now()?),
                };
                let observation = Observation {
                    provider: quota::provider_id(&args.provider)?,
                    account: args.account,
                    window_secs: args.window_secs,
                    used_percent: args.used,
                    resets_at: args.resets_at,
                    observed_at,
                    host: args.host,
                    bucket: args.bucket,
                };
                render::record(&self.operations.record(observation)?, output)
            }
            "quota_show" => {
                let args: ShowArguments = decode_arguments(arguments)?;
                let view = self.operations.show()?;
                if args.json {
                    render::show_json(&view, output)
                } else {
                    render::show(&view, output)
                }
            }
            other => bail!("Quota MCP has no tool {other:?}"),
        }
    }
}
