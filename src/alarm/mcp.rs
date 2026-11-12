//! Alarm MCP tools. Every write names who acts in an explicit, required
//! `from`: the server's PERSONA is never read, so a call cannot raise or ack
//! as whoever happens to run the server.

use std::path::PathBuf;

use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;

use super::follow;
use super::operations::{Alarm as Operations, RaiseRequest, Raised, SystemClock};
use super::render;
use super::rungs::{Executor, Live, Settings};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use crate::schemas::alarm::{VIA_CLI, VIA_RELAYED};

const TOOLS: &[Tool] = &[
    Tool {
        name: "alarm_raise",
        description: "Raise an alarm that wakes JP physically: a Discord mention and a colony message now, then sound, voice and lights until someone acks it. Only for something about to be LOST: class is data-loss, recovery-window, hung-host or security; what is one line; decays says what is lost and by when; evidence says what you measured. Before raising, name the physical thing you measured and the semantic thing you care about, and ask whether the system ever promised they would agree. from is the raising zooid (a live zooid's Relations label or id; required, never defaulted); for is the Relations person it wakes; host is the raising host. A raise matching an open or recent alarm becomes a note on it. test runs R0 only, without the JP mention; dry_run writes and sends nothing.",
        input_schema: r#"{"type":"object","properties":{"from":{"type":"string","minLength":1},"for":{"type":"string","minLength":1},"class":{"type":"string","enum":["data-loss","recovery-window","hung-host","security"]},"what":{"type":"string","minLength":1},"decays":{"type":"string","minLength":1},"evidence":{"type":"string","minLength":1},"host":{"type":"string","minLength":1},"incident":{"type":"string","minLength":1},"test":{"type":"boolean","default":false},"separate":{"type":"boolean","default":false},"dry_run":{"type":"boolean","default":false}},"required":["from","for","class","what","decays","evidence","host"],"additionalProperties":false}"#,
    },
    Tool {
        name: "alarm_ack",
        description: "Record that an alarm was seen, which closes it. id is the alarm id or a unique prefix. from is who saw it (an active Relations person; required). via is cli (default: from saw it) or relayed (from relays JP's word; evidence then says what JP said and where).",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string","minLength":1},"from":{"type":"string","minLength":1},"via":{"type":"string","enum":["cli","relayed"],"default":"cli"},"evidence":{"type":"string","minLength":1}},"required":["id","from"],"additionalProperties":false}"#,
    },
    Tool {
        name: "alarm_resolve",
        description: "Record that an alarm's cause was dealt with, which closes it. false_alarm marks it as a false alarm, which halves its raiser's daily allowance for 7 days. from is who resolves (required).",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string","minLength":1},"from":{"type":"string","minLength":1},"reason":{"type":"string","minLength":1},"false_alarm":{"type":"boolean","default":false}},"required":["id","from","reason"],"additionalProperties":false}"#,
    },
    Tool {
        name: "alarm_note",
        description: "Attach literal news to an alarm without restarting its ladder. from is who notes it (required).",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string","minLength":1},"from":{"type":"string","minLength":1},"text":{"type":"string","minLength":1}},"required":["id","from","text"],"additionalProperties":false}"#,
    },
    Tool {
        name: "alarm_list",
        description: "List alarms newest first, open or closed, with their step counts and each raiser's false alarms in the last 7 days. open keeps only the open ones.",
        input_schema: r#"{"type":"object","properties":{"open":{"type":"boolean","default":false}},"additionalProperties":false}"#,
    },
    Tool {
        name: "alarm_show",
        description: "Show one alarm: what, decays, evidence, every rung that ran and how it ended, and every ack, resolve and note.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string","minLength":1}},"required":["id"],"additionalProperties":false}"#,
    },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RaiseArguments {
    from: String,
    #[serde(rename = "for")]
    for_person: String,
    class: String,
    what: String,
    decays: String,
    evidence: String,
    host: String,
    incident: Option<String>,
    #[serde(default)]
    test: bool,
    #[serde(default)]
    separate: bool,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckArguments {
    id: String,
    from: String,
    via: Option<String>,
    evidence: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveArguments {
    id: String,
    from: String,
    reason: String,
    #[serde(default)]
    false_alarm: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteArguments {
    id: String,
    from: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArguments {
    #[serde(default)]
    open: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowArguments {
    id: String,
}

/// A non-empty `from`, checked before anything is opened.
fn from(value: &str) -> Result<&str> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_arguments("from must name who acts"));
    }
    Ok(value)
}

pub struct Alarm {
    operations: Operations,
    /// Where the rungs go, from the launcher: never read from the server's
    /// environment here.
    settings: Settings,
    /// The collection routes a follower inherits, from the launcher.
    routes: Vec<(String, String)>,
}

impl Alarm {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }

    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self::with_settings(storage, Settings::default())
    }

    pub fn with_settings(storage: crate::storage::Storage, settings: Settings) -> Self {
        Self {
            operations: Operations::with_storage(storage),
            settings,
            routes: Vec::new(),
        }
    }

    /// The routes a follower started by `alarm_raise` is given.
    pub fn with_follower_routes(mut self, routes: Vec<(String, String)>) -> Self {
        self.routes = routes;
        self
    }
}

impl Faculty for Alarm {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }

    fn call(&self, name: &str, arguments: Bytes, output: &mut Out<'_>) -> Result<()> {
        let mut clock = SystemClock;
        match name {
            "alarm_raise" => {
                let args: RaiseArguments = decode_arguments(arguments)?;
                let request = RaiseRequest {
                    raiser: from(&args.from)?,
                    for_person: &args.for_person,
                    class: &args.class,
                    host: &args.host,
                    what: &args.what,
                    decays: &args.decays,
                    evidence: &args.evidence,
                    incident: args.incident.as_deref(),
                    test: args.test,
                    separate: args.separate,
                };
                if args.dry_run {
                    return render::preview(
                        &self.operations.preview(&request, &mut clock)?,
                        output,
                    );
                }
                let mut executor = Executor::new(
                    Live::new(self.operations.storage().clone()),
                    self.settings.clone(),
                    crate::quota::local_host()?,
                );
                let raised = self.operations.raise(&request, &mut executor, &mut clock)?;
                render::raised(&raised, output)?;
                if let Raised::Alarm {
                    alarm: id, subject, ..
                } = &raised
                {
                    if !subject.test {
                        let locks = follow::runtime_dir()?;
                        match follow::start(
                            self.operations.storage(),
                            &self.settings,
                            &self.routes,
                            *id,
                            &mut *follow::default_spawner(&locks),
                        ) {
                            Ok(unit) => output.line(format!("follower started: {unit}"))?,
                            Err(error) => bail!(
                                "rung 0 sent; ladder NOT running ({error:#}); run `alarm follow \
                                 {id:x}`"
                            ),
                        }
                    }
                }
                Ok(())
            }
            "alarm_ack" => {
                let args: AckArguments = decode_arguments(arguments)?;
                let by = from(&args.from)?;
                let via = match args.via.as_deref().unwrap_or("cli") {
                    "cli" => VIA_CLI,
                    "relayed" => VIA_RELAYED,
                    other => {
                        return Err(invalid_arguments(format!(
                            "via {other:?} is not cli or relayed"
                        )))
                    }
                };
                let recorded =
                    self.operations
                        .ack(&args.id, by, via, args.evidence.as_deref(), &mut clock)?;
                render::recorded("ack", &recorded, output)
            }
            "alarm_resolve" => {
                let args: ResolveArguments = decode_arguments(arguments)?;
                let by = from(&args.from)?;
                let recorded = self.operations.resolve(
                    &args.id,
                    by,
                    &args.reason,
                    args.false_alarm,
                    &mut clock,
                )?;
                render::recorded("resolve", &recorded, output)
            }
            "alarm_note" => {
                let args: NoteArguments = decode_arguments(arguments)?;
                let by = from(&args.from)?;
                let recorded = self.operations.note(&args.id, by, &args.text, &mut clock)?;
                render::recorded("note", &recorded, output)
            }
            "alarm_list" => {
                let args: ListArguments = decode_arguments(arguments)?;
                render::list(
                    &self.operations.list(
                        args.open,
                        self.settings.jp_discord_user.as_deref(),
                        &mut clock,
                    )?,
                    output,
                )
            }
            "alarm_show" => {
                let args: ShowArguments = decode_arguments(arguments)?;
                render::show(&self.operations.show(&args.id)?, output)
            }
            other => bail!("Alarm MCP has no tool {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<crate::mcp::InvalidArguments>()
            .is_some()
    }

    #[test]
    fn alarm_mcp_writes_without_from_are_rejected_before_storage() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("never.pile");
        let adapter = Alarm::new(pile.clone(), None);
        let names: Vec<&str> = adapter.tools().iter().map(|tool| tool.name).collect();
        assert_eq!(
            names,
            [
                "alarm_raise",
                "alarm_ack",
                "alarm_resolve",
                "alarm_note",
                "alarm_list",
                "alarm_show"
            ]
        );
        for tool in adapter.tools() {
            let schema: serde_json::Value = serde_json::from_str(tool.input_schema).unwrap();
            assert_eq!(schema["additionalProperties"], false, "{}", tool.name);
            let writes = !matches!(tool.name, "alarm_list" | "alarm_show");
            let required: Vec<&str> = schema["required"]
                .as_array()
                .map(|names| names.iter().map(|name| name.as_str().unwrap()).collect())
                .unwrap_or_default();
            assert_eq!(required.contains(&"from"), writes, "{}", tool.name);
        }
        let call = |name: &str, body: &str| {
            adapter
                .call(
                    name,
                    Bytes::from(body.as_bytes().to_vec()),
                    &mut Out::new(&mut |_| Ok(())),
                )
                .unwrap_err()
        };
        let raise = r#"{"for":"jp","class":"data-loss","what":"pile truncated","decays":"snapshot at 04:00","evidence":"19 KB","host":"sky"}"#;
        assert!(invalid(&call("alarm_raise", raise)));
        let with_empty_from = raise.replacen('{', r#"{"from":"  ","#, 1);
        assert!(invalid(&call("alarm_raise", &with_empty_from)));
        let with_persona = raise.replacen('{', r#"{"from":"cc","persona":"cc","#, 1);
        assert!(invalid(&call("alarm_raise", &with_persona)));
        assert!(invalid(&call("alarm_ack", r#"{"id":"9e1a"}"#)));
        assert!(invalid(&call(
            "alarm_resolve",
            r#"{"id":"9e1a","reason":"fixed"}"#
        )));
        assert!(invalid(&call(
            "alarm_note",
            r#"{"id":"9e1a","text":"news"}"#
        )));
        assert!(invalid(&call(
            "alarm_ack",
            r#"{"id":"9e1a","from":"cc","via":"pigeon"}"#
        )));
        assert!(!pile.exists(), "a rejected call must not open the pile");
    }
}
